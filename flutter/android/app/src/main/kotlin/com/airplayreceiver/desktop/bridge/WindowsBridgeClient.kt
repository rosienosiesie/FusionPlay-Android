package com.airplayreceiver.desktop.bridge

import com.airplayreceiver.desktop.backend.FusionPlayRunIdentity
import com.airplayreceiver.desktop.backend.XiaomiNetworkAdapterState
import com.fusionplay.android.media.FusionPlayMediaChannel
import java.io.BufferedWriter
import java.io.Closeable
import java.io.IOException
import java.net.URI
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.Path
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.joinAll
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.doubleOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.put

sealed interface WindowsBridgeEvent {
    val rawJson: String
}

data class WindowsBridgeReady(
    val protocolVersion: Int,
    val processId: Long?,
    val windowHandle: String?,
    override val rawJson: String,
) : WindowsBridgeEvent

data class WindowsBridgeResponseError(
    val code: String?,
    val message: String?,
    val hresult: String?,
)

data class WindowsBridgeResponse(
    val requestId: String?,
    val command: String?,
    val succeeded: Boolean,
    val result: JsonElement?,
    val error: WindowsBridgeResponseError?,
    override val rawJson: String,
) : WindowsBridgeEvent

data class XiaomiPauseResult(
    val paused: Boolean,
    val connectionPreserved: Boolean,
    val method: String?,
    val message: String,
)

data class XiaomiControlResult(
    val succeeded: Boolean,
    val dispatched: Boolean,
    val confirmed: Boolean,
    val action: String,
    val positionMs: Long?,
    val connectionPreserved: Boolean,
    val method: String?,
    val message: String,
) {
    val accepted: Boolean
        // A successful pipe/stdin write only proves that the reverse-control
        // frame left FusionPlay. The source phone remains the sole authority:
        // callers may update playback state or progress only after HyperOS
        // reports the requested effect back through the MiPlay session.
        get() = confirmed
}

data class XiaomiNetworkAdapterList(
    val adapters: List<XiaomiNetworkAdapterState>,
    val autoSelectedAdapterId: String?,
)

class WindowsBridgeProtocolException(
    message: String,
    cause: Throwable? = null,
) : IOException(message, cause)

data class WindowsBridgeSmtcCommand(
    val command: String,
    val positionMs: Long?,
    override val rawJson: String,
) : WindowsBridgeEvent

data class WindowsBridgeXiaomiEvent(
    val eventName: String,
    val payload: JsonElement?,
    val bridgeSequence: Long? = null,
    val sessionSequence: Long? = null,
    override val rawJson: String,
) : WindowsBridgeEvent

data class WindowsBridgeXiaomiLog(
    val message: String,
    val isError: Boolean,
    override val rawJson: String,
) : WindowsBridgeEvent

data class WindowsBridgeXiaomiExit(
    val exitCode: Int?,
    override val rawJson: String,
) : WindowsBridgeEvent

data class WindowsBridgeError(
    val code: String?,
    val message: String,
    val hresult: String?,
    override val rawJson: String,
) : WindowsBridgeEvent

data class WindowsBridgeProcessExit(
    val exitCode: Int,
    val expected: Boolean,
    override val rawJson: String = "",
) : WindowsBridgeEvent

data class WindowsBridgeDiagnostic(
    val source: String,
    val message: String,
    override val rawJson: String,
) : WindowsBridgeEvent

data class WindowsBridgeUnknownEvent(
    val type: String,
    override val rawJson: String,
) : WindowsBridgeEvent

class WindowsBridgeCommandException(
    val response: WindowsBridgeResponse,
) : IOException(
    buildString {
        append("Windows bridge command")
        response.command?.let {
            append(" '")
            append(it)
            append('\'')
        }
        append(" failed")
        response.error?.code?.let {
            append(" [")
            append(it)
            append(']')
        }
        response.error?.hresult?.let {
            append(" (")
            append(it)
            append(')')
        }
        response.error?.message?.takeIf(String::isNotBlank)?.let {
            append(": ")
            append(it)
        }
        append('.')
    },
)

