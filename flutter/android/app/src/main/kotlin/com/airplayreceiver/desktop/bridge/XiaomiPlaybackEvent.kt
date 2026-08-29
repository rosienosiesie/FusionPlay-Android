package com.airplayreceiver.desktop.bridge

import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull

data class XiaomiPlaybackState(
    val sessionActive: Boolean,
    val sourceName: String?,
    val rawState: Int?,
    val sessionSequence: Long?,
    val eventSequence: Long?,
)

data class XiaomiMediaInfo(
    val title: String?,
    val artist: String?,
    val album: String?,
    val artworkUrl: String?,
    val durationMs: Long?,
    val positionMs: Long?,
    val sessionSequence: Long?,
    val eventSequence: Long?,
    val trackId: String? = null,
    val codec: String? = null,
    val bitrateBps: Long? = null,
    val sampleRate: Int? = null,
    val bitsPerSample: Int? = null,
    val channels: Int? = null,
    val metadataChangeType: Int? = null,
)

fun WindowsBridgeXiaomiEvent.toVolumePercentOrNull(): Int? {
    if (!eventName.equals("volume", ignoreCase = true)) {
        return null
    }
    val objectPayload = payload as? JsonObject ?: return null
    return objectPayload["percent"]
        ?.jsonPrimitive
        ?.longOrNull
        ?.takeIf { it in 0L..100L }
        ?.toInt()
}

fun WindowsBridgeXiaomiEvent.toPlaybackStateOrNull(): XiaomiPlaybackState? {
    if (!eventName.equals("playback_state", ignoreCase = true)) {
        return null
    }
    val objectPayload = payload as? JsonObject ?: return null
    val active = objectPayload["session_active"]
        ?.jsonPrimitive
        ?.booleanOrNull
        ?: return null
    val sourceName = objectPayload["source_name"]
        ?.jsonPrimitive
        ?.contentOrNull
        ?.trim()
        ?.takeIf {
            it.isNotEmpty() &&
                !it.equals("phone", ignoreCase = true)
        }
    val rawState = objectPayload["raw_state"]
        ?.jsonPrimitive
        ?.contentOrNull
        ?.toIntOrNull()
    return XiaomiPlaybackState(
        sessionActive = active,
        sourceName = sourceName,
        rawState = rawState,
        sessionSequence = sessionSequenceFromPayload(objectPayload),
        eventSequence = eventSequenceFromPayload(objectPayload),
    )
}

fun WindowsBridgeXiaomiEvent.toMediaInfoOrNull(): XiaomiMediaInfo? {
    val normalizedEvent = eventName.lowercase()
    if (normalizedEvent !in setOf("media_info", "audio_format", "progress")) {
        return null
    }
    val objectPayload = payload as? JsonObject ?: return null
    fun string(name: String): String? = objectPayload[name]
        ?.jsonPrimitive
        ?.contentOrNull
        ?.trim()
        ?.takeIf(String::isNotEmpty)

    return XiaomiMediaInfo(
        title = string("title"),
        artist = string("artist"),
        album = string("album"),
        artworkUrl = string("artwork_url") ?: string("artwork"),
        durationMs = objectPayload["duration_ms"]
            ?.jsonPrimitive
            ?.longOrNull
            ?.takeIf { it > 0L },
        positionMs = objectPayload["position_ms"]
            ?.jsonPrimitive
            ?.longOrNull
            ?.takeIf { it >= 0L },
        sessionSequence = sessionSequenceFromPayload(objectPayload),
        eventSequence = eventSequenceFromPayload(objectPayload),
        trackId = string("track_id"),
        codec = string("codec"),
        bitrateBps = (objectPayload["bitrate_bps"] ?: objectPayload["bitrate"])
            ?.jsonPrimitive
            ?.longOrNull
            ?.takeIf { it in 8_000L..100_000_000L },
        sampleRate = objectPayload["sample_rate"]
            ?.jsonPrimitive
            ?.longOrNull
            ?.takeIf { it in 8_000L..768_000L }
            ?.toInt(),
        bitsPerSample = objectPayload["bits_per_sample"]
            ?.jsonPrimitive
            ?.longOrNull
            ?.takeIf { it in 1L..64L }
            ?.toInt(),
        channels = objectPayload["channels"]
            ?.jsonPrimitive
            ?.longOrNull
            ?.takeIf { it in 1L..32L }
            ?.toInt(),
        metadataChangeType = objectPayload["metadata_change_type"]
            ?.jsonPrimitive
            ?.longOrNull
            ?.takeIf { it in 0L..2L }
            ?.toInt(),
    )
}

private fun WindowsBridgeXiaomiEvent.sessionSequenceFromPayload(
    payload: JsonObject,
): Long? = sessionSequence
    ?.takeIf { it > 0L }
    ?: payload.positiveLong(
        "session_seq",
        "session_sequence",
        "session_id",
    )

private fun WindowsBridgeXiaomiEvent.eventSequenceFromPayload(
    payload: JsonObject,
): Long? = bridgeSequence
    ?.takeIf { it > 0L }
    ?: payload.positiveLong(
        "event_seq",
        "event_sequence",
        "sequence",
    )

private fun JsonObject.positiveLong(vararg names: String): Long? {
    for (name in names) {
        val value = this[name]
            ?.jsonPrimitive
            ?.contentOrNull
            ?.toLongOrNull()
            ?.takeIf { it > 0L }
        if (value != null) {
            return value
        }
    }
    return null
}
