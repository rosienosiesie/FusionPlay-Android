package com.fusionplay.android

import android.accessibilityservice.AccessibilityService
import android.content.Intent
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.accessibility.AccessibilityEvent
import androidx.core.content.ContextCompat
import com.airplayreceiver.desktop.backend.AppSettings
import com.airplayreceiver.desktop.backend.SettingsStore
import com.fusionplay.android.media.FusionPlayMediaChannel
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class FusionPlayKeepAliveAccessibilityService : AccessibilityService() {
    private val mainHandler = Handler(Looper.getMainLooper())
    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private var previousPlaying = false
    private var lastWakeElapsedRealtime = 0L
    private val mediaStateListener: () -> Unit = {
        val playing = FusionPlayMediaChannel.currentSnapshot().playing
        mainHandler.post { handlePlaybackState(playing) }
    }

    override fun onServiceConnected() {
        super.onServiceConnected()
        previousPlaying = FusionPlayMediaChannel.currentSnapshot().playing
        FusionPlayMediaChannel.addStateListener(mediaStateListener)
        serviceScope.launch { startReceiverAtBootIfEnabled() }
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) = Unit

    override fun onInterrupt() = Unit

    override fun onDestroy() {
        FusionPlayMediaChannel.removeStateListener(mediaStateListener)
        mainHandler.removeCallbacksAndMessages(null)
        serviceScope.cancel()
        super.onDestroy()
    }

    private fun handlePlaybackState(playing: Boolean) {
        val playbackStarted = playing && !previousPlaying
        previousPlaying = playing
        if (!playbackStarted || MainActivity.isAppInForeground()) return
        serviceScope.launch {
            val settings = loadSettings()
            if (!settings.autoWakeEnabled) return@launch
            if (!FusionPlayMediaChannel.currentSnapshot().playing) return@launch
            wakeApplication(debounce = true)
        }
    }

    private suspend fun startReceiverAtBootIfEnabled() {
        val settings = loadSettings()
        if (!settings.startupEnabled) return
        if (AccessibilityKeepAliveController.wasAppLaunchedThisBoot(this)) return
        if (startReceiverService()) {
            AccessibilityKeepAliveController.markAppLaunchedThisBoot(this)
        }
    }

    private suspend fun loadSettings(): AppSettings = withContext(Dispatchers.IO) {
        runCatching { SettingsStore().load() }.getOrDefault(AppSettings())
    }

    private fun startReceiverService(): Boolean =
        runCatching {
            ContextCompat.startForegroundService(
                this,
                Intent(this, ReceiverForegroundService::class.java),
            )
        }.isSuccess

    private fun wakeApplication(debounce: Boolean) {
        if (MainActivity.isAppInForeground()) return
        val now = SystemClock.elapsedRealtime()
        if (
            debounce &&
            lastWakeElapsedRealtime != 0L &&
            now - lastWakeElapsedRealtime < AUTO_WAKE_DEBOUNCE_MS
        ) {
            return
        }
        lastWakeElapsedRealtime = now
        runCatching {
            startActivity(
                Intent(this, MainActivity::class.java).addFlags(
                    Intent.FLAG_ACTIVITY_NEW_TASK or
                        Intent.FLAG_ACTIVITY_REORDER_TO_FRONT or
                        Intent.FLAG_ACTIVITY_SINGLE_TOP,
                ),
            )
        }
    }

    companion object {
        private const val AUTO_WAKE_DEBOUNCE_MS = 2_000L
    }
}
