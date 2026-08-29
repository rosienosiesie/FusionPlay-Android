package com.airplayreceiver.desktop.backend

import com.airplayreceiver.desktop.nativebridge.FusionPlayNative
import com.airplayreceiver.desktop.nativebridge.NativeCallback
import java.io.Closeable
import java.nio.file.Files
import java.nio.file.Path
import java.time.Instant
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

enum class PlaybackCommand(val wireName: String) {
    PLAY("play"),
    PAUSE("pause"),
    PLAY_PAUSE("play_pause"),
    PREVIOUS_TRACK("previous_track"),
    NEXT_TRACK("next_track"),
}

enum class CoreLogLevel {
    INFO,
    ERROR,
}

enum class CoreLogSource {
    SERVICE,
    STDOUT,
    STDERR,
}

data class CoreProcessLog(
    val timestamp: Instant = Instant.now(),
    val level: CoreLogLevel,
    val source: CoreLogSource,
    val message: String,
)

data class CoreProcessExit(
    val exitCode: Int,
    val expected: Boolean,
)

private enum class CoreProtocol(val wireName: String) {
    AIRPLAY("airplay"),
    DLNA("dlna"),
}

private data class CoreProtocolConfiguration(
    val receiverName: String,
    val outputDeviceId: String?,
    val statePath: String,
)

