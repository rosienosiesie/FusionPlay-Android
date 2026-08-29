package com.airplayreceiver.desktop.backend

import java.io.Closeable
import java.util.concurrent.atomic.AtomicBoolean
import com.airplayreceiver.desktop.bridge.XiaomiNetworkAdapterList
import com.airplayreceiver.desktop.bridge.toXiaomiMiPlayUserMessage
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

internal const val XIAOMI_STARTUP_WARNING_ENV =
    "FUSIONPLAY_XIAOMI_STARTUP_WARNING"

enum class ReceiverProtocol {
    MIPLAY,
    AIRPLAY,
    DLNA,
}

data class ReceiverSettingsChange(
    val outputDeviceChanged: Boolean,
    val xiaomiNetworkAdapterChanged: Boolean,
) {
    val changed: Boolean
        get() = outputDeviceChanged || xiaomiNetworkAdapterChanged
}

internal fun receiverSettingsChange(
    current: AppSettings,
    outputDeviceId: String?,
    xiaomiNetworkAdapterId: String?,
): ReceiverSettingsChange {
    fun normalized(value: String?): String? =
        value?.trim()?.takeIf(String::isNotEmpty)

    return ReceiverSettingsChange(
        outputDeviceChanged = !current.outputDeviceId.equals(
            normalized(outputDeviceId),
            ignoreCase = true,
        ),
        xiaomiNetworkAdapterChanged =
            !current.xiaomiNetworkAdapterId.equals(
                normalized(xiaomiNetworkAdapterId),
                ignoreCase = true,
            ),
    )
}

/**
 * UI-agnostic state holder. Presentation bridges can collect [state] without
 * depending on Android lifecycle classes.
 */
