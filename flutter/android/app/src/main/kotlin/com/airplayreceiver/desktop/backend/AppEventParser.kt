package com.airplayreceiver.desktop.backend

import java.util.Locale
import kotlinx.serialization.SerializationException
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.doubleOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull

class AppEventParseException(
    message: String,
    cause: Throwable? = null,
) : IllegalArgumentException(message, cause)

object AppEventParser {
    private val json = Json {
        isLenient = false
        ignoreUnknownKeys = true
    }

    fun parse(line: String): AppEvent {
        val raw = line.trim()
        if (raw.isEmpty()) {
            throw AppEventParseException("Core event line is empty.")
        }

        val payload = try {
            json.parseToJsonElement(raw) as? JsonObject
                ?: throw AppEventParseException("Core event root must be a JSON object.")
        } catch (exception: SerializationException) {
            throw AppEventParseException("Core event is not valid JSON.", exception)
        }

        val type = payload.string("type", "event")
            ?.trim()
            ?.lowercase(Locale.ROOT)
            ?.takeIf(String::isNotEmpty)
            ?: "unknown"

        return when (type) {
            "status" -> AppEvent.Status(
                state = payload.string("state", "status") ?: "unknown",
                message = payload.string("message", "detail").orEmpty(),
                rawJson = raw,
            )

            "receiver_ready", "ready" -> AppEvent.ReceiverReady(
                name = payload.string("name"),
                pin = payload.string("pin"),
                port = payload.int("port"),
                deviceId = payload.string("device_id"),
                rawJson = raw,
            )

            "output_device" -> AppEvent.OutputDevice(
                name = payload.string("name", "display_name", "device").orEmpty(),
                id = payload.string("id", "device_id")
                    ?: payload.string("name", "display_name", "device").orEmpty(),
                isDefault = payload.boolean("is_default", "default") == true,
                sampleRate = payload.int("sample_rate"),
                channels = payload.int("channels"),
                sampleFormat = payload.string("sample_format"),
                bitsPerSample = payload.int("bits_per_sample"),
                rawJson = raw,
            )

            "client_connected" -> AppEvent.ClientConnected(
                address = payload.string("address", "client"),
                rawJson = raw,
            )

            "client_disconnected" -> AppEvent.ClientDisconnected(
                address = payload.string("address", "client"),
                rawJson = raw,
            )

            "stream_started" -> AppEvent.StreamStarted(
                sourceCodec = payload.string("source_codec"),
                sourceSampleRate = payload.int("source_sample_rate"),
                sourceChannels = payload.int("source_channels"),
                sourceBits = payload.int("source_bits"),
                decodedSampleRate = payload.int("decoded_sample_rate"),
                decodedChannels = payload.int("decoded_channels"),
                decodedBits = payload.int("decoded_bits"),
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "stream_stopped", "stopped" -> AppEvent.StreamStopped(
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "source_takeover" -> AppEvent.SourceTakeover(
                source = payload.string("source").orEmpty(),
                mediaKind = payload.string("media_kind"),
                epoch = payload.long("epoch") ?: 0L,
                previousSource = payload.string("previous_source"),
                previousMediaKind = payload.string("previous_media_kind"),
                previousEpoch = payload.long("previous_epoch"),
                reason = payload.string("reason"),
                rawJson = raw,
            )

            "now_playing", "metadata" -> AppEvent.NowPlaying(
                title = payload.string("title", "track", "name"),
                artist = payload.string("artist", "album_artist"),
                album = payload.string("album"),
                genre = payload.string("genre"),
                durationMs = payload.long("duration_ms")
                    ?: payload.double("duration")?.let { (it * 1_000.0).toLong() },
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "cover_art" -> AppEvent.CoverArt(
                path = payload.string("path", "file", "file_path"),
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "volume" -> AppEvent.Volume(
                decibels = payload.double("db", "decibels"),
                percent = payload.int("percent"),
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "progress" -> AppEvent.Progress(
                positionMs = payload.long("position_ms", "elapsed_ms")
                    ?: payload.double("position", "elapsed", "current")
                        ?.let { (it * 1_000.0).toLong() }
                    ?: 0,
                durationMs = payload.long("duration_ms", "total_ms")
                    ?: payload.double("duration", "total")
                        ?.let { (it * 1_000.0).toLong() }
                    ?: 0,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "playback_state" -> AppEvent.PlaybackState(
                playing = payload.boolean("playing", "is_playing")
                    ?: payload.string("state", "status")
                        ?.equals("playing", ignoreCase = true)
                    ?: false,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "video_play" -> AppEvent.VideoPlay(
                url = payload.string("url").orEmpty(),
                startPositionMs = payload.long("start_position_ms") ?: 0,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "video_seek" -> AppEvent.VideoSeek(
                positionMs = payload.long("position_ms") ?: 0,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "video_rate" -> AppEvent.VideoRate(
                rate = payload.double("rate") ?: 0.0,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "video_stop" -> AppEvent.VideoStop(
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "dlna_ready" -> AppEvent.DlnaReady(
                port = payload.int("port"),
                deviceUuid = payload.string("device_uuid", "uuid"),
                rawJson = raw,
            )

            "dlna_media" -> AppEvent.DlnaMedia(
                url = payload.string("url").orEmpty(),
                title = payload.string("title"),
                artist = payload.string("artist"),
                album = payload.string("album"),
                artworkUrl = payload.string("artwork_url", "album_art_url"),
                contentType = payload.string("content_type", "mime_type"),
                upnpClass = payload.string("upnp_class", "class"),
                mediaKind = payload.string("media_kind", "kind"),
                durationMs = payload.long("duration_ms"),
                startPositionMs = payload.long("start_position_ms") ?: 0,
                lyricsText = payload.string(
                    "lyrics_text",
                    "lyrics",
                    "synchronized_lyrics",
                ),
                lyricsUri = payload.string(
                    "lyrics_uri",
                    "lyric_uri",
                    "lyrics_url",
                    "lyric_url",
                ),
                bitrateBps = payload.long("bitrate_bps"),
                sampleRate = payload.int("sample_rate"),
                bitsPerSample = payload.int("bits_per_sample"),
                channels = payload.int("channels"),
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "dlna_seek" -> AppEvent.DlnaSeek(
                positionMs = payload.long("position_ms") ?: 0,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "dlna_rate" -> AppEvent.DlnaRate(
                rate = payload.double("rate") ?: 0.0,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "dlna_stop" -> AppEvent.DlnaStop(
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "dlna_volume" -> AppEvent.DlnaVolume(
                percent = payload.int("percent", "volume"),
                muted = payload.boolean("muted", "mute"),
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "remote_control_available" -> AppEvent.RemoteControlAvailable(
                commands = payload.stringArray("commands").toSet(),
                transport = payload.string("transport"),
                experimental = payload.boolean("experimental") == true,
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "remote_control_unavailable" -> AppEvent.RemoteControlUnavailable(
                reason = payload.string("reason", "message"),
                rawJson = raw,
                source = payload.string("source"),
                epoch = payload.long("epoch"),
            )

            "command_result" -> AppEvent.CommandResult(
                requestId = payload.string("request_id"),
                command = payload.string("command"),
                succeeded = payload.boolean("ok", "success") == true,
                message = payload.string("message", "error"),
                rawJson = raw,
            )

            "error" -> AppEvent.Error(
                message = payload.string("message", "error", "detail")
                    ?: "The core reported an unknown error.",
                rawJson = raw,
            )

            "log" -> AppEvent.Log(
                level = payload.string("level"),
                message = payload.string("message").orEmpty(),
                rawJson = raw,
            )

            else -> AppEvent.Unknown(type, raw)
        }
    }

    private fun JsonObject.string(vararg names: String): String? {
        for (name in names) {
            val primitive = this[name]?.jsonPrimitive ?: continue
            primitive.contentOrNull?.let { return it }
        }
        return null
    }

    private fun JsonObject.long(vararg names: String): Long? {
        for (name in names) {
            val primitive = this[name]?.jsonPrimitive ?: continue
            primitive.longOrNull?.let { return it }
            primitive.contentOrNull?.toLongOrNull()?.let { return it }
        }
        return null
    }

    private fun JsonObject.int(vararg names: String): Int? {
        for (name in names) {
            val primitive = this[name]?.jsonPrimitive ?: continue
            primitive.intOrNull?.let { return it }
            primitive.contentOrNull?.toIntOrNull()?.let { return it }
        }
        return null
    }

    private fun JsonObject.double(vararg names: String): Double? {
        for (name in names) {
            val primitive = this[name]?.jsonPrimitive ?: continue
            primitive.doubleOrNull?.let { return it }
            primitive.contentOrNull?.toDoubleOrNull()?.let { return it }
        }
        return null
    }

    private fun JsonObject.boolean(vararg names: String): Boolean? {
        for (name in names) {
            val primitive = this[name]?.jsonPrimitive ?: continue
            primitive.booleanOrNull?.let { return it }
            primitive.contentOrNull?.toBooleanStrictOrNull()?.let { return it }
        }
        return null
    }

    private fun JsonObject.stringArray(name: String): List<String> {
        val array = this[name] as? JsonArray ?: return emptyList()
        return array.mapNotNull { element ->
            element.jsonPrimitive.contentOrNull
        }
    }
}
