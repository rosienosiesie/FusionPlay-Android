package com.fusionplay.android

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.media.app.NotificationCompat.MediaStyle
import com.airplayreceiver.desktop.nativebridge.FusionPlayNative
import com.fusionplay.android.media.FusionPlayMediaChannel
import com.fusionplay.android.media.FusionPlayMediaCommand

class ReceiverForegroundService : Service() {
    private var multicastLock: WifiManager.MulticastLock? = null
    private val mainHandler = Handler(Looper.getMainLooper())
    private val notificationRefresh = Runnable {
        refreshNotification()
    }
    private val mediaStateListener: () -> Unit = {
        mainHandler.removeCallbacks(notificationRefresh)
        mainHandler.postDelayed(
            notificationRefresh,
            NOTIFICATION_REFRESH_DEBOUNCE_MS,
        )
    }

    override fun onCreate() {
        super.onCreate()
        diagnosticLogger().write(
            component = "android_service",
            event = "receiver_service",
            outcome = "started",
        )
        createChannel()
        FusionPlayMediaChannel.addStateListener(mediaStateListener)
        startForeground(NOTIFICATION_ID, notification())
        acquireMulticastLock()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        diagnosticLogger().write(
            component = "android_service",
            event = "start_command",
            outcome = "observed",
            details = mapOf("action" to intent?.action),
        )
        when (intent?.action) {
            ACTION_PLAY -> FusionPlayMediaChannel.dispatch(FusionPlayMediaCommand.PLAY)
            ACTION_PAUSE -> FusionPlayMediaChannel.dispatch(FusionPlayMediaCommand.PAUSE)
            ACTION_PREVIOUS -> FusionPlayMediaChannel.dispatch(FusionPlayMediaCommand.PREVIOUS)
            ACTION_NEXT -> FusionPlayMediaChannel.dispatch(FusionPlayMediaCommand.NEXT)
        }
        mainHandler.removeCallbacks(notificationRefresh)
        mainHandler.post(notificationRefresh)
        return START_STICKY
    }

    override fun onDestroy() {
        diagnosticLogger().write(
            component = "android_service",
            event = "receiver_service",
            outcome = "stopped",
        )
        // The Activity can be stopped while a sender is searching, so native
        // MiPlay deliberately outlives its UI bridge. The foreground service
        // is the process-level owner and performs the final shutdown.
        runCatching { FusionPlayNative.nativeStopMiPlay() }
        multicastLock?.let { lock ->
            if (lock.isHeld) {
                lock.release()
            }
        }
        multicastLock = null
        FusionPlayMediaChannel.removeStateListener(mediaStateListener)
        mainHandler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun acquireMulticastLock() {
        val wifi = applicationContext.getSystemService(WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("FusionPlay-mdns").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            return
        }
        val manager = getSystemService(NOTIFICATION_SERVICE) as NotificationManager
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_ID,
                getString(R.string.receiver_notification_channel),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
    }

    private fun notification(): Notification {
        val pendingIntentFlags = PendingIntent.FLAG_UPDATE_CURRENT or
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) PendingIntent.FLAG_IMMUTABLE else 0
        val launch = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            pendingIntentFlags,
        )
        val media = FusionPlayMediaChannel.currentSnapshot()
        val builder = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_launcher_foreground_mark_v3)
            .setContentTitle(getString(R.string.receiver_notification_title))
            .setContentText(getString(R.string.receiver_notification_text))
            .setContentIntent(launch)
            .setCategory(
                if (media.hasMedia) {
                    NotificationCompat.CATEGORY_TRANSPORT
                } else {
                    NotificationCompat.CATEGORY_SERVICE
                },
            )
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setShowWhen(false)
            .setSilent(true)
            .setOnlyAlertOnce(true)
            .setVisibility(NotificationCompat.VISIBILITY_PUBLIC)

        if (media.hasMedia) {
            val compactActions = mutableListOf<Int>()
            if (media.canPrevious) {
                compactActions += builder.addAction(
                    android.R.drawable.ic_media_previous,
                    "上一曲",
                    serviceAction(ACTION_PREVIOUS, 1),
                ).let { compactActions.size }
            }
            if (media.canPlayPause) {
                val action = if (media.playing) ACTION_PAUSE else ACTION_PLAY
                val icon = if (media.playing) {
                    android.R.drawable.ic_media_pause
                } else {
                    android.R.drawable.ic_media_play
                }
                compactActions += builder.addAction(
                    icon,
                    if (media.playing) "暂停" else "播放",
                    serviceAction(action, 2),
                ).let { compactActions.size }
            }
            if (media.canNext) {
                compactActions += builder.addAction(
                    android.R.drawable.ic_media_next,
                    "下一曲",
                    serviceAction(ACTION_NEXT, 3),
                ).let { compactActions.size }
            }
            val style = MediaStyle()
                .setMediaSession(FusionPlayMediaChannel.sessionToken())
            if (compactActions.isNotEmpty()) {
                style.setShowActionsInCompactView(*compactActions.take(3).toIntArray())
            }
            builder.setStyle(style)
        }
        return builder.build()
    }

    @SuppressLint("MissingPermission")
    private fun refreshNotification() {
        try {
            NotificationManagerCompat.from(this).notify(
                NOTIFICATION_ID,
                notification(),
            )
        } catch (_: SecurityException) {
            // A denied app or channel permission can reject non-media updates.
            diagnosticLogger().write(
                component = "android_service",
                event = "notification_refresh",
                outcome = "warning",
                details = mapOf("permission_denied" to true),
            )
        }
    }

    private fun diagnosticLogger() =
        (application as FusionPlayFlutterApplication).diagnosticLogger

    private fun serviceAction(action: String, requestCode: Int): PendingIntent {
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                PendingIntent.FLAG_IMMUTABLE
            } else {
                0
            }
        return PendingIntent.getService(
            this,
            requestCode,
            Intent(this, ReceiverForegroundService::class.java).setAction(action),
            flags,
        )
    }

    companion object {
        private const val CHANNEL_ID = "fusionplay-receiver"
        private const val NOTIFICATION_ID = 0x4650
        private const val ACTION_PLAY = "com.fusionplay.android.media.PLAY"
        private const val ACTION_PAUSE = "com.fusionplay.android.media.PAUSE"
        private const val ACTION_PREVIOUS = "com.fusionplay.android.media.PREVIOUS"
        private const val ACTION_NEXT = "com.fusionplay.android.media.NEXT"
        private const val NOTIFICATION_REFRESH_DEBOUNCE_MS = 150L
    }
}
