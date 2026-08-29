package com.fusionplay.android

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.view.Window
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.fusionplay.android.media.FusionPlayMediaChannel
import com.fusionplay.android.media.FusionPlayMediaCommand
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    private val consumedMediaKeys = mutableSetOf<Int>()
    private var navigationKeyChannel: MethodChannel? = null
    private var pendingAccessibilityKeepAlivePrompt = false

    override fun onCreate(savedInstanceState: Bundle?) {
        configureImmersiveMode(window)
        super.onCreate(savedInstanceState)
        AccessibilityKeepAliveController.markAppLaunchedThisBoot(this)
        volumeControlStream = AudioManager.STREAM_MUSIC
        pendingAccessibilityKeepAlivePrompt = shouldShowAccessibilityKeepAlivePrompt()
        requestReceiverPermissions()
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        (application as FusionPlayFlutterApplication).runtime.attach(flutterEngine.dartExecutor.binaryMessenger)
        navigationKeyChannel = MethodChannel(
            flutterEngine.dartExecutor.binaryMessenger,
            NAVIGATION_KEY_CHANNEL,
        )
    }

    override fun onStart() {
        super.onStart()
        appInForeground = true
    }

    override fun onStop() {
        appInForeground = false
        super.onStop()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) configureImmersiveMode(window)
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (isVolumeKey(event.keyCode)) {
            val handled = super.dispatchKeyEvent(event)
            if (event.action == KeyEvent.ACTION_UP) {
                (application as FusionPlayFlutterApplication).runtime.syncVolumeFromReceiver()
            }
            return handled
        }
        if (event.keyCode == KeyEvent.KEYCODE_MENU) {
            if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) {
                navigationKeyChannel?.invokeMethod("menu", null)
            }
            return true
        }
        if (!isMediaKey(event.keyCode)) {
            return super.dispatchKeyEvent(event)
        }
        return when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                if (event.repeatCount > 0 && event.keyCode in consumedMediaKeys) {
                    true
                } else if (dispatchMediaKey(event.keyCode)) {
                    consumedMediaKeys += event.keyCode
                    true
                } else {
                    super.dispatchKeyEvent(event)
                }
            }

            KeyEvent.ACTION_UP -> {
                if (consumedMediaKeys.remove(event.keyCode)) {
                    true
                } else {
                    super.dispatchKeyEvent(event)
                }
            }

            else -> super.dispatchKeyEvent(event)
        }
    }

    private fun dispatchMediaKey(keyCode: Int): Boolean {
        val state = FusionPlayMediaChannel.currentSnapshot()
        val command = when (keyCode) {
            KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE,
            KeyEvent.KEYCODE_HEADSETHOOK,
            -> if (state.canPlayPause) {
                if (state.playing) FusionPlayMediaCommand.PAUSE else FusionPlayMediaCommand.PLAY
            } else {
                null
            }

            KeyEvent.KEYCODE_MEDIA_PLAY ->
                FusionPlayMediaCommand.PLAY.takeIf { state.canPlayPause }
            KeyEvent.KEYCODE_MEDIA_PAUSE,
            KeyEvent.KEYCODE_MEDIA_STOP,
            -> FusionPlayMediaCommand.PAUSE.takeIf { state.canPlayPause }
            KeyEvent.KEYCODE_MEDIA_PREVIOUS ->
                FusionPlayMediaCommand.PREVIOUS.takeIf { state.canPrevious }
            KeyEvent.KEYCODE_MEDIA_NEXT ->
                FusionPlayMediaCommand.NEXT.takeIf { state.canNext }
            else -> null
        } ?: return false
        FusionPlayMediaChannel.dispatch(command)
        return true
    }

    private fun requestReceiverPermissions() {
        if (Build.VERSION.SDK_INT < 33) {
            startReceiverService()
            return
        }
        val permissions = listOf(
            Manifest.permission.POST_NOTIFICATIONS,
            Manifest.permission.NEARBY_WIFI_DEVICES,
        ).filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (permissions.isEmpty()) {
            startReceiverService()
        } else {
            requestPermissions(permissions.toTypedArray(), RECEIVER_PERMISSION_REQUEST)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == RECEIVER_PERMISSION_REQUEST) startReceiverService()
    }

    private fun startReceiverService() {
        ContextCompat.startForegroundService(this, Intent(this, ReceiverForegroundService::class.java))
        promptAccessibilityKeepAliveIfNeeded()
    }

    private fun shouldShowAccessibilityKeepAlivePrompt(): Boolean {
        val preferences = getSharedPreferences(
            ACCESSIBILITY_PROMPT_PREFERENCES,
            Context.MODE_PRIVATE,
        )
        if (preferences.getBoolean(ACCESSIBILITY_PROMPT_SHOWN_KEY, false)) {
            return false
        }
        if (AccessibilityKeepAliveController.isServiceEnabled(this)) {
            preferences.edit().putBoolean(ACCESSIBILITY_PROMPT_SHOWN_KEY, true).apply()
            return false
        }
        return true
    }

    private fun promptAccessibilityKeepAliveIfNeeded() {
        if (!pendingAccessibilityKeepAlivePrompt) return
        pendingAccessibilityKeepAlivePrompt = false
        getSharedPreferences(ACCESSIBILITY_PROMPT_PREFERENCES, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(ACCESSIBILITY_PROMPT_SHOWN_KEY, true)
            .apply()
        AccessibilityKeepAliveController.requestAuthorization(this)
    }

    companion object {
        private const val RECEIVER_PERMISSION_REQUEST = 1201
        private const val NAVIGATION_KEY_CHANNEL = "com.fusionplay.android/navigation_keys"
        private const val ACCESSIBILITY_PROMPT_PREFERENCES = "accessibility_keep_alive_prompt"
        private const val ACCESSIBILITY_PROMPT_SHOWN_KEY = "shown_v1"

        @Volatile
        private var appInForeground = false

        internal fun isAppInForeground(): Boolean = appInForeground

        private fun isMediaKey(keyCode: Int): Boolean =
            keyCode == KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE ||
                keyCode == KeyEvent.KEYCODE_HEADSETHOOK ||
                keyCode == KeyEvent.KEYCODE_MEDIA_PLAY ||
                keyCode == KeyEvent.KEYCODE_MEDIA_PAUSE ||
                keyCode == KeyEvent.KEYCODE_MEDIA_STOP ||
                keyCode == KeyEvent.KEYCODE_MEDIA_PREVIOUS ||
                keyCode == KeyEvent.KEYCODE_MEDIA_NEXT

        private fun isVolumeKey(keyCode: Int): Boolean =
            keyCode == KeyEvent.KEYCODE_VOLUME_UP ||
                keyCode == KeyEvent.KEYCODE_VOLUME_DOWN ||
                keyCode == KeyEvent.KEYCODE_VOLUME_MUTE

        private fun configureImmersiveMode(window: Window) {
            WindowCompat.setDecorFitsSystemWindows(window, false)
            window.statusBarColor = Color.TRANSPARENT
            window.navigationBarColor = Color.TRANSPARENT
            if (Build.VERSION.SDK_INT >= 29) window.isNavigationBarContrastEnforced = false
            WindowCompat.getInsetsController(window, window.decorView).apply {
                hide(WindowInsetsCompat.Type.systemBars())
                systemBarsBehavior =
                    WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            }
        }

    }
}
