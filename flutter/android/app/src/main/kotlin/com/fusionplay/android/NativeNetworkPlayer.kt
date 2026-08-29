package com.fusionplay.android

import android.content.Context
import android.os.Handler
import android.os.Looper
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.PlaybackParameters
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import com.airplayreceiver.desktop.backend.MediaSource
import java.io.Closeable

internal class NativeNetworkPlayer(
    context: Context,
    private val onError: (String) -> Unit = {},
) : Closeable {
    private val applicationContext = context.applicationContext
    private val mainHandler = Handler(Looper.getMainLooper())
    private var player: ExoPlayer? = null

    private fun createPlayer(): ExoPlayer = ExoPlayer.Builder(applicationContext).build().apply {
        setAudioAttributes(
            AudioAttributes.Builder().setUsage(C.USAGE_MEDIA).setContentType(C.AUDIO_CONTENT_TYPE_MUSIC).build(),
            true,
        )
        addListener(object : Player.Listener {
            override fun onIsPlayingChanged(isPlaying: Boolean) {
                playing = isPlaying
            }

            override fun onPlaybackStateChanged(playbackState: Int) {
                ready = playbackState == Player.STATE_READY
            }

            override fun onPlayerError(error: PlaybackException) {
                onError(error.message ?: "未知播放器错误")
            }
        })
        volume = if (muted) 0f else unmutedVolume
    }

    var source: MediaSource? = null
        private set
    var sourceEpoch: Long? = null
        private set
    var url: String? = null
        private set
    var playing: Boolean = false
        private set
    var ready: Boolean = false
        private set
    var rate: Double = 1.0
        private set
    private var unmutedVolume = 1f
    private var muted = false
    val positionMs: Long get() = player?.currentPosition?.coerceAtLeast(0) ?: 0
    val durationMs: Long get() = player?.duration?.takeIf { it > 0 } ?: 0

    fun open(source: MediaSource, url: String, epoch: Long?, positionMs: Long, autoPlay: Boolean) = onMain {
        val activePlayer = player ?: createPlayer().also { player = it }
        this.source = source
        this.url = url
        sourceEpoch = epoch
        rate = 1.0
        activePlayer.playbackParameters = PlaybackParameters.DEFAULT
        activePlayer.setMediaItem(MediaItem.fromUri(url), positionMs.coerceAtLeast(0))
        activePlayer.prepare()
        activePlayer.playWhenReady = autoPlay
    }

    fun play() = onMain { player?.play() }
    fun pause() = onMain { player?.pause() }
    fun seek(positionMs: Long) = onMain { player?.seekTo(positionMs.coerceAtLeast(0)) }
    fun setRate(value: Double) = onMain {
        rate = value.coerceAtLeast(0.0)
        val activePlayer = player ?: return@onMain
        if (rate == 0.0) activePlayer.pause() else {
            activePlayer.playbackParameters = PlaybackParameters(rate.toFloat())
            activePlayer.play()
        }
    }
    fun setVolumePercent(percent: Int) = onMain {
        unmutedVolume = percent.coerceIn(0, 100) / 100f
        if (!muted) player?.volume = unmutedVolume
    }
    fun setMuted(muted: Boolean) = onMain {
        this.muted = muted
        player?.volume = if (muted) 0f else unmutedVolume
    }
    fun stop() = onMain {
        player?.release()
        player = null
        source = null
        url = null
        sourceEpoch = null
        playing = false
        ready = false
    }

    override fun close() = onMain {
        player?.release()
        player = null
    }

    private fun onMain(block: () -> Unit) {
        if (Looper.myLooper() == Looper.getMainLooper()) block() else mainHandler.post(block)
    }
}