class CoreProcessService(
    val executablePath: Path = defaultExecutablePath(),
    val statePath: Path = defaultStatePath(),
    private val eventParser: (String) -> AppEvent = AppEventParser::parse,
    parentScope: CoroutineScope? = null,
) : Closeable, NativeCallback {
    private val serviceJob = SupervisorJob(parentScope?.coroutineContext?.get(Job))
    private val scope = CoroutineScope(
        (parentScope?.coroutineContext ?: Dispatchers.IO) + serviceJob,
    )
    private val lifecycleMutex = Mutex()
    private val closed = AtomicBoolean(false)
    private val appliedConfigurations = mutableMapOf<CoreProtocol, CoreProtocolConfiguration>()
    private val nativeCallbacks = Channel<NativeCoreCallback>(
        capacity = MAX_PENDING_NATIVE_CALLBACKS,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    private val _events = MutableSharedFlow<AppEvent>(extraBufferCapacity = 64)
    val events: SharedFlow<AppEvent> = _events.asSharedFlow()

    private val _logs = MutableSharedFlow<CoreProcessLog>(extraBufferCapacity = 64)
    val logs: SharedFlow<CoreProcessLog> = _logs.asSharedFlow()

    private val _exits = MutableSharedFlow<CoreProcessExit>(extraBufferCapacity = 4)
    val exits: SharedFlow<CoreProcessExit> = _exits.asSharedFlow()

    private val _isRunning = MutableStateFlow(false)
    val isRunning: StateFlow<Boolean> = _isRunning.asStateFlow()

    init {
        scope.launch(Dispatchers.Default) {
            for (callback in nativeCallbacks) {
                when (callback) {
                    is NativeCoreCallback.Event -> try {
                        _events.emit(eventParser(callback.json))
                    } catch (exception: Exception) {
                        if (exception is CancellationException) throw exception
                        emitLog(
                            CoreLogLevel.INFO,
                            CoreLogSource.STDOUT,
                            "Unparsed core output: ${callback.json}",
                        )
                    }
                    is NativeCoreCallback.Log -> emitLog(
                        if (callback.isError) CoreLogLevel.ERROR else CoreLogLevel.INFO,
                        CoreLogSource.STDERR,
                        callback.message,
                    )
                }
            }
        }
        FusionPlayNative.addListener(this)
    }

    suspend fun start(
        receiverName: String,
        outputDeviceId: String? = null,
        airPlayEnabled: Boolean = true,
        dlnaEnabled: Boolean = true,
    ) {
        ensureOpen()
        val normalizedName = receiverName.trim()
        require(normalizedName.isNotEmpty()) { "receiverName must not be blank." }
        require(normalizedName.length <= AppSettings.MAX_RECEIVER_NAME_LENGTH) {
            "receiverName must not exceed ${AppSettings.MAX_RECEIVER_NAME_LENGTH} characters."
        }
        lifecycleMutex.withLock {
            ensureOpen()
            val normalizedStatePath = statePath.toAbsolutePath().normalize()
            normalizedStatePath.parent?.let(Files::createDirectories)
            val configuration = CoreProtocolConfiguration(
                receiverName = normalizedName,
                outputDeviceId = outputDeviceId?.trim()?.takeIf(String::isNotEmpty),
                statePath = normalizedStatePath.toString(),
            )
            val desired = buildSet {
                if (airPlayEnabled) add(CoreProtocol.AIRPLAY)
                if (dlnaEnabled) add(CoreProtocol.DLNA)
            }

            CoreProtocol.entries
                .filterNot(desired::contains)
                .forEach { stopProtocolLocked(it) }
            desired.forEach { protocol ->
                if (appliedConfigurations[protocol] != configuration) {
                    stopProtocolLocked(protocol)
                    FusionPlayNative.nativeStartCoreProtocol(
                        protocol.wireName,
                        configuration.receiverName,
                        configuration.statePath,
                        configuration.outputDeviceId,
                    )
                    appliedConfigurations[protocol] = configuration
                    emitLog(
                        CoreLogLevel.INFO,
                        CoreLogSource.SERVICE,
                        "${protocol.wireName} service started independently.",
                    )
                }
            }
            updateRunningState()
        }
    }

    suspend fun sendPlayback(
        command: PlaybackCommand,
        requestId: String = UUID.randomUUID().toString().replace("-", ""),
        source: String? = null,
    ): String {
        require(requestId.isNotBlank()) { "requestId must not be blank." }
        val payload = buildJsonObject {
            put("command", command.wireName)
            put("request_id", requestId)
            source?.trim()?.takeIf(String::isNotEmpty)?.let { put("source", it) }
        }
        writeNdjson(payload.toString(), targetForSource(source))
        return requestId
    }

    suspend fun sendSeek(
        positionMs: Long,
        requestId: String = UUID.randomUUID().toString().replace("-", ""),
        source: String? = null,
    ): String {
        require(positionMs >= 0) { "positionMs must not be negative." }
        require(requestId.isNotBlank()) { "requestId must not be blank." }
        val payload = buildJsonObject {
            put("command", "seek")
            put("request_id", requestId)
            put("position_ms", positionMs)
            source?.trim()?.takeIf(String::isNotEmpty)?.let { put("source", it) }
        }
        writeNdjson(payload.toString(), targetForSource(source))
        return requestId
    }

    suspend fun sendVolume(
        percent: Int,
        requestId: String = UUID.randomUUID().toString().replace("-", ""),
        source: String? = null,
    ): String {
        require(percent in 0..100) { "percent must be between 0 and 100." }
        require(requestId.isNotBlank()) { "requestId must not be blank." }
        val payload = buildJsonObject {
            put("command", "set_volume")
            put("request_id", requestId)
            put("position_ms", percent)
            source?.trim()?.takeIf(String::isNotEmpty)?.let { put("source", it) }
        }
        writeNdjson(payload.toString(), targetForSource(source))
        return requestId
    }

    suspend fun sendVideoState(
        positionMs: Long,
        durationMs: Long,
        rate: Double,
        ready: Boolean,
        source: String? = null,
    ) {
        require(positionMs >= 0) { "positionMs must not be negative." }
        require(durationMs >= 0) { "durationMs must not be negative." }
        require(rate.isFinite() && rate >= 0) { "rate must be finite and non-negative." }
        val payload = buildJsonObject {
            put("command", "video_state")
            put("position_ms", positionMs)
            put("duration_ms", durationMs)
            put("rate", rate)
            put("ready", ready)
            source?.trim()?.takeIf(String::isNotEmpty)?.let { put("source", it) }
        }
        writeNdjson(payload.toString(), targetForSource(source))
    }

    suspend fun sendSourceTakeover(
        source: String,
        mediaKind: String? = null,
    ) {
        val normalizedSource = source
            .trim()
            .lowercase()
            .takeIf(String::isNotEmpty)
            ?: throw IllegalArgumentException("source must not be blank.")
        val payload = buildJsonObject {
            put("command", "takeover")
            put("source", normalizedSource)
            mediaKind
                ?.trim()
                ?.lowercase()
                ?.takeIf(String::isNotEmpty)
                ?.let { put("media_kind", it) }
        }
        // Both native protocol runtimes share one playback arbiter, so a
        // single command is enough to suspend whichever core source is active.
        writeNdjson(payload.toString(), "auto")
    }

    suspend fun shutdown() {
        lifecycleMutex.withLock {
            if (appliedConfigurations.isEmpty()) {
                return
            }
            CoreProtocol.entries.forEach { stopProtocolLocked(it) }
            updateRunningState()
            _exits.emit(CoreProcessExit(exitCode = 0, expected = true))
        }
    }

    suspend fun closeAsync() {
        if (!closed.compareAndSet(false, true)) {
            return
        }
        try {
            shutdown()
        } finally {
            FusionPlayNative.removeListener(this)
            nativeCallbacks.close()
            serviceJob.cancel()
            scope.cancel()
        }
    }

    override fun close() {
        runBlocking { closeAsync() }
    }

    override fun onCoreEvent(json: String) {
        nativeCallbacks.trySend(NativeCoreCallback.Event(json))
    }

    override fun onXiaomiEvent(json: String) = Unit

    override fun onNativeLog(message: String, isError: Boolean) {
        nativeCallbacks.trySend(NativeCoreCallback.Log(message, isError))
    }

    private suspend fun writeNdjson(line: String, target: String = "auto") {
        ensureOpen()
        if (!FusionPlayNative.nativeSendCoreCommand(target, line)) {
            throw IllegalStateException("The requested receiver service is not running.")
        }
    }

    private suspend fun stopProtocolLocked(protocol: CoreProtocol) {
        if (appliedConfigurations.remove(protocol) == null) {
            return
        }
        FusionPlayNative.nativeStopCoreProtocol(protocol.wireName)
        emitLog(
            CoreLogLevel.INFO,
            CoreLogSource.SERVICE,
            "${protocol.wireName} service stopped independently.",
        )
    }

    private fun updateRunningState() {
        _isRunning.value = appliedConfigurations.isNotEmpty()
    }

    private fun targetForSource(source: String?): String = when (
        source?.trim()?.lowercase()
    ) {
        "airplay" -> CoreProtocol.AIRPLAY.wireName
        "dlna" -> CoreProtocol.DLNA.wireName
        else -> "auto"
    }

    private suspend fun emitLog(
        level: CoreLogLevel,
        source: CoreLogSource,
        message: String,
    ) {
        _logs.emit(
            CoreProcessLog(
                level = level,
                source = source,
                message = message,
            ),
        )
    }

    private fun ensureOpen() {
        check(!closed.get()) { "CoreProcessService is closed." }
    }

    companion object {
        private const val MAX_PENDING_NATIVE_CALLBACKS = 256

        const val EXECUTABLE_OVERRIDE_PROPERTY = "airplayreceiver.core.path"

        fun defaultStatePath(): Path = AndroidPaths.coreStatePath()

        fun defaultExecutablePath(): Path = AndroidPaths.stateDirectory()
    }

    private sealed interface NativeCoreCallback {
        data class Event(val json: String) : NativeCoreCallback

        data class Log(val message: String, val isError: Boolean) : NativeCoreCallback
    }
}

internal fun buildCoreProcessCommand(
    executablePath: Path,
    receiverName: String,
    statePath: Path,
    outputDeviceId: String? = null,
): List<String> = buildList {
    add(executablePath.toString())
    add("--name")
    add(receiverName)
    add("--transient")
    add("--state")
    add(statePath.toString())
    outputDeviceId
        ?.trim()
        ?.takeIf(String::isNotEmpty)
        ?.let {
            add("--output-device")
            add(it)
        }
}
