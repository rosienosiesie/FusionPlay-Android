package com.airplayreceiver.desktop.backend

/**
 * Strongly typed representation of one NDJSON event emitted by the Rust core.
 *
 * [rawJson] is retained for diagnostics and forward compatibility, while
 * [Unknown] prevents a newer core event from breaking the reader loop.
 */
sealed interface AppEvent {
    val type: String
    val rawJson: String

    data class Status(
        val state: String,
        val message: String,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "status"
    }

    data class ReceiverReady(
        val name: String?,
        val pin: String?,
        val port: Int?,
        val deviceId: String?,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "receiver_ready"
    }

    data class OutputDevice(
        val name: String,
        val id: String,
        val isDefault: Boolean,
        val sampleRate: Int?,
        val channels: Int?,
        val sampleFormat: String?,
        val bitsPerSample: Int?,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "output_device"
    }

    data class ClientConnected(
        val address: String?,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "client_connected"
    }

    data class ClientDisconnected(
        val address: String?,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "client_disconnected"
    }

    data class StreamStarted(
        val sourceCodec: String?,
        val sourceSampleRate: Int?,
        val sourceChannels: Int?,
        val sourceBits: Int?,
        val decodedSampleRate: Int?,
        val decodedChannels: Int?,
        val decodedBits: Int?,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "stream_started"
    }

    data class StreamStopped(
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "stream_stopped"
    }

    data class SourceTakeover(
        val source: String,
        val mediaKind: String?,
        val epoch: Long,
        val previousSource: String?,
        val previousMediaKind: String?,
        val previousEpoch: Long?,
        val reason: String?,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "source_takeover"
    }

    data class NowPlaying(
        val title: String?,
        val artist: String?,
        val album: String?,
        val genre: String?,
        val durationMs: Long?,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "now_playing"
    }

    data class CoverArt(
        val path: String?,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "cover_art"
    }

    data class Volume(
        val decibels: Double?,
        val percent: Int?,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "volume"
    }

    data class Progress(
        val positionMs: Long,
        val durationMs: Long,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "progress"
    }

    data class PlaybackState(
        val playing: Boolean,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "playback_state"
    }

    data class VideoPlay(
        val url: String,
        val startPositionMs: Long,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "video_play"
    }

    data class VideoSeek(
        val positionMs: Long,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "video_seek"
    }

    data class VideoRate(
        val rate: Double,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "video_rate"
    }

    data class VideoStop(
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "video_stop"
    }

    data class DlnaReady(
        val port: Int?,
        val deviceUuid: String?,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "dlna_ready"
    }

    data class DlnaMedia(
        val url: String,
        val title: String?,
        val artist: String?,
        val album: String?,
        val artworkUrl: String?,
        val contentType: String?,
        val upnpClass: String?,
        val mediaKind: String?,
        val durationMs: Long?,
        val startPositionMs: Long,
        val lyricsText: String? = null,
        val lyricsUri: String? = null,
        val bitrateBps: Long? = null,
        val sampleRate: Int? = null,
        val bitsPerSample: Int? = null,
        val channels: Int? = null,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "dlna_media"
    }

    data class DlnaSeek(
        val positionMs: Long,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "dlna_seek"
    }

    data class DlnaRate(
        val rate: Double,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "dlna_rate"
    }

    data class DlnaStop(
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "dlna_stop"
    }

    data class DlnaVolume(
        val percent: Int?,
        val muted: Boolean?,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "dlna_volume"
    }

    data class RemoteControlAvailable(
        val commands: Set<String>,
        val transport: String?,
        val experimental: Boolean,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "remote_control_available"
    }

    data class RemoteControlUnavailable(
        val reason: String?,
        override val rawJson: String,
        val source: String? = null,
        val epoch: Long? = null,
    ) : AppEvent {
        override val type = "remote_control_unavailable"
    }

    data class CommandResult(
        val requestId: String?,
        val command: String?,
        val succeeded: Boolean,
        val message: String?,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "command_result"
    }

    data class Error(
        val message: String,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "error"
    }

    data class Log(
        val level: String?,
        val message: String,
        override val rawJson: String,
    ) : AppEvent {
        override val type = "log"
    }

    data class Unknown(
        override val type: String,
        override val rawJson: String,
    ) : AppEvent
}
