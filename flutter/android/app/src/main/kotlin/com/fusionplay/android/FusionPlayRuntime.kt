package com.fusionplay.android

import android.content.Context
import android.content.Intent
import com.airplayreceiver.desktop.backend.AppEvent
import com.airplayreceiver.desktop.backend.AppState
import com.airplayreceiver.desktop.backend.AppViewModel
import com.airplayreceiver.desktop.backend.CorePlaybackStateSideEffect
import com.airplayreceiver.desktop.backend.FusionPlayDiagnosticLogger
import com.airplayreceiver.desktop.backend.MediaSource
import com.airplayreceiver.desktop.backend.MediaSourceArbiter
import com.airplayreceiver.desktop.backend.MiPlayDeviceIdentity
import com.airplayreceiver.desktop.backend.NetworkMediaEventGate
import com.airplayreceiver.desktop.backend.PlaybackCommand
import com.airplayreceiver.desktop.backend.ReceiverProtocol
import com.airplayreceiver.desktop.backend.SourceTakeoverPolicy
import com.airplayreceiver.desktop.backend.XiaomiTakeoverGate
import com.airplayreceiver.desktop.bridge.WindowsBridgeDiagnostic
import com.airplayreceiver.desktop.bridge.WindowsBridgeClient
import com.airplayreceiver.desktop.bridge.WindowsBridgeError
import com.airplayreceiver.desktop.bridge.WindowsBridgeProcessExit
import com.airplayreceiver.desktop.bridge.WindowsBridgeSmtcCommand
import com.airplayreceiver.desktop.bridge.WindowsBridgeXiaomiEvent
import com.airplayreceiver.desktop.bridge.WindowsBridgeXiaomiExit
import com.airplayreceiver.desktop.bridge.WindowsBridgeXiaomiLog
import com.airplayreceiver.desktop.bridge.XiaomiPlaybackMutation
import com.airplayreceiver.desktop.bridge.XiaomiPlaybackReducer
import com.airplayreceiver.desktop.bridge.toVolumePercentOrNull
import com.airplayreceiver.desktop.bridge.toXiaomiMiPlayUserMessage
import com.fusionplay.android.media.FusionPlayMediaChannel
import com.fusionplay.android.media.FusionPlayMediaCommand
import io.flutter.plugin.common.BinaryMessenger
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import java.io.Closeable
import java.net.URI
import java.nio.file.Paths
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

/**
 * Process-scoped owner of the original FusionPlay backend.
 *
 * Flutter is intentionally only a view layer. Protocol lifecycle, source
 * arbitration, playback projection and transport controls remain here and use
 * the same backend policies as the original Android frontend.
 */