/**
 * Owns AirPlayReceiver.WindowsBridge.exe and its UTF-8 NDJSON transport.
 *
 * [initialize] starts the sidecar on first use, waits for `bridge_ready`, and
 * initializes SMTC. All command responses and asynchronous native events are
 * also published through [events].
 */

class WindowsBridgeClient(
    parentScope: CoroutineScope? = null,
    private val stopXiaomiOnClose: Boolean = true,
) : Closeable, com.airplayreceiver.desktop.nativebridge.NativeCallback {
    private val serviceJob =
        SupervisorJob(parentScope?.coroutineContext?.get(Job))
    private val scope = CoroutineScope(
        (parentScope?.coroutineContext ?: Dispatchers.IO) + serviceJob,
    )
    private val lifecycleMutex = Mutex()
    private val closed = AtomicBoolean(false)
    private val json = Json { ignoreUnknownKeys = true }
    private val nativeCallbacks = Channel<NativeCallbackMessage>(
        capacity = MAX_PENDING_NATIVE_CALLBACKS,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    private val _events =
        MutableSharedFlow<WindowsBridgeEvent>(extraBufferCapacity = 128)
    val events: SharedFlow<WindowsBridgeEvent> = _events.asSharedFlow()

    private val _isRunning = MutableStateFlow(false)
    val isRunning: StateFlow<Boolean> = _isRunning.asStateFlow()

    private val _ready = MutableStateFlow<WindowsBridgeReady?>(null)
    val ready: StateFlow<WindowsBridgeReady?> = _ready.asStateFlow()

    init {
        scope.launch(Dispatchers.Default) {
            for (callback in nativeCallbacks) {
                val event = when (callback) {
                    is NativeCallbackMessage.XiaomiEvent ->
                        runCatching { parseEvent(callback.json) }.getOrNull()
                    is NativeCallbackMessage.Log -> WindowsBridgeXiaomiLog(
                        message = callback.message,
                        isError = callback.isError,
                        rawJson = "{}",
                    )
                }
                if (event != null) _events.emit(event)
            }
        }
        com.airplayreceiver.desktop.nativebridge.FusionPlayNative.addListener(this)
    }

    suspend fun start(): WindowsBridgeReady = initialize().let {
        _ready.value ?: WindowsBridgeReady(
            protocolVersion = 2,
            processId = android.os.Process.myPid().toLong(),
            windowHandle = null,
            rawJson = "{}",
        )
    }

    suspend fun initialize(): WindowsBridgeResponse {
        val ready = WindowsBridgeReady(
            protocolVersion = 2,
            processId = android.os.Process.myPid().toLong(),
            windowHandle = null,
            rawJson = """{"type":"bridge_ready","protocol_version":2}""",
        )
        _ready.value = ready
        _isRunning.value = true
        _events.emit(ready)
        return ok("initialize")
    }

    suspend fun setMetadata(
        title: String? = null,
        artist: String? = null,
        album: String? = null,
        mediaIdentity: String? = null,
    ): WindowsBridgeResponse {
        FusionPlayMediaChannel.setMetadata(
            title = title,
            artist = artist,
            album = album,
            mediaIdentity = mediaIdentity,
        )
        return ok("set_metadata")
    }

    suspend fun setArtwork(path: Path): WindowsBridgeResponse {
        FusionPlayMediaChannel.setArtwork(path)
        return ok("set_artwork")
    }

    suspend fun setArtwork(uri: URI): WindowsBridgeResponse {
        FusionPlayMediaChannel.setArtwork(uri)
        return ok("set_artwork")
    }

    suspend fun setArtworkDataUri(dataUri: String): WindowsBridgeResponse {
        FusionPlayMediaChannel.setArtworkDataUri(dataUri)
        return ok("set_artwork")
    }

    suspend fun clearArtwork(): WindowsBridgeResponse {
        FusionPlayMediaChannel.clearArtwork()
        return ok("clear_artwork")
    }

    suspend fun reportDiagnostic(
        source: String? = null,
        message: String? = null,
        component: String? = null,
        event: String? = null,
        outcome: String? = null,
        reason: String? = null,
        bridgeSequence: Long? = null,
        sessionSequence: Long? = null,
    ): WindowsBridgeResponse = ok("report_diagnostic")

    suspend fun setPlayback(playing: Boolean): WindowsBridgeResponse {
        FusionPlayMediaChannel.setPlayback(playing)
        return ok("set_playback")
    }

    suspend fun setTimeline(
        positionMs: Long,
        durationMs: Long,
    ): WindowsBridgeResponse {
        FusionPlayMediaChannel.setTimeline(positionMs, durationMs)
        return ok("set_timeline")
    }

    suspend fun setCapabilities(
        canPlayPause: Boolean,
        canPrevious: Boolean,
        canNext: Boolean,
        canSeek: Boolean,
    ): WindowsBridgeResponse {
        FusionPlayMediaChannel.setCapabilities(
            canPlayPause = canPlayPause,
            canPrevious = canPrevious,
            canNext = canNext,
            canSeek = canSeek,
        )
        return ok("set_capabilities")
    }

    suspend fun clear(): WindowsBridgeResponse {
        FusionPlayMediaChannel.clear()
        return ok("clear")
    }

    suspend fun listXiaomiNetworkAdapters(): XiaomiNetworkAdapterList {
        val raw = com.airplayreceiver.desktop.nativebridge.FusionPlayNative
            .nativeListNetworkAdapters()
        val parsed = json.parseToJsonElement(raw)
        return parseXiaomiNetworkAdapterListResult(parsed)
    }

    suspend fun startXiaomi(
        receiverName: String,
        networkAdapterId: String? = null,
        outputDeviceId: String? = null,
        deviceType: Int = 2,
    ): WindowsBridgeResponse {
        val adapters = listXiaomiNetworkAdapters()
        val resolvedAdapterId = resolveXiaomiStartNetworkAdapterId(
            requestedAdapterId = networkAdapterId,
            adapterList = adapters,
        ) ?: throw XiaomiMiPlayClientException(
            code = "xiaomi_miplay_physical_adapter_required",
            message =
                "No connected physical Ethernet or Wi-Fi adapter is available " +
                    "for Xiaomi MiPlay.",
        )
        val adapter = adapters.adapters.first {
            it.id.equals(resolvedAdapterId, ignoreCase = true)
        }
        val ipv4 = adapter.ipv4Address
            ?: throw XiaomiMiPlayClientException(
                code = "selected_adapter_unavailable",
                message = "Adapter '${adapter.name}' has no IPv4 address.",
            )
        com.airplayreceiver.desktop.nativebridge.FusionPlayNative.nativeStartMiPlay(
            receiverName.trim(),
            ipv4,
            adapter.name,
            adapter.macAddress,
            com.airplayreceiver.desktop.backend.AndroidPaths.identityDirectory().toString(),
            outputDeviceId,
            FusionPlayMediaChannel.currentSystemMediaVolumePercent(),
            deviceType,
        )
        return ok("xiaomi_start")
    }

    suspend fun stopXiaomi(): WindowsBridgeResponse {
        com.airplayreceiver.desktop.nativebridge.FusionPlayNative.nativeStopMiPlay()
        return ok("xiaomi_stop")
    }

    suspend fun pauseXiaomi(): XiaomiPauseResult {
        val result = controlXiaomi("pause")
        return XiaomiPauseResult(
            paused = result.accepted || result.dispatched,
            connectionPreserved = result.connectionPreserved,
            method = result.method,
            message = result.message,
        )
    }

    suspend fun suspendXiaomiOutput(): WindowsBridgeResponse {
        com.airplayreceiver.desktop.nativebridge.FusionPlayNative
            .nativeSuspendMiPlayOutput()
        return ok("xiaomi_suspend_output")
    }

    suspend fun resumeXiaomiOutput(): WindowsBridgeResponse {
        com.airplayreceiver.desktop.nativebridge.FusionPlayNative
            .nativeResumeMiPlayOutput()
        return ok("xiaomi_resume_output")
    }

    suspend fun setXiaomiVolume(percent: Int): Boolean =
        com.airplayreceiver.desktop.nativebridge.FusionPlayNative
            .nativeSetMiPlayVolume(percent.coerceIn(0, 100))

    suspend fun controlXiaomi(
        action: String,
        positionMs: Long? = null,
    ): XiaomiControlResult {
        val raw = com.airplayreceiver.desktop.nativebridge.FusionPlayNative
            .nativeControlMiPlay(action, positionMs ?: -1L)
        val payload = json.parseToJsonElement(raw) as JsonObject
        return XiaomiControlResult(
            succeeded = payload["succeeded"]?.jsonPrimitive?.booleanOrNull == true,
            dispatched = payload["dispatched"]?.jsonPrimitive?.booleanOrNull == true,
            confirmed = payload["confirmed"]?.jsonPrimitive?.booleanOrNull == true,
            action = payload["action"]?.jsonPrimitive?.contentOrNull ?: action,
            positionMs = payload["position_ms"]?.jsonPrimitive?.longOrNull,
            connectionPreserved =
                payload["connection_preserved"]?.jsonPrimitive?.booleanOrNull ?: true,
            method = payload["method"]?.jsonPrimitive?.contentOrNull,
            message = payload["message"]?.jsonPrimitive?.contentOrNull.orEmpty(),
        )
    }

    suspend fun shutdown() {
        closeAsync()
    }

    suspend fun closeAsync() {
        if (!closed.compareAndSet(false, true)) {
            return
        }
        if (stopXiaomiOnClose) {
            runCatching { stopXiaomi() }
        }
        FusionPlayMediaChannel.clear()
        com.airplayreceiver.desktop.nativebridge.FusionPlayNative.removeListener(this)
        nativeCallbacks.close()
        serviceJob.cancel()
        scope.cancel()
        _isRunning.value = false
    }

    val shuttingDown: Boolean
        get() = closed.get()

    override fun close() {
        runBlocking { closeAsync() }
    }

    override fun onCoreEvent(json: String) = Unit

    override fun onXiaomiEvent(json: String) {
        nativeCallbacks.trySend(NativeCallbackMessage.XiaomiEvent(json))
    }

    override fun onNativeLog(message: String, isError: Boolean) {
        nativeCallbacks.trySend(NativeCallbackMessage.Log(message, isError))
    }

    private fun ok(command: String): WindowsBridgeResponse =
        WindowsBridgeResponse(
            requestId = UUID.randomUUID().toString().replace("-", ""),
            command = command,
            succeeded = true,
            result = null,
            error = null,
            rawJson = "{}",
        )

    companion object {
        private const val MAX_PENDING_NATIVE_CALLBACKS = 128

        private val EMPTY_JSON_OBJECT = JsonObject(emptyMap())
        private val json = Json { ignoreUnknownKeys = true }

        internal fun buildXiaomiStartRequestBody(
            receiverName: String,
            networkAdapterId: String?,
            outputDeviceId: String? = null,
        ): JsonObject {
            val normalizedName = receiverName.trim()
            require(normalizedName.isNotEmpty()) {
                "receiverName must not be blank."
            }
            val normalizedAdapterId = networkAdapterId?.trim()
            val normalizedOutputDeviceId = outputDeviceId
                ?.trim()
                ?.takeIf(String::isNotEmpty)
            require(networkAdapterId == null || !normalizedAdapterId.isNullOrEmpty()) {
                "networkAdapterId must not be blank."
            }
            return buildJsonObject {
                put("receiver_name", normalizedName)
                normalizedAdapterId?.let {
                    put("network_adapter_id", it)
                }
                normalizedOutputDeviceId?.let {
                    put("output_device_id", it)
                }
            }
        }

        internal fun resolveXiaomiStartNetworkAdapterId(
            requestedAdapterId: String?,
            adapterList: XiaomiNetworkAdapterList,
        ): String? {
            val requested = requestedAdapterId?.trim()?.takeIf(String::isNotEmpty)
            if (requested != null) {
                val adapter = adapterList.adapters.firstOrNull {
                    it.id.equals(requested, ignoreCase = true)
                } ?: return requested
                if (!adapter.supportsMiPlayTransport) {
                    throw XiaomiMiPlayClientException(
                        code = "xiaomi_miplay_physical_adapter_required",
                        message =
                            "Adapter '${adapter.name}' is not a physical Ethernet " +
                                "or Wi-Fi adapter accepted by Xiaomi MiPlay.",
                    )
                }
                if (!adapter.manualEligible) {
                    throw XiaomiMiPlayClientException(
                        code = "selected_adapter_unavailable",
                        message =
                            "Adapter '${adapter.name}' is not currently available " +
                                "for Xiaomi MiPlay.",
                    )
                }
                return requested
            }

            return selectPreferredXiaomiNetworkAdapterId(adapterList.adapters)
        }

        /**
         * Selects only real physical transports. Physical Ethernet is always
         * preferred over Wi-Fi, with the default route breaking ties inside
         * each transport class. Virtual Ethernet, VPN and tunnel interfaces
         * never become eligible merely because the bridge marks them active.
         */
        internal fun selectPreferredXiaomiNetworkAdapterId(
            adapters: List<XiaomiNetworkAdapterState>,
        ): String? = adapters
            .asSequence()
            .filter { it.miPlayEligible && it.autoEligible }
            .sortedWith(
                compareBy<XiaomiNetworkAdapterState>(
                    { automaticXiaomiAdapterPriority(it) },
                    { it.interfaceIndex },
                    { it.id.lowercase() },
                ),
            )
            .firstOrNull()
            ?.id

        private fun automaticXiaomiAdapterPriority(
            adapter: XiaomiNetworkAdapterState,
        ): Int = when {
            adapter.isPhysicalEthernet && adapter.isDefaultRoute -> 0
            adapter.isPhysicalEthernet -> 1
            adapter.isDefaultRoute -> 2
            else -> 3
        }

        internal fun parseXiaomiNetworkAdapterListResult(
            result: JsonElement?,
        ): XiaomiNetworkAdapterList {
            val payload = result as? JsonObject
                ?: throw WindowsBridgeProtocolException(
                    "xiaomi_list_network_adapters result must be an object.",
                )
            val autoSelectedAdapterId = payload.optionalNonBlankString(
                name = "auto_selected_adapter_id",
                path = "result",
            )
            val rawAdapters = payload["adapters"] as? JsonArray
                ?: throw WindowsBridgeProtocolException(
                    "result.adapters must be an array.",
                )
            val seenAdapterIds = mutableSetOf<String>()
            val adapters = rawAdapters.mapIndexed { index, element ->
                val path = "result.adapters[$index]"
                val adapter = element as? JsonObject
                    ?: throw WindowsBridgeProtocolException(
                        "$path must be an object.",
                    )
                val id = adapter.requiredString(
                    name = "id",
                    path = path,
                    allowBlank = false,
                )
                if (seenAdapterIds.any { it.equals(id, ignoreCase = true) }) {
                    throw WindowsBridgeProtocolException(
                        "$path.id duplicates another adapter ID.",
                    )
                }
                seenAdapterIds += id

                XiaomiNetworkAdapterState(
                    id = id,
                    name = adapter.requiredString(
                        name = "name",
                        path = path,
                        allowBlank = false,
                    ),
                    description = adapter.requiredString(
                        name = "description",
                        path = path,
                        allowBlank = true,
                    ),
                    interfaceType = adapter.requiredString(
                        name = "interface_type",
                        path = path,
                        allowBlank = false,
                    ),
                    interfaceIndex = adapter.requiredNonNegativeInt(
                        name = "interface_index",
                        path = path,
                    ),
                    ipv4Address = adapter.optionalString(
                        name = "ipv4_address",
                        path = path,
                    ),
                    macAddress = adapter.optionalString(
                        name = "mac_address",
                        path = path,
                    ),
                    isUp = adapter.requiredBoolean("is_up", path),
                    classification = adapter.requiredString(
                        name = "classification",
                        path = path,
                        allowBlank = false,
                    ),
                    autoEligible = adapter.requiredBoolean(
                        name = "auto_eligible",
                        path = path,
                    ),
                    manualEligible = adapter.requiredBoolean(
                        name = "manual_eligible",
                        path = path,
                    ),
                    isDefaultRoute = adapter.requiredBoolean(
                        name = "is_default_route",
                        path = path,
                    ),
                    warning = adapter.optionalString("warning", path),
                )
            }

            if (autoSelectedAdapterId != null &&
                adapters.none { it.id.equals(autoSelectedAdapterId, ignoreCase = true) }
            ) {
                throw WindowsBridgeProtocolException(
                    "result.auto_selected_adapter_id does not identify a returned adapter.",
                )
            }
            val preferredAdapterId =
                selectPreferredXiaomiNetworkAdapterId(adapters)
            return XiaomiNetworkAdapterList(
                adapters = adapters,
                autoSelectedAdapterId = preferredAdapterId,
            )
        }

        internal fun parseEvent(line: String): WindowsBridgeEvent {
            val raw = line.trim()
            if (raw.isEmpty()) {
                throw WindowsBridgeProtocolException(
                    "Bridge event line is empty.",
                )
            }
            val payload = try {
                json.parseToJsonElement(raw) as? JsonObject
                    ?: throw WindowsBridgeProtocolException(
                        "Bridge event root must be a JSON object.",
                    )
            } catch (exception: SerializationException) {
                throw WindowsBridgeProtocolException(
                    "Bridge event is not valid JSON.",
                    exception,
                )
            }

            val type = payload.requiredString(
                name = "type",
                path = "event",
                allowBlank = false,
            )
                .lowercase()

            return when (type) {
                "bridge_ready" -> WindowsBridgeReady(
                    protocolVersion = payload.int("protocol_version") ?: 0,
                    processId = payload.long("process_id"),
                    windowHandle = payload.string("window_handle"),
                    rawJson = raw,
                )

                "response" -> parseResponse(payload, raw)

                "smtc_command" -> WindowsBridgeSmtcCommand(
                    command = payload.string("command").orEmpty(),
                    positionMs = payload.long("position_ms"),
                    rawJson = raw,
                )

                "xiaomi_event" -> WindowsBridgeXiaomiEvent(
                    eventName = payload.string("event").orEmpty(),
                    payload = payload["payload"].unlessJsonNull(),
                    bridgeSequence = payload.long("bridge_seq"),
                    sessionSequence = payload.long("session_seq"),
                    rawJson = raw,
                )

                "xiaomi_log" -> WindowsBridgeXiaomiLog(
                    message = payload.string("message").orEmpty(),
                    isError = payload.boolean("is_error") == true,
                    rawJson = raw,
                )

                "xiaomi_exit" -> WindowsBridgeXiaomiExit(
                    exitCode = payload.int("exit_code"),
                    rawJson = raw,
                )

                "bridge_error" -> WindowsBridgeError(
                    code = payload.string("code"),
                    message = payload.string("message").orEmpty(),
                    hresult = payload.string("hresult"),
                    rawJson = raw,
                )

                else -> WindowsBridgeUnknownEvent(
                    type = type,
                    rawJson = raw,
                )
            }
        }

        internal fun dispatchResponseProtocolFailure(
            line: String,
            failure: WindowsBridgeProtocolException,
            complete: (String, WindowsBridgeProtocolException) -> Boolean,
        ): Boolean {
            val requestId = extractResponseRequestId(line) ?: return false
            return complete(requestId, failure)
        }

        internal fun extractResponseRequestId(line: String): String? {
            val payload = runCatching {
                json.parseToJsonElement(line.trim()) as? JsonObject
            }.getOrNull() ?: return null
            val type = (payload["type"] as? JsonPrimitive)
                ?.takeIf(JsonPrimitive::isString)
                ?.content
                ?.trim()
            if (!type.equals("response", ignoreCase = true)) {
                return null
            }
            return (payload["request_id"] as? JsonPrimitive)
                ?.takeIf(JsonPrimitive::isString)
                ?.content
                ?.trim()
                ?.takeIf(String::isNotEmpty)
        }

        private fun parseResponse(
            payload: JsonObject,
            raw: String,
        ): WindowsBridgeResponse {
            val requestId = payload.requiredString(
                name = "request_id",
                path = "response",
                allowBlank = false,
            )
            val command = payload.requiredString(
                name = "command",
                path = "response",
                allowBlank = false,
            )
            val succeeded = payload.requiredBoolean(
                name = "ok",
                path = "response",
            )
            val errorElement = payload["error"]
            val error = when (errorElement) {
                null, JsonNull -> null
                is JsonObject -> WindowsBridgeResponseError(
                    code = errorElement.optionalString(
                        name = "code",
                        path = "response.error",
                    ),
                    message = errorElement.optionalString(
                        name = "message",
                        path = "response.error",
                    ),
                    hresult = errorElement.optionalString(
                        name = "hresult",
                        path = "response.error",
                    ),
                )

                else -> throw WindowsBridgeProtocolException(
                    "response.error must be an object or null.",
                )
            }
            if (!succeeded && error == null) {
                throw WindowsBridgeProtocolException(
                    "response.error must be an object when response.ok is false.",
                )
            }
            return WindowsBridgeResponse(
                requestId = requestId,
                command = command,
                succeeded = succeeded,
                result = payload["result"].unlessJsonNull(),
                error = error,
                rawJson = raw,
            )
        }

        private fun JsonObject.string(name: String): String? =
            (this[name] as? JsonPrimitive)?.contentOrNull

        private fun JsonObject.long(name: String): Long? {
            val value = this[name] as? JsonPrimitive ?: return null
            return value.longOrNull
                ?: value.doubleOrNull?.toLong()
                ?: value.contentOrNull?.toLongOrNull()
        }

        private fun JsonObject.int(name: String): Int? {
            val value = this[name] as? JsonPrimitive ?: return null
            return value.intOrNull
                ?: value.longOrNull?.toInt()
                ?: value.contentOrNull?.toIntOrNull()
        }

        private fun JsonObject.boolean(name: String): Boolean? {
            val value = this[name] as? JsonPrimitive ?: return null
            return value.booleanOrNull
                ?: value.contentOrNull?.toBooleanStrictOrNull()
        }

        private fun JsonElement?.unlessJsonNull(): JsonElement? =
            this?.takeUnless { it === JsonNull }

        private fun JsonObject.requiredString(
            name: String,
            path: String,
            allowBlank: Boolean,
        ): String {
            val value = this[name] as? JsonPrimitive
                ?: throw WindowsBridgeProtocolException(
                    "$path.$name must be a string.",
                )
            if (!value.isString) {
                throw WindowsBridgeProtocolException(
                    "$path.$name must be a string.",
                )
            }
            val normalized = value.content.trim()
            if (!allowBlank && normalized.isEmpty()) {
                throw WindowsBridgeProtocolException(
                    "$path.$name must not be blank.",
                )
            }
            return normalized
        }

        private fun JsonObject.optionalString(
            name: String,
            path: String,
        ): String? {
            val element = this[name] ?: return null
            if (element is JsonNull) {
                return null
            }
            val value = element as? JsonPrimitive
                ?: throw WindowsBridgeProtocolException(
                    "$path.$name must be a string or null.",
                )
            if (!value.isString) {
                throw WindowsBridgeProtocolException(
                    "$path.$name must be a string or null.",
                )
            }
            return value.content.trim().takeIf(String::isNotEmpty)
        }

        private fun JsonObject.optionalNonBlankString(
            name: String,
            path: String,
        ): String? {
            val element = this[name] ?: return null
            if (element is JsonNull) {
                return null
            }
            return requiredString(
                name = name,
                path = path,
                allowBlank = false,
            )
        }

        private fun JsonObject.requiredNonNegativeInt(
            name: String,
            path: String,
        ): Int {
            val value = this[name] as? JsonPrimitive
                ?: throw WindowsBridgeProtocolException(
                    "$path.$name must be a non-negative integer.",
                )
            val parsed = if (value.isString) null else value.intOrNull
            if (parsed == null || parsed < 0) {
                throw WindowsBridgeProtocolException(
                    "$path.$name must be a non-negative integer.",
                )
            }
            return parsed
        }

        private fun JsonObject.requiredBoolean(
            name: String,
            path: String,
        ): Boolean {
            val value = this[name] as? JsonPrimitive
                ?: throw WindowsBridgeProtocolException(
                    "$path.$name must be a boolean.",
                )
            if (value.isString) {
                throw WindowsBridgeProtocolException(
                    "$path.$name must be a boolean.",
                )
            }
            return value.booleanOrNull
                ?: throw WindowsBridgeProtocolException(
                    "$path.$name must be a boolean.",
                )
        }

    }

    private sealed interface NativeCallbackMessage {
        data class XiaomiEvent(val json: String) : NativeCallbackMessage

        data class Log(val message: String, val isError: Boolean) : NativeCallbackMessage
    }
}