class AppViewModel(
    private val settingsStore: SettingsStore = SettingsStore(),
    private val startupManager: WindowsStartupManager =
        WindowsStartupManager(settingsStore),
    private val startupInitializer: suspend () -> StartupInitialization =
        startupManager::initialize,
    private val coreProcessService: CoreProcessService = CoreProcessService(),
    private val lyricsResolver: OfflineLyricsResolver = OfflineLyricsResolver(),
    private val diagnosticLogger: FusionPlayDiagnosticLogger =
        FusionPlayDiagnosticLogger(),
    private val startupWarningProvider: () -> String? = {
        System.getenv(XIAOMI_STARTUP_WARNING_ENV)
    },
    receiverNameProvider: () -> String = WindowsComputerName::current,
    parentScope: CoroutineScope? = null,
) : Closeable {
    private val defaultReceiverName: String = receiverNameProvider()
        .trim()
        .take(AppSettings.MAX_RECEIVER_NAME_LENGTH)
        .also {
            require(it.isNotEmpty()) {
                "Windows computer name must not be blank."
            }
        }
    val receiverName: String
        get() = _state.value.settings.receiverName ?: defaultReceiverName
    private val viewModelJob = SupervisorJob(parentScope?.coroutineContext?.get(Job))
    private val scope = CoroutineScope(
        (parentScope?.coroutineContext ?: Dispatchers.Default) + viewModelJob,
    )
    private val actionMutex = Mutex()
    private val closed = AtomicBoolean(false)
    private val initializationComplete = CompletableDeferred<Unit>()
    private val dlnaMediaProbe = DlnaMediaProbe()
    private val xiaomiNetworkAdapterRefreshMutex = Mutex()
    private var lyricsLoadJob: Job? = null
    private var dlnaQualityProbeJob: Job? = null

    private val _state = MutableStateFlow(AppState())
    val state: StateFlow<AppState> = _state.asStateFlow()
    private val eventChannel = Channel<AppEvent>(capacity = Channel.BUFFERED)
    val events: Flow<AppEvent> = eventChannel.receiveAsFlow()

    init {
        diagnosticLogger.write(
            component = "application",
            event = "process",
            outcome = "started",
            details = mapOf(
                "receiver_name_present" to receiverName.isNotBlank(),
            ),
        )
        scope.launch {
            coreProcessService.events.collect { event ->
                diagnosticLogger.writeCoreEvent(event)
                reduceEvent(event)
                eventChannel.send(event)
            }
        }
        scope.launch {
            coreProcessService.logs.collect { log ->
                diagnosticLogger.writeCoreProcessLog(log)
                appendLog(
                    level = if (log.level == CoreLogLevel.ERROR) {
                        AppLogLevel.ERROR
                    } else {
                        AppLogLevel.INFO
                    },
                    message = log.message,
                    persist = false,
                )
            }
        }
        scope.launch {
            coreProcessService.isRunning.collect { running ->
                _state.update {
                    it.copy(
                        coreRunning = running,
                        receiverReady = if (running) it.receiverReady else false,
                    )
                }
            }
        }
        scope.launch {
            coreProcessService.exits.collect { exit ->
                diagnosticLogger.writeCoreProcessExit(exit)
                val level = if (exit.expected || exit.exitCode == 0) {
                    AppLogLevel.INFO
                } else {
                    AppLogLevel.ERROR
                }
                appendLog(
                    level,
                    "Core process exited with code ${exit.exitCode}.",
                    persist = false,
                )
                if (!exit.expected && exit.exitCode != 0) {
                    _state.update {
                        it.copy(lastError = "Core process exited with code ${exit.exitCode}.")
                    }
                }
            }
        }
        scope.launch {
            initialize()
        }
    }

    suspend fun awaitInitialized() {
        initializationComplete.await()
    }

    suspend fun updateReceiverSettings(
        outputDeviceId: String?,
        xiaomiNetworkAdapterId: String?,
    ): ReceiverSettingsChange {
        awaitInitialized()
        var change = ReceiverSettingsChange(
            outputDeviceChanged = false,
            xiaomiNetworkAdapterChanged = false,
        )
        runBusyAction("Unable to save receiver settings") {
            val current = _state.value.settings
            change = receiverSettingsChange(
                current = current,
                outputDeviceId = outputDeviceId,
                xiaomiNetworkAdapterId = xiaomiNetworkAdapterId,
            )
            val updated = settingsStore.update {
                it.copy(
                    outputDeviceId = outputDeviceId,
                    xiaomiNetworkAdapterId = xiaomiNetworkAdapterId,
                )
            }
            _state.update { it.copy(settings = updated) }
            appendLog(
                AppLogLevel.INFO,
                if (change.changed) {
                    "Receiver settings saved."
                } else {
                    "Receiver settings were already up to date."
                },
            )
        }
        return change
    }

    suspend fun refreshXiaomiNetworkAdapters(
        loader: suspend () -> XiaomiNetworkAdapterList,
    ): Boolean {
        awaitInitialized()
        return xiaomiNetworkAdapterRefreshMutex.withLock {
            _state.update {
                it.copy(xiaomiNetworkAdaptersLoading = true)
            }
            try {
                val result = loader()
                _state.update {
                    it.copy(
                        xiaomiNetworkAdapters = result.adapters,
                        xiaomiAutoSelectedAdapterId =
                            result.autoSelectedAdapterId,
                    )
                }
                appendLog(
                    AppLogLevel.INFO,
                    "Xiaomi MiPlay network adapters refreshed " +
                        "(${result.adapters.size} found).",
                )
                true
            } catch (exception: Exception) {
                if (exception is CancellationException) {
                    throw exception
                }
                val message = exception.toXiaomiMiPlayUserMessage(
                    "刷新小米妙播网卡失败",
                )
                _state.update {
                    it.copy(
                        lastError = message,
                    )
                }
                appendLog(AppLogLevel.ERROR, message)
                false
            } finally {
                _state.update {
                    it.copy(xiaomiNetworkAdaptersLoading = false)
                }
            }
        }
    }

    suspend fun setStartupEnabled(enabled: Boolean) {
        awaitInitialized()
        runBusyAction("Unable to update startup registration") {
            try {
                val updated = startupManager.setEnabled(enabled)
                _state.update {
                    it.copy(
                        settings = updated,
                        startupRegistered = startupManager.isRegistrationEnabled(),
                    )
                }
                appendLog(
                    AppLogLevel.INFO,
                    if (enabled) {
                        "Windows startup enabled."
                    } else {
                        "Windows startup disabled."
                    },
                )
            } catch (exception: Exception) {
                if (exception is CancellationException) {
                    throw exception
                }
                // setEnabled persists the preference before touching HKCU.
                // Reload it so the UI reflects the durable user choice.
                val persisted = runCatching { settingsStore.load() }
                    .getOrDefault(_state.value.settings.copy(startupEnabled = enabled))
                _state.update { it.copy(settings = persisted) }
                throw exception
            }
        }
    }

    suspend fun setAdvancedEffectsEnabled(enabled: Boolean) {
        awaitInitialized()
        runBusyAction("Unable to update advanced effects setting") {
            val updated = settingsStore.update { current ->
                current.copy(advancedEffectsEnabled = enabled)
            }
            _state.update { it.copy(settings = updated) }
            appendLog(
                AppLogLevel.INFO,
                "Advanced effects ${if (enabled) "enabled" else "disabled"}.",
            )
        }
    }

    suspend fun setAutoWakeEnabled(enabled: Boolean) {
        awaitInitialized()
        runBusyAction("Unable to update automatic wake setting") {
            val updated = settingsStore.update { current ->
                current.copy(autoWakeEnabled = enabled)
            }
            _state.update { it.copy(settings = updated) }
            appendLog(
                AppLogLevel.INFO,
                "Automatic wake ${if (enabled) "enabled" else "disabled"}.",
            )
        }
    }

    suspend fun setProtocolEnabled(
        protocol: ReceiverProtocol,
        enabled: Boolean,
    ) {
        awaitInitialized()
        runBusyAction("Unable to update protocol settings") {
            val updated = settingsStore.update { current ->
                when (protocol) {
                    ReceiverProtocol.MIPLAY ->
                        current.copy(miPlayEnabled = enabled)

                    ReceiverProtocol.AIRPLAY ->
                        current.copy(airPlayEnabled = enabled)

                    ReceiverProtocol.DLNA ->
                        current.copy(dlnaEnabled = enabled)
                }
            }
            _state.update { it.copy(settings = updated) }
            appendLog(
                AppLogLevel.INFO,
                "${protocol.name} receiver ${if (enabled) "enabled" else "disabled"}.",
            )
        }
    }

    suspend fun setMiPlayDeviceIdentity(identity: MiPlayDeviceIdentity) {
        awaitInitialized()
        val previousSettings = _state.value.settings
        if (previousSettings.miPlayDeviceIdentity == identity) {
            return
        }

        // Project the user's choice before persistence and listener restart.
        // The settings-driven lifecycle effect can therefore render the new
        // radio state immediately instead of waiting for disk I/O to finish.
        _state.update { current ->
            current.copy(
                settings = current.settings.copy(
                    miPlayDeviceIdentity = identity,
                ),
            )
        }
        try {
            runBusyAction("Unable to update MiPlay device identity") {
                val updated = settingsStore.update { current ->
                    current.copy(miPlayDeviceIdentity = identity)
                }
                _state.update { it.copy(settings = updated) }
                appendLog(
                    AppLogLevel.INFO,
                    "MiPlay device identity changed to ${identity.persistedValue}.",
                )
            }
        } catch (exception: Exception) {
            val persisted = runCatching { settingsStore.load() }
                .getOrDefault(previousSettings)
            _state.update { it.copy(settings = persisted) }
            throw exception
        }
    }

    suspend fun setReceiverName(name: String) {
        awaitInitialized()
        runBusyAction("Unable to update receiver name") {
            val normalizedName = name.trim().takeIf(String::isNotEmpty)
            require(
                normalizedName == null ||
                    normalizedName.length <= AppSettings.MAX_RECEIVER_NAME_LENGTH,
            ) {
                "Receiver name must not exceed " +
                    "${AppSettings.MAX_RECEIVER_NAME_LENGTH} characters."
            }
            val updated = settingsStore.update { current ->
                current.copy(receiverName = normalizedName)
            }
            _state.update { it.copy(settings = updated) }
            appendLog(
                AppLogLevel.INFO,
                "Receiver name updated.",
            )
        }
    }

    suspend fun reconcileCoreProtocols() {
        awaitInitialized()
        runBusyAction("Unable to apply AirPlay/DLNA settings") {
            val settings = _state.value.settings
            coreProcessService.start(
                receiverName = receiverName,
                outputDeviceId = null,
                airPlayEnabled = settings.airPlayEnabled,
                dlnaEnabled = settings.dlnaEnabled,
            )
            deactivateDisabledCorePlayback(settings)
        }
    }

    suspend fun startReceiver() {
        awaitInitialized()
        runBusyAction("Unable to start receiver") {
            val settings = _state.value.settings
            coreProcessService.start(
                receiverName = receiverName,
                outputDeviceId = null,
                airPlayEnabled = settings.airPlayEnabled,
                dlnaEnabled = settings.dlnaEnabled,
            )
        }
    }

    suspend fun restartReceiver() {
        awaitInitialized()
        runBusyAction("Unable to restart receiver") {
            coreProcessService.shutdown()
            deactivateCorePlayback()
            val settings = _state.value.settings
            coreProcessService.start(
                receiverName = receiverName,
                outputDeviceId = null,
                airPlayEnabled = settings.airPlayEnabled,
                dlnaEnabled = settings.dlnaEnabled,
            )
        }
    }

    suspend fun stopReceiver() {
        awaitInitialized()
        runBusyAction("Unable to stop receiver") {
            coreProcessService.shutdown()
            resetPlayback()
        }
    }

    suspend fun sendPlayback(command: PlaybackCommand): String {
        awaitInitialized()
        return coreProcessService.sendPlayback(
            command = command,
            source = AppStateTransitions.commandSource(_state.value),
        )
    }

    suspend fun seek(positionMs: Long): String {
        awaitInitialized()
        val target = positionMs.coerceAtLeast(0L)
        return coreProcessService.sendSeek(
            positionMs = target,
            source = AppStateTransitions.commandSource(_state.value),
        )
    }

    suspend fun sendVolume(percent: Int): String {
        awaitInitialized()
        return coreProcessService.sendVolume(
            percent = percent.coerceIn(0, 100),
            source = AppStateTransitions.commandSource(_state.value),
        )
    }

    suspend fun sendVideoState(
        positionMs: Long,
        durationMs: Long,
        rate: Double,
        ready: Boolean,
        source: String? = null,
    ) {
        awaitInitialized()
        coreProcessService.sendVideoState(
            positionMs = positionMs,
            durationMs = durationMs,
            rate = rate,
            ready = ready,
            source = source,
        )
    }

    suspend fun takeOverWithExternalSource(
        source: String,
        mediaKind: String? = null,
    ) {
        awaitInitialized()
        coreProcessService.sendSourceTakeover(
            source = source,
            mediaKind = mediaKind,
        )
    }

    fun clearLogs() {
        _state.update { it.copy(logs = emptyList()) }
    }

    fun clearError() {
        _state.update { it.copy(lastError = null) }
    }

    fun reportExternalLog(
        message: String,
        isError: Boolean = false,
        persist: Boolean = true,
    ) {
        appendLog(
            if (isError) AppLogLevel.ERROR else AppLogLevel.INFO,
            message,
            persist,
        )
        if (isError) {
            _state.update { it.copy(lastError = message) }
        }
    }

    fun updateNetworkMediaState(
        positionMs: Long,
        durationMs: Long,
        playing: Boolean,
        source: MediaSource? = null,
        sourceEpoch: Long? = null,
    ) {
        _state.update { current ->
            AppStateTransitions.networkProgress(
                current = current,
                positionMs = positionMs,
                durationMs = durationMs,
                playing = playing,
                source = source ?: current.activeMediaSource,
                sourceEpoch = sourceEpoch,
            )
        }
    }

    fun playbackForSource(source: MediaSource): PlaybackSnapshot =
        SourcePlaybackProjection.playback(_state.value, source)

    fun activateSourceProjection(source: MediaSource) {
        _state.update { current ->
            SourcePlaybackProjection.activate(current, source)
        }
    }

    fun markSourcePaused(source: MediaSource) {
        _state.update { current ->
            SourcePlaybackProjection.markPaused(current, source)
        }
    }

    fun pauseConnectedXiaomiAndExposeIfForegroundIdle() {
        _state.update { current ->
            AppStateTransitions.pauseConnectedXiaomiAndExposeIfForegroundIdle(
                current,
            )
        }
    }

    fun activateXiaomiPlayback(
        sourceName: String?,
        newSession: Boolean = false,
        rawState: Int? = null,
        claimPlayback: Boolean = rawState == 2,
    ) {
        _state.update { current ->
            val cachedPlayback = SourcePlaybackProjection.playback(
                current,
                MediaSource.XIAOMI_MIPLAY,
            )
            val updatedPlayback = cachedPlayback.activateXiaomi(
                    sourceName = sourceName,
                    newSession = newSession,
                    rawState = rawState,
            )
            val cached = SourcePlaybackProjection.cacheRemoteControl(
                current = SourcePlaybackProjection.cachePlayback(
                    current = current,
                    source = MediaSource.XIAOMI_MIPLAY,
                    playback = updatedPlayback,
                ),
                source = MediaSource.XIAOMI_MIPLAY,
                remoteControl = RemoteControlState(
                    available = true,
                    commands = XIAOMI_REMOTE_COMMANDS,
                    transport = "miplay_reverse_control",
                    experimental = true,
                ),
            )
            if (claimPlayback && updatedPlayback.isPlaying) {
                SourcePlaybackProjection.activate(
                    cached,
                    MediaSource.XIAOMI_MIPLAY,
                )
            } else {
                cached
            }
        }
    }

    fun updateXiaomiMediaInfo(
        trackId: String? = null,
        title: String?,
        artist: String?,
        album: String?,
        artworkUrl: String?,
        durationMs: Long?,
        positionMs: Long?,
        replaceTrack: Boolean = false,
        codec: String? = null,
        bitrateBps: Long? = null,
        sampleRate: Int? = null,
        bitsPerSample: Int? = null,
        channels: Int? = null,
    ) {
        _state.update { current ->
            SourcePlaybackProjection.updatePlayback(
                current = current,
                source = MediaSource.XIAOMI_MIPLAY,
            ) { playback ->
                playback.applyXiaomiMediaInfo(
                    trackId = trackId,
                    title = title,
                    artist = artist,
                    album = album,
                    artworkUrl = artworkUrl,
                    durationMs = durationMs,
                    positionMs = positionMs,
                    replaceTrack = replaceTrack,
                    codec = codec,
                    bitrateBps = bitrateBps,
                    sampleRate = sampleRate,
                    bitsPerSample = bitsPerSample,
                    channels = channels,
                )
            }
        }
    }

    fun advanceXiaomiProgress(elapsedMs: Long) {
        _state.update { current ->
            val playback = SourcePlaybackProjection.playback(
                current,
                MediaSource.XIAOMI_MIPLAY,
            )
            val updatedPlayback = playback.advanceXiaomiProgress(
                elapsedMs = elapsedMs,
            )
            if (updatedPlayback === playback) {
                current
            } else {
                SourcePlaybackProjection.cachePlayback(
                    current = current,
                    source = MediaSource.XIAOMI_MIPLAY,
                    playback = updatedPlayback,
                )
            }
        }
    }

    fun deactivateXiaomiPlayback() {
        _state.update { current ->
            if (
                MediaSource.XIAOMI_MIPLAY !in
                current.sourcePlaybackStates &&
                current.activeMediaSource != MediaSource.XIAOMI_MIPLAY
            ) {
                current
            } else {
                SourcePlaybackProjection.remove(
                    current,
                    MediaSource.XIAOMI_MIPLAY,
                )
            }
        }
    }

    private fun deactivateCorePlayback() {
        lyricsLoadJob?.cancel()
        dlnaQualityProbeJob?.cancel()
        _state.update { current ->
            SourcePlaybackProjection.remove(
                SourcePlaybackProjection.remove(
                    current,
                    MediaSource.AIRPLAY,
                ),
                MediaSource.DLNA,
            ).copy(selectedCoreMediaSource = null)
        }
    }

    private fun deactivateDisabledCorePlayback(settings: AppSettings) {
        _state.update { current ->
            var updated = current
            if (!settings.airPlayEnabled) {
                updated = SourcePlaybackProjection.remove(updated, MediaSource.AIRPLAY)
            }
            if (!settings.dlnaEnabled) {
                updated = SourcePlaybackProjection.remove(updated, MediaSource.DLNA)
            }
            if (
                updated.selectedCoreMediaSource == MediaSource.AIRPLAY &&
                !settings.airPlayEnabled ||
                updated.selectedCoreMediaSource == MediaSource.DLNA &&
                !settings.dlnaEnabled
            ) {
                updated.copy(selectedCoreMediaSource = null)
            } else {
                updated
            }
        }
    }

    suspend fun closeAsync() {
        if (!closed.compareAndSet(false, true)) {
            return
        }
        diagnosticLogger.write(
            component = "application",
            event = "process",
            outcome = "stopping",
        )
        try {
            coreProcessService.closeAsync()
        } finally {
            diagnosticLogger.write(
                component = "application",
                event = "process",
                outcome = "stopped",
            )
            eventChannel.close()
            viewModelJob.cancel()
            scope.cancel()
        }
    }

    override fun close() {
        runBlocking {
            closeAsync()
        }
    }

    private suspend fun initialize() {
        val xiaomiStartupWarning = startupWarningProvider()
            ?.trim()
            ?.takeIf(String::isNotEmpty)
            ?.take(4_096)
        try {
            val startup = startupInitializer()
            _state.update {
                it.copy(
                    initialized = true,
                    settings = startup.settings,
                    startupRegistered = startup.registrationEnabled,
                    lastError = xiaomiStartupWarning,
                )
            }
            xiaomiStartupWarning?.let {
                appendLog(AppLogLevel.ERROR, it)
            }
            if (startup.firstRun) {
                appendLog(
                    AppLogLevel.INFO,
                    "First run: Windows startup is enabled by default.",
                )
            }
        } catch (exception: Exception) {
            if (exception is CancellationException) {
                initializationComplete.cancel(exception)
                throw exception
            }

            val settings = runCatching { settingsStore.load() }
                .getOrDefault(AppSettings())
            _state.update {
                it.copy(
                    initialized = true,
                    settings = settings,
                    startupRegistered = false,
                    lastError = exception.message,
                )
            }
            appendLog(
                AppLogLevel.ERROR,
                "Startup initialization failed: ${exception.message}",
            )
        } finally {
            if (!initializationComplete.isCompleted) {
                initializationComplete.complete(Unit)
            }
        }
    }

    private suspend fun runBusyAction(
        failurePrefix: String,
        action: suspend () -> Unit,
    ) {
        actionMutex.withLock {
            check(!closed.get()) { "AppViewModel is closed." }
            _state.update { it.copy(busy = true, lastError = null) }
            try {
                action()
            } catch (exception: Exception) {
                if (exception is CancellationException) {
                    throw exception
                }
                val message = "$failurePrefix: ${exception.message ?: exception::class.simpleName}"
                _state.update { it.copy(lastError = message) }
                appendLog(AppLogLevel.ERROR, message)
                throw exception
            } finally {
                _state.update { it.copy(busy = false) }
            }
        }
    }

    private fun reduceEvent(event: AppEvent) {
        _state.update { current ->
            when (event) {
                is AppEvent.Status -> current.copy(
                    lastEvent = event,
                    lastError = if (event.state.equals("error", ignoreCase = true)) {
                        event.message
                    } else {
                        current.lastError
                    },
                )

                is AppEvent.ReceiverReady -> current.copy(
                    coreRunning = true,
                    receiverReady = true,
                    receiverPort = event.port?.takeIf { it > 0 } ?: current.receiverPort,
                    receiverDeviceId = event.deviceId,
                    lastEvent = event,
                )

                is AppEvent.OutputDevice -> {
                    val device = OutputDeviceState(
                        id = event.id,
                        name = event.name,
                        isDefault = event.isDefault,
                        sampleRate = event.sampleRate,
                        channels = event.channels,
                        sampleFormat = event.sampleFormat,
                        bitsPerSample = event.bitsPerSample,
                    )
                    current.copy(
                        outputDevices = (
                            current.outputDevices.filterNot {
                                it.id.equals(device.id, ignoreCase = true)
                            } + device
                            ).sortedBy(OutputDeviceState::name),
                        lastEvent = event,
                    )
                }

                is AppEvent.ClientConnected -> current.copy(
                    connectedClient = event.address,
                    lastEvent = event,
                )

                is AppEvent.ClientDisconnected ->
                    AppStateTransitions.airPlayClientDisconnected(
                        current,
                        event,
                    )

                is AppEvent.StreamStarted ->
                    AppStateTransitions.streamStarted(current, event)

                is AppEvent.StreamStopped ->
                    AppStateTransitions.streamStopped(current, event)

                is AppEvent.SourceTakeover ->
                    AppStateTransitions.sourceTakeover(current, event)

                is AppEvent.NowPlaying ->
                    AppStateTransitions.nowPlaying(current, event)

                is AppEvent.CoverArt -> {
                    val source = SourcePlaybackProjection
                        .sourceForCoreId(event.source)
                        ?: current.selectedCoreMediaSource
                        ?: current.activeMediaSource
                        ?: return@update current.copy(lastEvent = event)
                    if (!eventMatchesSource(current, source, event.epoch)) {
                        current.copy(lastEvent = event)
                    } else {
                        SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = source,
                        ) {
                            it.copy(
                                coverArt = event.path
                                    ?.takeIf(String::isNotBlank)
                                    ?: it.coverArt,
                                sourceEpoch = event.epoch ?: it.sourceEpoch,
                            )
                        }
                    }
                }

                is AppEvent.Progress ->
                    AppStateTransitions.progress(current, event)

                is AppEvent.Volume -> {
                    val source = SourcePlaybackProjection
                        .sourceForCoreId(event.source)
                        ?: current.selectedCoreMediaSource
                        ?: current.activeMediaSource
                    if (
                        source == null ||
                        eventMatchesSource(current, source, event.epoch)
                    ) {
                        if (source == null) {
                            current.copy(lastEvent = event)
                        } else {
                            SourcePlaybackProjection.updatePlayback(
                                current = current.copy(lastEvent = event),
                                source = source,
                            ) {
                                it.copy(
                                    sourceEpoch =
                                        event.epoch ?: it.sourceEpoch,
                                )
                            }
                        }
                    } else {
                        current
                    }
                }

                is AppEvent.DlnaVolume -> {
                    val source = SourcePlaybackProjection
                        .sourceForCoreId(event.source)
                        ?: MediaSource.DLNA
                    if (
                        eventMatchesSource(
                            current,
                            source,
                            event.epoch,
                        )
                    ) {
                        SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = source,
                        ) {
                            it.copy(
                                sourceEpoch = event.epoch ?: it.sourceEpoch,
                            )
                        }
                    } else {
                        current
                    }
                }

                is AppEvent.PlaybackState ->
                    AppStateTransitions.playbackState(current, event)

                is AppEvent.VideoPlay -> {
                    val cached = SourcePlaybackProjection.cachePlayback(
                        current = current.copy(
                            selectedCoreMediaSource = MediaSource.AIRPLAY,
                            lastEvent = event,
                        ),
                        source = MediaSource.AIRPLAY,
                        playback = PlaybackSnapshot(
                        mediaUrl = event.url,
                        mediaKind = "video",
                        protocol = "AirPlay",
                        durationMs = null,
                        positionMs = event.startPositionMs,
                        isPlaying = true,
                        streamActive = true,
                        sourceEpoch = event.epoch,
                    ),
                    )
                    SourcePlaybackProjection.activate(
                        SourcePlaybackProjection.cacheRemoteControl(
                            current = cached,
                            source = MediaSource.AIRPLAY,
                            remoteControl = RemoteControlState(),
                        ),
                        MediaSource.AIRPLAY,
                    )
                }

                is AppEvent.VideoSeek -> {
                    if (!eventMatchesSource(current, MediaSource.AIRPLAY, event.epoch)) {
                        current.copy(lastEvent = event)
                    } else {
                        SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = MediaSource.AIRPLAY,
                        ) {
                            it.copy(positionMs = event.positionMs)
                        }
                    }
                }

                is AppEvent.VideoRate -> {
                    if (!eventMatchesSource(current, MediaSource.AIRPLAY, event.epoch)) {
                        current.copy(lastEvent = event)
                    } else {
                        val cached = SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = MediaSource.AIRPLAY,
                        ) {
                            it.copy(
                                isPlaying = event.rate > 0,
                                streamActive = true,
                            )
                        }
                        if (event.rate > 0) {
                            SourcePlaybackProjection.activate(
                                cached,
                                MediaSource.AIRPLAY,
                            )
                        } else {
                            cached
                        }
                    }
                }

                is AppEvent.VideoStop -> {
                    if (!eventMatchesSource(current, MediaSource.AIRPLAY, event.epoch)) {
                        current.copy(lastEvent = event)
                    } else {
                        SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = MediaSource.AIRPLAY,
                        ) {
                            it.copy(
                                isPlaying = false,
                                streamActive = false,
                                mediaUrl = null,
                            )
                        }
                    }
                }

                is AppEvent.DlnaMedia ->
                    AppStateTransitions.dlnaMedia(current, event)

                is AppEvent.DlnaSeek -> {
                    if (!eventMatchesSource(current, MediaSource.DLNA, event.epoch)) {
                        current.copy(lastEvent = event)
                    } else {
                        SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = MediaSource.DLNA,
                        ) {
                            it.copy(positionMs = event.positionMs)
                        }
                    }
                }

                is AppEvent.DlnaRate -> {
                    if (!eventMatchesSource(current, MediaSource.DLNA, event.epoch)) {
                        current.copy(lastEvent = event)
                    } else {
                        val cached = SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = MediaSource.DLNA,
                        ) {
                            it.copy(
                                isPlaying = event.rate > 0,
                                streamActive = true,
                            )
                        }
                        if (event.rate > 0) {
                            SourcePlaybackProjection.activate(
                                cached,
                                MediaSource.DLNA,
                            )
                        } else {
                            cached
                        }
                    }
                }

                is AppEvent.DlnaStop -> {
                    if (!eventMatchesSource(current, MediaSource.DLNA, event.epoch)) {
                        current.copy(lastEvent = event)
                    } else {
                        SourcePlaybackProjection.updatePlayback(
                            current = current.copy(lastEvent = event),
                            source = MediaSource.DLNA,
                        ) {
                            it.copy(
                                isPlaying = false,
                                streamActive = true,
                            )
                        }
                    }
                }

                is AppEvent.RemoteControlAvailable ->
                    AppStateTransitions.remoteControlAvailable(current, event)

                is AppEvent.RemoteControlUnavailable ->
                    AppStateTransitions.remoteControlUnavailable(current, event)

                is AppEvent.CommandResult -> current.copy(
                    lastError = if (event.succeeded) {
                        current.lastError
                    } else {
                        event.message ?: "Playback command failed."
                    },
                    lastEvent = event,
                )

                is AppEvent.Error -> current.copy(
                    lastError = event.message,
                    lastEvent = event,
                )

                else -> current.copy(lastEvent = event)
            }
        }

        when (event) {
            is AppEvent.Status -> if (event.message.isNotBlank()) {
                appendLog(
                    if (event.state.equals("error", ignoreCase = true)) {
                        AppLogLevel.ERROR
                    } else {
                        AppLogLevel.INFO
                    },
                    event.message,
                    persist = false,
                )
            }

            is AppEvent.CommandResult -> appendLog(
                if (event.succeeded) AppLogLevel.INFO else AppLogLevel.ERROR,
                event.message
                    ?: "Playback command ${event.command.orEmpty()} " +
                    if (event.succeeded) "completed." else "failed.",
                persist = false,
            )

            is AppEvent.Error -> appendLog(
                AppLogLevel.ERROR,
                event.message,
                persist = false,
            )
            is AppEvent.Log -> appendLog(
                if (event.level.equals("error", ignoreCase = true)) {
                    AppLogLevel.ERROR
                } else {
                    AppLogLevel.INFO
                },
                event.message,
                persist = false,
            )

            is AppEvent.Unknown -> appendLog(
                AppLogLevel.INFO,
                "Received unknown core event: ${event.type}",
                persist = false,
            )

            else -> Unit
        }

        when (event) {
            is AppEvent.DlnaMedia -> {
                resolveDlnaLyrics(event)
                resolveDlnaQuality(event)
            }
            is AppEvent.SourceTakeover,
            is AppEvent.StreamStarted,
            is AppEvent.VideoPlay,
            is AppEvent.DlnaStop,
            -> {
                lyricsLoadJob?.cancel()
                dlnaQualityProbeJob?.cancel()
            }

            else -> Unit
        }
    }

    private fun resetPlayback() {
        lyricsLoadJob?.cancel()
        dlnaQualityProbeJob?.cancel()
        _state.update {
            it.copy(
                playback = PlaybackSnapshot(),
                remoteControl = RemoteControlState(),
                activeMediaSource = null,
                selectedCoreMediaSource = null,
                sourcePlaybackStates = emptyMap(),
            )
        }
    }

    private fun resolveDlnaLyrics(event: AppEvent.DlnaMedia) {
        lyricsLoadJob?.cancel()
        val request = LyricsRequest(
            embeddedText = event.lyricsText,
            metadataUri = event.lyricsUri,
            mediaUri = event.url,
        )
        lyricsLoadJob = scope.launch {
            val lyrics = lyricsResolver.resolve(request)
            _state.update { current ->
                val playback = SourcePlaybackProjection.playback(
                    current,
                    MediaSource.DLNA,
                )
                val sameEpoch =
                    event.epoch == null ||
                        playback.sourceEpoch == null ||
                        playback.sourceEpoch == event.epoch
                if (
                    playback.mediaUrl != event.url ||
                    !sameEpoch
                ) {
                    current
                } else {
                    SourcePlaybackProjection.cachePlayback(
                        current = current,
                        source = MediaSource.DLNA,
                        playback = playback.copy(
                            lyrics = lyrics ?: playback.lyrics,
                            lyricsLoading = false,
                        ),
                    )
                }
            }
        }
    }

    private fun resolveDlnaQuality(event: AppEvent.DlnaMedia) {
        dlnaQualityProbeJob?.cancel()
        dlnaQualityProbeJob = scope.launch {
            /*
             * JavaFX often learns duration shortly after DlnaMedia. Waiting one
             * feedback tick lets a static HTTP Content-Length yield a measured
             * average bitrate when DIDL omitted it.
             */
            if (event.durationMs == null) {
                delay(DLNA_QUALITY_DURATION_WAIT_MS)
            }
            val currentDuration = SourcePlaybackProjection.playback(
                _state.value,
                MediaSource.DLNA,
            ).takeIf { playback ->
                playback.mediaUrl == event.url &&
                    (
                        event.epoch == null ||
                            playback.sourceEpoch == null ||
                            playback.sourceEpoch == event.epoch
                        )
            }?.durationMs
            val probed = dlnaMediaProbe.probe(
                url = event.url,
                durationMs = event.durationMs ?: currentDuration,
            )
            if (probed.isEmpty) {
                return@launch
            }
            val enriched = event.copy(
                contentType = preferredDlnaContentType(
                    declared = event.contentType,
                    probed = probed.contentType,
                ),
                bitrateBps = event.bitrateBps ?: probed.bitrateBps,
                sampleRate = event.sampleRate ?: probed.sampleRate,
                bitsPerSample =
                    event.bitsPerSample ?: probed.bitsPerSample,
                channels = event.channels ?: probed.channels,
            )
            val qualityText =
                AppStateTransitions.dlnaQualityText(enriched) ?: return@launch
            _state.update { current ->
                AppStateTransitions.mergeDlnaProbedQuality(
                    current = current,
                    mediaUrl = event.url,
                    epoch = event.epoch,
                    qualityText = qualityText,
                )
            }
        }
    }

    private fun preferredDlnaContentType(
        declared: String?,
        probed: String?,
    ): String? {
        val normalized = declared
            ?.substringBefore(';')
            ?.trim()
            ?.lowercase()
        return if (
            normalized.isNullOrEmpty() ||
            normalized == "application/octet-stream" ||
            normalized == "application/octetstream"
        ) {
            probed ?: declared
        } else {
            declared
        }
    }

    private fun eventMatchesSource(
        current: AppState,
        source: MediaSource,
        epoch: Long?,
    ): Boolean {
        val currentEpoch = SourcePlaybackProjection
            .playback(current, source)
            .sourceEpoch
        return epoch == null || currentEpoch == null || epoch == currentEpoch
    }

    private fun appendLog(
        level: AppLogLevel,
        message: String,
        persist: Boolean = true,
    ) {
        val normalized = message.trim()
        if (normalized.isEmpty()) {
            return
        }
        if (persist) {
            diagnosticLogger.writeApplicationMessage(level, normalized)
        }
        _state.update { current ->
            val updatedLogs = current.logs + AppLogLine(
                level = level,
                message = normalized,
            )
            current.copy(
                logs = if (updatedLogs.size <= AppState.MAX_LOG_LINES) {
                    updatedLogs
                } else {
                    updatedLogs.takeLast(AppState.MAX_LOG_LINES)
                },
            )
        }
    }

    private companion object {
        const val CORE_SOURCE_AIRPLAY = "airplay"
        const val CORE_SOURCE_DLNA = "dlna"
        const val CORE_SOURCE_XIAOMI = "xiaomi_miplay"
        const val DLNA_QUALITY_DURATION_WAIT_MS = 650L
        val XIAOMI_REMOTE_COMMANDS = setOf(
            "play",
            "pause",
            "play_pause",
            "previous_track",
            "next_track",
            "seek",
        )
    }

}