class FusionPlayRuntime(
    private val context: Context,
    private val diagnosticLogger: FusionPlayDiagnosticLogger,
) : Closeable {
    private val job = SupervisorJob()
    private val scope = CoroutineScope(Dispatchers.Main.immediate + job)
    private val viewModel = AppViewModel(
        diagnosticLogger = diagnosticLogger,
        parentScope = scope,
    )
    private val bridge = WindowsBridgeClient(parentScope = scope, stopXiaomiOnClose = false)
    private val xiaomiReducer = XiaomiPlaybackReducer()
    private val xiaomiTakeoverGate = XiaomiTakeoverGate()
    private val mediaSourceArbiter = MediaSourceArbiter()
    private val coreLifecycleMutex = Mutex()
    private val miPlayLifecycleMutex = Mutex()
    private val networkPlayer = NativeNetworkPlayer(context) { message ->
        viewModel.reportExternalLog("网络媒体播放失败：$message", isError = true)
    }
    private val closed = AtomicBoolean(false)

    private var eventSink: EventChannel.EventSink? = null
    private var lastDesiredCoreConfiguration: CoreReceiverConfiguration? = null
    private var lastDesiredMiPlayConfiguration: MiPlayReceiverConfiguration? = null
    private var appliedMiPlayConfiguration: MiPlayReceiverConfiguration? = null
    private var lastPublishedArtwork: String? = null
    private var lastPublishedMetadata: MediaMetadataProjection? = null
    private var lastPublishedPlaying: Boolean? = null
    private var lastPublishedCapabilities: MediaCapabilitiesProjection? = null
    private var mediaSessionHasMedia = false
    private var artworkJob: Job? = null
    private var xiaomiRestartGeneration = 0L
    private var pendingManualXiaomiStop = false
    private var xiaomiTrackChangeDirection: String? = null
    private var lastMiPlayProgressLogNanos = 0L

    init {
        scope.launch {
            viewModel.awaitInitialized()
            runCatching { bridge.initialize() }
                .onFailure { reportError("媒体桥初始化失败", it) }
            observeDesiredReceivers(viewModel.state.value)
        }
        scope.launch {
            viewModel.state.collect { state ->
                publishState(state)
                publishMediaSession(state)
                observeDesiredReceivers(state)
            }
        }
        scope.launch { bridge.events.collect(::handleBridgeEvent) }
        scope.launch { viewModel.events.collect(::handleCoreEvent) }
        scope.launch {
            FusionPlayMediaChannel.commands.collect { event ->
                when (event.command) {
                    FusionPlayMediaCommand.PLAY -> dispatchPlayback(PlaybackCommand.PLAY)
                    FusionPlayMediaCommand.PAUSE -> dispatchPlayback(PlaybackCommand.PAUSE)
                    FusionPlayMediaCommand.PREVIOUS -> dispatchPlayback(PlaybackCommand.PREVIOUS_TRACK)
                    FusionPlayMediaCommand.NEXT -> dispatchPlayback(PlaybackCommand.NEXT_TRACK)
                    FusionPlayMediaCommand.SEEK -> event.positionMs?.let { dispatchSeek(it) }
                }
            }
        }
        scope.launch {
            var lastTickNanos = System.nanoTime()
            while (isActive) {
                delay(PROGRESS_TICK_MS)
                val nowNanos = System.nanoTime()
                val elapsedMs = ((nowNanos - lastTickNanos) / 1_000_000L).coerceAtLeast(0L)
                lastTickNanos = nowNanos

                val networkSource = networkPlayer.source
                if (networkSource != null) {
                    val networkPlayback = viewModel.playbackForSource(networkSource)
                    if (networkPlayback.mediaUrl != null) {
                        val position = networkPlayer.positionMs
                        val playing = networkPlayer.playing
                        val duration = networkPlayer.durationMs.takeIf { it > 0 }
                            ?: networkPlayback.durationMs
                            ?: 0L
                        viewModel.updateNetworkMediaState(
                            positionMs = position,
                            durationMs = duration,
                            playing = playing,
                            source = networkSource,
                            sourceEpoch = networkPlayer.sourceEpoch,
                        )
                        runCatching {
                            viewModel.sendVideoState(
                                positionMs = position,
                                durationMs = duration,
                                rate = if (playing) networkPlayer.rate else 0.0,
                                ready = networkPlayer.ready,
                                source = when (networkSource) {
                                    MediaSource.AIRPLAY -> "AirPlay"
                                    MediaSource.DLNA -> "DLNA"
                                    MediaSource.XIAOMI_MIPLAY -> null
                                },
                            )
                        }
                    }
                }

                val playback = viewModel.state.value.playback
                if (
                    playback.protocol.equals(XIAOMI_PROTOCOL, ignoreCase = true) &&
                    playback.isPlaying &&
                    (playback.durationMs ?: 0L) > 0L
                ) {
                    viewModel.advanceXiaomiProgress(elapsedMs)
                }
            }
        }
    }

    fun attach(messenger: BinaryMessenger) {
        MethodChannel(messenger, METHOD_CHANNEL).setMethodCallHandler(::onMethodCall)
        EventChannel(messenger, EVENT_CHANNEL).setStreamHandler(
            object : EventChannel.StreamHandler {
                override fun onListen(arguments: Any?, events: EventChannel.EventSink?) {
                    eventSink = events
                    events?.success(stateMap(viewModel.state.value))
                }

                override fun onCancel(arguments: Any?) {
                    eventSink = null
                }
            },
        )
    }

    private fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        if (call.method == "state") {
            result.success(stateMap(viewModel.state.value))
            return
        }
        scope.launch {
            runCatching {
                when (call.method) {
                    "setReceiverName" -> viewModel.setReceiverName(call.string("value"))
                    "setStartupEnabled" -> {
                        val enabled = call.boolean("value")
                        viewModel.setStartupEnabled(enabled)
                        if (enabled) {
                            AccessibilityKeepAliveController.requestAuthorization(context)
                        }
                    }
                    "setAutoWakeEnabled" -> {
                        val enabled = call.boolean("value")
                        viewModel.setAutoWakeEnabled(enabled)
                        if (enabled) {
                            AccessibilityKeepAliveController.requestAuthorization(context)
                        }
                    }
                    "setAdvancedEffectsEnabled" ->
                        viewModel.setAdvancedEffectsEnabled(call.boolean("value"))
                    "setProtocolEnabled" -> viewModel.setProtocolEnabled(
                        ReceiverProtocol.valueOf(call.string("protocol").uppercase()),
                        call.boolean("value"),
                    )
                    "setMiPlayDeviceIdentity" -> viewModel.setMiPlayDeviceIdentity(
                        MiPlayDeviceIdentity.fromPersistedValue(call.string("value")),
                    )
                    "playback" -> when (call.string("command")) {
                        "previous" -> dispatchPlayback(PlaybackCommand.PREVIOUS_TRACK)
                        "next" -> dispatchPlayback(PlaybackCommand.NEXT_TRACK)
                        else -> dispatchPlayback(PlaybackCommand.PLAY_PAUSE)
                    }
                    "seek" -> dispatchSeek(call.long("positionMs"))
                    "volume" -> dispatchVolume(call.int("percent"), applyLocal = true)
                    "exportLogs" -> {
                        viewModel.reportExternalLog("用户请求导出诊断日志。")
                        val intent = withContext(Dispatchers.IO) {
                            FusionPlayLogExporter.createShareIntent(
                                context,
                                diagnosticLogger,
                            )
                        }
                        context.startActivity(
                            intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                        )
                        viewModel.reportExternalLog("已打开系统日志导出菜单。")
                    }
                    "clearError" -> viewModel.clearError()
                    else -> error("Unknown method ${call.method}")
                }
                stateMap(viewModel.state.value)
            }.fold(
                onSuccess = result::success,
                onFailure = { error ->
                    reportError("前后端调用失败", error)
                    result.error("fusionplay_runtime", error.message, null)
                },
            )
        }
    }

    private fun routesControlsToXiaomi(state: AppState): Boolean {
        if (state.activeMediaSource == MediaSource.XIAOMI_MIPLAY) return true
        if (state.activeMediaSource != null) return false
        return state.playback.streamActive &&
            state.playback.protocol.equals(XIAOMI_PROTOCOL, ignoreCase = true)
    }

    private suspend fun sendXiaomiControl(action: String, positionMs: Long? = null): Boolean =
        runCatching { bridge.controlXiaomi(action, positionMs) }
            .fold(
                onSuccess = { response ->
                    val handled = response.accepted || response.dispatched
                    logMiPlay(
                        event = "remote_control",
                        outcome = if (handled) "success" else "failure",
                        details = mapOf(
                            "action" to response.action,
                            "position_ms" to positionMs,
                            "dispatched" to response.dispatched,
                            "confirmed" to response.confirmed,
                            "connection_preserved" to response.connectionPreserved,
                            "method" to response.method,
                            "message" to response.message,
                        ),
                    )
                    if (!response.accepted && !response.dispatched) {
                        viewModel.reportExternalLog(
                            "小米妙播${response.action}控制未执行：${response.message}",
                        )
                    }
                    handled
                },
                onFailure = { error ->
                    logMiPlayFailure("remote_control", error, mapOf("action" to action))
                    viewModel.reportExternalLog(
                        error.toXiaomiMiPlayUserMessage("小米妙播控制失败"),
                        isError = true,
                    )
                    false
                },
            )

    private suspend fun pauseSourceForTakeover(
        source: MediaSource,
        remoteAlreadySuspended: Boolean = false,
    ) {
        val playback = viewModel.playbackForSource(source)
        if (networkPlayer.source == source) {
            viewModel.updateNetworkMediaState(
                positionMs = networkPlayer.positionMs,
                durationMs = networkPlayer.durationMs,
                playing = false,
                source = source,
                sourceEpoch = networkPlayer.sourceEpoch,
            )
            networkPlayer.pause()
        }
        if (remoteAlreadySuspended && source != MediaSource.XIAOMI_MIPLAY) {
            viewModel.markSourcePaused(source)
            return
        }
        when (source) {
            MediaSource.XIAOMI_MIPLAY -> {
                xiaomiTakeoverGate.arm(playback.isPlaying)
                runCatching { bridge.suspendXiaomiOutput() }
                    .onFailure { reportError("小米妙播本机输出暂停失败", it) }
                runCatching { bridge.pauseXiaomi() }
                    .onFailure { reportError("小米妙播暂停失败", it) }
            }
            MediaSource.AIRPLAY -> if (playback.mediaUrl == null) {
                runCatching { viewModel.sendPlayback(PlaybackCommand.PAUSE) }
            }
            MediaSource.DLNA -> runCatching { viewModel.sendPlayback(PlaybackCommand.PAUSE) }
        }
        if (
            source != MediaSource.XIAOMI_MIPLAY ||
            playback.streamActive ||
            playback.isPlaying
        ) {
            viewModel.markSourcePaused(source)
        }
    }

    private suspend fun activatePlaybackSource(
        source: MediaSource,
        previousAlreadySuspendedByCore: Boolean = false,
        projectSource: Boolean = true,
    ) {
        if (source != MediaSource.XIAOMI_MIPLAY) {
            // Do not rely solely on the projected arbiter source here. Xiaomi
            // state and core events are collected independently, so the local
            // projection can briefly be empty while its RTSP audio is still
            // live. Receiver-side suspension is idempotent and survives a
            // concurrent Xiaomi media-session replacement.
            pauseSourceForTakeover(MediaSource.XIAOMI_MIPLAY)
        }
        mediaSourceArbiter.activate(source) { transition ->
            transition.previous
                ?.takeUnless { it == MediaSource.XIAOMI_MIPLAY }
                ?.let {
                pauseSourceForTakeover(it, previousAlreadySuspendedByCore)
            }
        }
        if (projectSource) viewModel.activateSourceProjection(source)
    }

    private fun resumeCachedNetworkSource(source: MediaSource, rate: Double) {
        val playback = viewModel.playbackForSource(source)
        val url = playback.mediaUrl ?: return
        if (networkPlayer.source == source && networkPlayer.url == url) {
            networkPlayer.setRate(rate)
        } else {
            networkPlayer.open(
                source = source,
                url = url,
                epoch = playback.sourceEpoch,
                positionMs = playback.positionMs,
                autoPlay = rate > 0.0,
            )
        }
    }

    private suspend fun dispatchPlayback(command: PlaybackCommand) {
        val state = viewModel.state.value
        val playback = state.playback
        when {
            routesControlsToXiaomi(state) -> when (command) {
                PlaybackCommand.PLAY -> {
                    xiaomiTakeoverGate.explicitResume()
                    if (sendXiaomiControl("play")) {
                        viewModel.activateXiaomiPlayback(
                            sourceName = null,
                            rawState = 2,
                            claimPlayback = true,
                        )
                        mediaSourceArbiter.activate(MediaSource.XIAOMI_MIPLAY) { }
                    }
                }
                PlaybackCommand.PAUSE -> if (sendXiaomiControl("pause")) {
                    xiaomiTakeoverGate.confirmPaused()
                    viewModel.pauseConnectedXiaomiAndExposeIfForegroundIdle()
                    mediaSourceArbiter.deactivate(MediaSource.XIAOMI_MIPLAY)
                }
                PlaybackCommand.PLAY_PAUSE -> dispatchPlayback(
                    if (playback.isPlaying) PlaybackCommand.PAUSE else PlaybackCommand.PLAY,
                )
                PlaybackCommand.PREVIOUS_TRACK -> sendXiaomiControl("previous")
                PlaybackCommand.NEXT_TRACK -> sendXiaomiControl("next")
            }
            playback.mediaUrl != null &&
                playback.protocol.equals("AirPlay", ignoreCase = true) -> when (command) {
                    PlaybackCommand.PLAY -> resumeCachedNetworkSource(MediaSource.AIRPLAY, 1.0)
                    PlaybackCommand.PAUSE -> networkPlayer.pause()
                    PlaybackCommand.PLAY_PAUSE -> if (networkPlayer.playing) {
                        networkPlayer.pause()
                    } else {
                        resumeCachedNetworkSource(MediaSource.AIRPLAY, 1.0)
                    }
                    else -> viewModel.sendPlayback(command)
                }
            else -> viewModel.sendPlayback(command)
        }
    }

    private suspend fun dispatchSeek(positionMs: Long) {
        val state = viewModel.state.value
        when {
            routesControlsToXiaomi(state) -> if (sendXiaomiControl("seek", positionMs)) {
                xiaomiReducer.beginSeek(positionMs)
                viewModel.updateXiaomiMediaInfo(
                    trackId = null,
                    title = null,
                    artist = null,
                    album = null,
                    artworkUrl = null,
                    durationMs = null,
                    positionMs = positionMs,
                )
            }
            state.playback.mediaUrl != null &&
                state.playback.protocol.equals("AirPlay", ignoreCase = true) ->
                networkPlayer.seek(positionMs)
            else -> viewModel.seek(positionMs)
        }
    }

    /**
     * Synchronizes a hardware-key volume change back to the current sender.
     * Sender-originated volume frames only call setSystemMediaVolumePercent,
     * so they never enter this path and cannot form a feedback loop.
     */
    fun syncVolumeFromReceiver() {
        val percent = FusionPlayMediaChannel.currentSystemMediaVolumePercent()
        scope.launch { dispatchVolume(percent, applyLocal = false) }
    }

    private suspend fun dispatchVolume(percent: Int, applyLocal: Boolean) {
        val normalized = percent.coerceIn(0, 100)
        if (applyLocal && !FusionPlayMediaChannel.setSystemMediaVolumePercent(normalized)) {
            viewModel.reportExternalLog("无法将被控端媒体音量调整到 $normalized%。", isError = true)
        }
        val state = viewModel.state.value
        when {
            routesControlsToXiaomi(state) -> {
                if (!bridge.setXiaomiVolume(normalized)) {
                    viewModel.reportExternalLog("小米妙播发送端音量同步失败。", isError = true)
                }
            }
            state.activeMediaSource == MediaSource.AIRPLAY ||
                (state.activeMediaSource == null &&
                    state.playback.streamActive &&
                    state.playback.protocol.equals("AirPlay", ignoreCase = true)) ->
                runCatching { viewModel.sendVolume(normalized) }
                    .onFailure { reportError("AirPlay 发送端音量同步失败", it) }
            state.activeMediaSource == MediaSource.DLNA ||
                (state.activeMediaSource == null &&
                    state.playback.streamActive &&
                    state.playback.protocol.equals("DLNA", ignoreCase = true)) ->
                runCatching { viewModel.sendVolume(normalized) }
                    .onFailure { reportError("DLNA 渲染音量同步失败", it) }
        }
    }

    private fun eventTargetsSourcePlayback(protocol: String, source: String?, epoch: Long?): Boolean {
        val mediaSource = when {
            protocol.equals("AirPlay", ignoreCase = true) -> MediaSource.AIRPLAY
            protocol.equals("DLNA", ignoreCase = true) -> MediaSource.DLNA
            else -> return false
        }
        val playback = viewModel.playbackForSource(mediaSource)
        if (!playback.protocol.equals(protocol, ignoreCase = true)) return false
        val expectedSource = if (mediaSource == MediaSource.AIRPLAY) CORE_SOURCE_AIRPLAY else CORE_SOURCE_DLNA
        if (source != null && !source.equals(expectedSource, ignoreCase = true)) return false
        return epoch == null || playback.sourceEpoch == null || epoch == playback.sourceEpoch
    }

    private fun observeDesiredReceivers(state: AppState) {
        if (!state.initialized) return
        val desiredCore = CoreReceiverConfiguration(
            name = viewModel.receiverName,
            airPlayEnabled = state.settings.airPlayEnabled,
            dlnaEnabled = state.settings.dlnaEnabled,
        )
        if (desiredCore != lastDesiredCoreConfiguration) {
            lastDesiredCoreConfiguration = desiredCore
            logCoreConfiguration("airplay", desiredCore.airPlayEnabled, "requested")
            logCoreConfiguration("dlna", desiredCore.dlnaEnabled, "requested")
            scope.launch {
                coreLifecycleMutex.withLock {
                    if (desiredCore != lastDesiredCoreConfiguration) return@withLock
                    runCatching { viewModel.reconcileCoreProtocols() }.fold(
                        onSuccess = {
                            logCoreConfiguration(
                                "airplay",
                                desiredCore.airPlayEnabled,
                                "applied",
                            )
                            logCoreConfiguration(
                                "dlna",
                                desiredCore.dlnaEnabled,
                                "applied",
                            )
                        },
                        onFailure = { error ->
                            logProtocolFailure("airplay", "receiver_configuration", error)
                            logProtocolFailure("dlna", "receiver_configuration", error)
                            reportError("AirPlay/DLNA 启动失败", error)
                        },
                    )
                }
            }
        }

        val desiredMiPlay = MiPlayReceiverConfiguration(
            name = viewModel.receiverName,
            enabled = state.settings.miPlayEnabled,
            identity = state.settings.miPlayDeviceIdentity,
        )
        if (desiredMiPlay != lastDesiredMiPlayConfiguration) {
            lastDesiredMiPlayConfiguration = desiredMiPlay
            scope.launch {
                miPlayLifecycleMutex.withLock {
                    if (desiredMiPlay != lastDesiredMiPlayConfiguration) return@withLock
                    val previous = appliedMiPlayConfiguration
                    val restartGeneration = ++xiaomiRestartGeneration
                    if (previous?.enabled == true) {
                        pendingManualXiaomiStop = true
                        logMiPlay("receiver_stop", "requested")
                        runCatching { bridge.stopXiaomi() }.fold(
                            onSuccess = {
                                logMiPlay("receiver_stop", "success")
                            },
                            onFailure = { error ->
                                logMiPlayFailure("receiver_stop", error)
                            },
                        )
                    }
                    if (desiredMiPlay.enabled) {
                        pendingManualXiaomiStop = false
                        val started = startMiPlay(desiredMiPlay, reportFailure = true)
                        appliedMiPlayConfiguration = desiredMiPlay.copy(enabled = started)
                        if (!started) {
                            scheduleMiPlayRecovery(desiredMiPlay, restartGeneration)
                        }
                    } else {
                        logMiPlay(
                            event = "receiver_configuration",
                            outcome = "disabled",
                            details = mapOf(
                                "identity" to desiredMiPlay.identity.persistedValue,
                            ),
                        )
                        xiaomiReducer.reset()
                        xiaomiTakeoverGate.reset()
                        mediaSourceArbiter.deactivate(MediaSource.XIAOMI_MIPLAY) {
                            viewModel.deactivateXiaomiPlayback()
                        }
                        appliedMiPlayConfiguration = desiredMiPlay
                    }
                }
            }
        }
    }

    private suspend fun startMiPlay(
        configuration: MiPlayReceiverConfiguration,
        reportFailure: Boolean,
    ): Boolean {
        val details = mapOf(
            "identity" to configuration.identity.persistedValue,
            "device_type" to configuration.identity.protocolValue,
            "receiver_name_present" to configuration.name.isNotBlank(),
        )
        logMiPlay("receiver_start", "requested", details)
        return runCatching {
            bridge.startXiaomi(
                receiverName = configuration.name,
                networkAdapterId = null,
                outputDeviceId = null,
                deviceType = configuration.identity.protocolValue,
            )
        }.fold(
            onSuccess = {
                logMiPlay("receiver_start", "success", details)
                true
            },
            onFailure = { error ->
                logMiPlayFailure("receiver_start", error, details)
                if (reportFailure) reportError("小米妙播启动失败", error)
                false
            },
        )
    }

    private fun scheduleMiPlayRecovery(
        configuration: MiPlayReceiverConfiguration,
        restartGeneration: Long,
    ) {
        scope.launch {
            for (retryDelay in XIAOMI_RESTART_DELAYS_MS) {
                delay(retryDelay)
                if (
                    bridge.shuttingDown ||
                    restartGeneration != xiaomiRestartGeneration ||
                    configuration != lastDesiredMiPlayConfiguration
                ) {
                    return@launch
                }
                var started = false
                miPlayLifecycleMutex.withLock {
                    if (
                        restartGeneration == xiaomiRestartGeneration &&
                        configuration == lastDesiredMiPlayConfiguration &&
                        appliedMiPlayConfiguration?.enabled != true
                    ) {
                        started = startMiPlay(configuration, reportFailure = false)
                        if (started) appliedMiPlayConfiguration = configuration
                    }
                }
                if (started) {
                    logMiPlay("receiver_recovery", "success")
                    viewModel.reportExternalLog("小米妙播接收器已自动恢复。")
                    return@launch
                }
            }
            if (
                restartGeneration == xiaomiRestartGeneration &&
                configuration == lastDesiredMiPlayConfiguration
            ) {
                logMiPlay("receiver_recovery", "failure")
                viewModel.reportExternalLog("小米妙播接收器自动恢复失败。", isError = true)
            }
        }
    }

    private suspend fun handleBridgeEvent(event: Any) {
        when (event) {
            is WindowsBridgeSmtcCommand -> when (event.command.lowercase()) {
                "play" -> dispatchPlayback(PlaybackCommand.PLAY)
                "pause" -> dispatchPlayback(PlaybackCommand.PAUSE)
                "previous" -> dispatchPlayback(PlaybackCommand.PREVIOUS_TRACK)
                "next" -> dispatchPlayback(PlaybackCommand.NEXT_TRACK)
                "seek" -> event.positionMs?.let { dispatchSeek(it) }
            }
            is WindowsBridgeXiaomiLog -> {
                logMiPlay(
                    event = "bridge_log",
                    outcome = if (event.isError) "failure" else "observed",
                    details = mapOf("message" to event.message),
                )
                viewModel.reportExternalLog(event.message, event.isError, persist = false)
            }
            is WindowsBridgeError -> {
                logMiPlay(
                    event = "bridge_error",
                    outcome = "failure",
                    details = mapOf(
                        "code" to event.code,
                        "hresult" to event.hresult,
                        "message" to event.message,
                    ),
                )
                viewModel.reportExternalLog("媒体桥错误：${event.message}", isError = true)
            }
            is WindowsBridgeProcessExit -> {
                logMiPlay(
                    event = "bridge_process_exit",
                    outcome = if (event.expected || event.exitCode == 0) {
                        "success"
                    } else {
                        "failure"
                    },
                    details = mapOf(
                        "exit_code" to event.exitCode,
                        "expected" to event.expected,
                    ),
                )
                if (!event.expected) {
                    xiaomiReducer.reset(resetEventSequence = true)
                    xiaomiTakeoverGate.reset()
                    viewModel.reportExternalLog(
                        "媒体桥意外退出（代码 ${event.exitCode}）。",
                        isError = true,
                    )
                }
            }
            is WindowsBridgeDiagnostic -> {
                logMiPlay(
                    event = "bridge_diagnostic",
                    outcome = "observed",
                    details = mapOf(
                        "source" to event.source,
                        "message" to event.message,
                    ),
                )
                viewModel.reportExternalLog("${event.source}：${event.message}")
            }
            is WindowsBridgeXiaomiExit -> handleXiaomiExit(event)
            is WindowsBridgeXiaomiEvent -> handleXiaomiEvent(event)
        }
    }

    private suspend fun handleXiaomiExit(event: WindowsBridgeXiaomiExit) {
        val expectedStop = pendingManualXiaomiStop
        pendingManualXiaomiStop = false
        xiaomiReducer.reset()
        xiaomiTakeoverGate.reset()
        mediaSourceArbiter.deactivate(MediaSource.XIAOMI_MIPLAY) {
            viewModel.deactivateXiaomiPlayback()
        }
        logMiPlay(
            event = "receiver_exit",
            outcome = if (expectedStop || event.exitCode == null || event.exitCode == 0) {
                "success"
            } else {
                "failure"
            },
            details = mapOf(
                "exit_code" to event.exitCode,
                "expected" to expectedStop,
            ),
        )
        if (expectedStop) return
        viewModel.reportExternalLog(
            "小米妙播进程已退出（代码 ${event.exitCode ?: -1}）。",
            isError = event.exitCode != null && event.exitCode != 0,
        )
        if (event.exitCode == 2 || !viewModel.state.value.settings.miPlayEnabled) return
        val state = viewModel.state.value
        val configuration = MiPlayReceiverConfiguration(
            name = viewModel.receiverName,
            enabled = true,
            identity = state.settings.miPlayDeviceIdentity,
        )
        val restartGeneration = ++xiaomiRestartGeneration
        appliedMiPlayConfiguration = configuration.copy(enabled = false)
        scheduleMiPlayRecovery(configuration, restartGeneration)
    }

    private suspend fun handleXiaomiEvent(event: WindowsBridgeXiaomiEvent) {
        event.toVolumePercentOrNull()?.let { percent ->
            if (!FusionPlayMediaChannel.setSystemMediaVolumePercent(percent)) {
                viewModel.reportExternalLog("无法将被控端媒体音量同步到 $percent%。", isError = true)
            }
        }
        val reduction = xiaomiReducer.reduce(event)
        if (shouldPersistMiPlayEvent(event.eventName)) {
            logMiPlay(
                event = "protocol_event",
                outcome = reduction.outcome.name.lowercase(),
                details = mapOf(
                    "event_name" to event.eventName,
                    "event_sequence" to reduction.eventSequence,
                    "session_sequence" to reduction.sessionSequence,
                    "reason" to reduction.reason,
                    "mutation_count" to reduction.mutations.size,
                    "volume_percent" to event.toVolumePercentOrNull(),
                ),
            )
        }
        for (mutation in reduction.mutations) {
            when (mutation) {
                is XiaomiPlaybackMutation.Activate -> {
                    viewModel.activateXiaomiPlayback(
                        sourceName = mutation.sourceName,
                        newSession = mutation.newSession,
                        rawState = mutation.rawState,
                        claimPlayback = false,
                    )
                    when (mutation.rawState) {
                        2 -> {
                            if (!xiaomiTakeoverGate.acceptPlaying()) {
                                viewModel.markSourcePaused(MediaSource.XIAOMI_MIPLAY)
                                continue
                            }
                            val takeover = runCatching {
                                viewModel.takeOverWithExternalSource(CORE_SOURCE_XIAOMI, "audio")
                            }.onFailure { reportError("小米妙播切换播放源失败", it) }
                            if (takeover.isSuccess) {
                                val resumed = runCatching { bridge.resumeXiaomiOutput() }
                                    .onFailure { reportError("小米妙播恢复输出失败", it) }
                                if (resumed.isSuccess) {
                                    activatePlaybackSource(
                                        MediaSource.XIAOMI_MIPLAY,
                                        previousAlreadySuspendedByCore = true,
                                    )
                                }
                            }
                        }
                        null -> Unit
                        else -> {
                            xiaomiTakeoverGate.confirmPaused()
                            viewModel.pauseConnectedXiaomiAndExposeIfForegroundIdle()
                            mediaSourceArbiter.deactivate(MediaSource.XIAOMI_MIPLAY)
                        }
                    }
                }
                is XiaomiPlaybackMutation.Deactivate -> {
                    xiaomiTrackChangeDirection = null
                    xiaomiTakeoverGate.reset()
                    mediaSourceArbiter.deactivate(MediaSource.XIAOMI_MIPLAY) {
                        viewModel.deactivateXiaomiPlayback()
                    }
                }
                is XiaomiPlaybackMutation.ApplyMediaInfo -> mutation.mediaInfo.let { media ->
                    if (mutation.replaceTrack) {
                        xiaomiTrackChangeDirection = when (media.metadataChangeType) {
                            1 -> TRACK_CHANGE_NEXT
                            2 -> TRACK_CHANGE_PREVIOUS
                            else -> null
                        }
                    }
                    viewModel.updateXiaomiMediaInfo(
                        trackId = media.trackId,
                        title = media.title,
                        artist = media.artist,
                        album = media.album,
                        artworkUrl = media.artworkUrl,
                        durationMs = media.durationMs,
                        positionMs = media.positionMs,
                        replaceTrack = mutation.replaceTrack,
                        codec = media.codec,
                        bitrateBps = media.bitrateBps,
                        sampleRate = media.sampleRate,
                        bitsPerSample = media.bitsPerSample,
                        channels = media.channels,
                    )
                }
            }
        }
        runCatching {
            bridge.reportDiagnostic(
                component = "flutter_xiaomi",
                event = "event_reduced",
                outcome = reduction.outcome.name.lowercase(),
                reason = reduction.reason,
                bridgeSequence = reduction.eventSequence,
                sessionSequence = reduction.sessionSequence,
            )
        }
    }

    private suspend fun handleCoreEvent(event: AppEvent) {
        when (event) {
            is AppEvent.SourceTakeover -> if (
                SourceTakeoverPolicy.shouldSuspend(
                    previousSource = event.previousSource,
                    newSource = event.source,
                    candidateSource = CORE_SOURCE_XIAOMI,
                )
            ) {
                mediaSourceArbiter.suspendObserved(MediaSource.XIAOMI_MIPLAY) {
                    pauseSourceForTakeover(MediaSource.XIAOMI_MIPLAY)
                }
            }
            is AppEvent.StreamStarted -> activatePlaybackSource(MediaSource.AIRPLAY)
            is AppEvent.StreamStopped -> if (
                eventTargetsSourcePlayback("AirPlay", event.source, event.epoch)
            ) {
                mediaSourceArbiter.deactivate(MediaSource.AIRPLAY)
            }
            is AppEvent.PlaybackState -> {
                val source = sourceFromCore(event.source)
                if (source != null && CorePlaybackStateSideEffect.isCurrent(viewModel.state.value, event)) {
                    if (event.playing) {
                        activatePlaybackSource(source, projectSource = false)
                    } else {
                        mediaSourceArbiter.deactivate(source)
                    }
                }
            }
            is AppEvent.VideoPlay -> if (
                eventTargetsSourcePlayback("AirPlay", event.source, event.epoch)
            ) {
                activatePlaybackSource(MediaSource.AIRPLAY)
                networkPlayer.open(
                    MediaSource.AIRPLAY,
                    event.url,
                    event.epoch,
                    event.startPositionMs,
                    true,
                )
            }
            is AppEvent.VideoSeek -> if (
                eventTargetsSourcePlayback("AirPlay", event.source, event.epoch) &&
                networkPlayer.source == MediaSource.AIRPLAY
            ) {
                networkPlayer.seek(event.positionMs)
            }
            is AppEvent.VideoRate -> if (
                eventTargetsSourcePlayback("AirPlay", event.source, event.epoch)
            ) {
                if (event.rate > 0.0) {
                    activatePlaybackSource(MediaSource.AIRPLAY)
                    resumeCachedNetworkSource(MediaSource.AIRPLAY, event.rate)
                } else if (networkPlayer.source == MediaSource.AIRPLAY) {
                    networkPlayer.pause()
                }
            }
            is AppEvent.VideoStop -> if (
                eventTargetsSourcePlayback("AirPlay", event.source, event.epoch)
            ) {
                if (networkPlayer.source == MediaSource.AIRPLAY) networkPlayer.stop()
                mediaSourceArbiter.deactivate(MediaSource.AIRPLAY)
            }
            is AppEvent.DlnaMedia -> if (
                event.source == null || event.source.equals(CORE_SOURCE_DLNA, ignoreCase = true)
            ) {
                activatePlaybackSource(MediaSource.DLNA)
                if (!NetworkMediaEventGate.selectsCurrentResource(
                        activeSource = networkPlayer.source,
                        activeUrl = networkPlayer.url,
                        activeEpoch = networkPlayer.sourceEpoch,
                        eventSource = MediaSource.DLNA,
                        eventUrl = event.url,
                        eventEpoch = event.epoch,
                    )
                ) {
                    networkPlayer.open(
                        MediaSource.DLNA,
                        event.url,
                        event.epoch,
                        event.startPositionMs,
                        true,
                    )
                }
            }
            is AppEvent.DlnaSeek -> if (networkEventMatches(
                    MediaSource.DLNA,
                    CORE_SOURCE_DLNA,
                    event.source,
                    event.epoch,
                )
            ) {
                networkPlayer.seek(event.positionMs)
            }
            is AppEvent.DlnaRate -> if (networkEventMatches(
                    MediaSource.DLNA,
                    CORE_SOURCE_DLNA,
                    event.source,
                    event.epoch,
                )
            ) {
                if (event.rate > 0.0) {
                    activatePlaybackSource(MediaSource.DLNA)
                    if (networkPlayer.source == MediaSource.DLNA && networkPlayer.url != null) {
                        networkPlayer.setRate(event.rate)
                    } else {
                        resumeCachedNetworkSource(MediaSource.DLNA, event.rate)
                    }
                } else if (networkPlayer.source == MediaSource.DLNA) {
                    networkPlayer.pause()
                }
            }
            is AppEvent.DlnaStop -> if (networkEventMatches(
                    MediaSource.DLNA,
                    CORE_SOURCE_DLNA,
                    event.source,
                    event.epoch,
                )
            ) {
                if (networkPlayer.source == MediaSource.DLNA) networkPlayer.pause()
                mediaSourceArbiter.deactivate(MediaSource.DLNA)
            }
            is AppEvent.DlnaVolume -> if (
                eventTargetsSourcePlayback("DLNA", event.source, event.epoch) &&
                networkPlayer.source == MediaSource.DLNA
            ) {
                event.percent?.let(networkPlayer::setVolumePercent)
                event.muted?.let(networkPlayer::setMuted)
            }
            else -> Unit
        }
    }

    private fun networkEventMatches(
        expectedSource: MediaSource,
        expectedWireSource: String,
        eventSource: String?,
        eventEpoch: Long?,
    ): Boolean = NetworkMediaEventGate.matches(
        activeSource = networkPlayer.source,
        activeEpoch = networkPlayer.sourceEpoch,
        expectedSource = expectedSource,
        expectedWireSource = expectedWireSource,
        eventSource = eventSource,
        eventEpoch = eventEpoch,
    )

    private fun sourceFromCore(source: String?): MediaSource? = when (source?.trim()?.lowercase()) {
        CORE_SOURCE_AIRPLAY -> MediaSource.AIRPLAY
        CORE_SOURCE_DLNA -> MediaSource.DLNA
        CORE_SOURCE_XIAOMI -> MediaSource.XIAOMI_MIPLAY
        else -> null
    }

    private fun publishState(state: AppState) {
        eventSink?.success(stateMap(state))
    }

    private fun publishMediaSession(state: AppState) {
        val playback = state.playback
        val hasMedia = playback.title != null || playback.streamActive || playback.mediaUrl != null
        if (!hasMedia) {
            if (mediaSessionHasMedia) {
                artworkJob?.cancel()
                artworkJob = null
                FusionPlayMediaChannel.clear()
                mediaSessionHasMedia = false
                lastPublishedArtwork = null
                lastPublishedMetadata = null
                lastPublishedPlaying = null
                lastPublishedCapabilities = null
            }
            return
        }
        mediaSessionHasMedia = true
        val metadata = MediaMetadataProjection(
            title = playback.title ?: playback.protocol ?: "FusionPlay",
            artist = playback.artist,
            album = playback.album,
            mediaIdentity = playback.trackIdentity ?: playback.mediaUrl ?: playback.title,
        )
        if (metadata != lastPublishedMetadata) {
            lastPublishedMetadata = metadata
            FusionPlayMediaChannel.setMetadata(
                title = metadata.title,
                artist = metadata.artist,
                album = metadata.album,
                mediaIdentity = metadata.mediaIdentity,
            )
        }
        if (playback.isPlaying != lastPublishedPlaying) {
            lastPublishedPlaying = playback.isPlaying
            FusionPlayMediaChannel.setPlayback(playback.isPlaying)
        }
        FusionPlayMediaChannel.setTimeline(playback.positionMs, playback.durationMs ?: 0)
        val capabilities = MediaCapabilitiesProjection(
            canPlayPause = true,
            canPrevious = state.remoteControl.commands.any { it.contains("previous", true) },
            canNext = state.remoteControl.commands.any { it.contains("next", true) },
            canSeek = state.remoteControl.commands.any { it.contains("seek", true) },
        )
        if (capabilities != lastPublishedCapabilities) {
            lastPublishedCapabilities = capabilities
            FusionPlayMediaChannel.setCapabilities(
                canPlayPause = capabilities.canPlayPause,
                canPrevious = capabilities.canPrevious,
                canNext = capabilities.canNext,
                canSeek = capabilities.canSeek,
            )
        }
        val artwork = playback.coverArt
        if (artwork != lastPublishedArtwork) {
            lastPublishedArtwork = artwork
            artworkJob?.cancel()
            artworkJob = scope.launch {
                if (artwork.isNullOrBlank()) {
                    FusionPlayMediaChannel.clearArtwork()
                } else {
                    runCatching {
                        when {
                            artwork.startsWith("data:", ignoreCase = true) ->
                                bridge.setArtworkDataUri(artwork)
                            artwork.startsWith("http://", ignoreCase = true) ||
                                artwork.startsWith("https://", ignoreCase = true) ||
                                artwork.startsWith("file:", ignoreCase = true) ->
                                bridge.setArtwork(URI.create(artwork))
                            else -> bridge.setArtwork(Paths.get(artwork))
                        }
                    }.onFailure { reportError("媒体封面更新失败", it) }
                }
            }
        }
    }

    private fun stateMap(state: AppState): Map<String, Any?> = mapOf(
        "initialized" to state.initialized,
        "busy" to state.busy,
        "coreRunning" to state.coreRunning,
        "receiverReady" to state.receiverReady,
        "receiverPort" to state.receiverPort,
        "receiverDeviceId" to state.receiverDeviceId,
        "connectedClient" to state.connectedClient,
        "activeMediaSource" to state.activeMediaSource?.name?.lowercase(),
        "selectedCoreMediaSource" to state.selectedCoreMediaSource?.name?.lowercase(),
        "receiverName" to viewModel.receiverName,
        "settings" to mapOf(
            "schemaVersion" to state.settings.schemaVersion,
            "receiverName" to state.settings.receiverName,
            "startupEnabled" to state.settings.startupEnabled,
            "autoWakeEnabled" to state.settings.autoWakeEnabled,
            "advancedEffectsEnabled" to state.settings.advancedEffectsEnabled,
            "miPlayEnabled" to state.settings.miPlayEnabled,
            "miPlayDeviceIdentity" to state.settings.miPlayDeviceIdentity.persistedValue,
            "airPlayEnabled" to state.settings.airPlayEnabled,
            "dlnaEnabled" to state.settings.dlnaEnabled,
        ),
        "playback" to mapOf(
            "title" to state.playback.title,
            "artist" to state.playback.artist,
            "album" to state.playback.album,
            "coverArt" to state.playback.coverArt,
            "mediaUrl" to state.playback.mediaUrl,
            "mediaKind" to state.playback.mediaKind,
            "protocol" to state.playback.protocol,
            "qualityText" to state.playback.qualityText,
            "durationMs" to state.playback.durationMs,
            "positionMs" to state.playback.positionMs,
            "volumePercent" to FusionPlayMediaChannel.currentSystemMediaVolumePercent(),
            "isPlaying" to state.playback.isPlaying,
            "streamActive" to state.playback.streamActive,
            "sourceEpoch" to state.playback.sourceEpoch,
            "trackIdentity" to state.playback.trackIdentity,
            "trackChangeDirection" to xiaomiTrackChangeDirection.takeIf {
                state.playback.protocol.equals(XIAOMI_PROTOCOL, ignoreCase = true)
            },
        ),
        "remoteControl" to mapOf(
            "available" to state.remoteControl.available,
            "commands" to state.remoteControl.commands.toList(),
            "transport" to state.remoteControl.transport,
            "experimental" to state.remoteControl.experimental,
        ),
        "lastError" to state.lastError,
    )

    private fun reportError(prefix: String, error: Throwable) {
        viewModel.reportExternalLog("$prefix：${error.message}", isError = true)
    }

    private fun logCoreConfiguration(
        component: String,
        enabled: Boolean,
        outcome: String,
    ) {
        diagnosticLogger.write(
            component = component,
            event = "receiver_configuration",
            outcome = outcome,
            details = mapOf("enabled" to enabled),
        )
    }

    private fun logProtocolFailure(
        component: String,
        event: String,
        error: Throwable,
    ) {
        diagnosticLogger.write(
            component = component,
            event = event,
            outcome = "failure",
            details = mapOf(
                "error_type" to error::class.java.simpleName,
                "message" to error.message,
            ),
        )
    }

    private fun logMiPlay(
        event: String,
        outcome: String,
        details: Map<String, Any?> = emptyMap(),
    ) {
        diagnosticLogger.write(
            component = "xiaomi_miplay",
            event = event,
            outcome = outcome,
            details = details,
        )
    }

    private fun logMiPlayFailure(
        event: String,
        error: Throwable,
        details: Map<String, Any?> = emptyMap(),
    ) {
        logMiPlay(
            event = event,
            outcome = "failure",
            details = details + mapOf(
                "error_type" to error::class.java.simpleName,
                "message" to error.message,
            ),
        )
    }

    private fun shouldPersistMiPlayEvent(eventName: String): Boolean {
        if (!eventName.equals("progress", ignoreCase = true)) return true
        val now = System.nanoTime()
        if (now - lastMiPlayProgressLogNanos < MIPLAY_PROGRESS_LOG_INTERVAL_NANOS) {
            return false
        }
        lastMiPlayProgressLogNanos = now
        return true
    }

    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        networkPlayer.close()
        bridge.close()
        viewModel.close()
        scope.cancel()
    }

    private fun MethodCall.string(name: String): String = argument<String>(name).orEmpty()
    private fun MethodCall.boolean(name: String): Boolean = argument<Boolean>(name) == true
    private fun MethodCall.long(name: String): Long = (argument<Number>(name) ?: 0).toLong()
    private fun MethodCall.int(name: String): Int = (argument<Number>(name) ?: 0).toInt()

    private data class CoreReceiverConfiguration(
        val name: String,
        val airPlayEnabled: Boolean,
        val dlnaEnabled: Boolean,
    )

    private data class MiPlayReceiverConfiguration(
        val name: String,
        val enabled: Boolean,
        val identity: MiPlayDeviceIdentity,
    )

    private data class MediaMetadataProjection(
        val title: String,
        val artist: String?,
        val album: String?,
        val mediaIdentity: String?,
    )

    private data class MediaCapabilitiesProjection(
        val canPlayPause: Boolean,
        val canPrevious: Boolean,
        val canNext: Boolean,
        val canSeek: Boolean,
    )

    companion object {
        private const val METHOD_CHANNEL = "com.fusionplay.android/runtime"
        private const val EVENT_CHANNEL = "com.fusionplay.android/runtime_events"
        private const val XIAOMI_PROTOCOL = "小米妙播"
        private const val TRACK_CHANGE_PREVIOUS = "previous"
        private const val TRACK_CHANGE_NEXT = "next"
        private const val CORE_SOURCE_AIRPLAY = "airplay"
        private const val CORE_SOURCE_DLNA = "dlna"
        private const val CORE_SOURCE_XIAOMI = "xiaomi_miplay"
        private const val PROGRESS_TICK_MS = 500L
        private const val MIPLAY_PROGRESS_LOG_INTERVAL_NANOS = 3_000_000_000L
        private val XIAOMI_RESTART_DELAYS_MS = longArrayOf(1_000L, 2_000L, 4_000L)
    }
}
