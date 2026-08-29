package com.airplayreceiver.desktop.bridge

import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class XiaomiPlaybackReducerContinuityTest {
    @Test
    fun resumedSessionMergesPartialMetadataWithPreviousArtwork() {
        val reducer = XiaomiPlaybackReducer()
        reducer.reduce(playbackEvent(session = 1, sequence = 1, active = true, rawState = 2))
        reducer.reduce(
            mediaEvent(
                session = 1,
                sequence = 2,
                title = "Song",
                trackId = "track-1",
                artwork = "https://example.test/cover.webp",
            ),
        )
        reducer.reduce(playbackEvent(session = 1, sequence = 3, active = false, rawState = 3))

        val resumed = reducer.reduce(
            playbackEvent(session = 2, sequence = 4, active = true, rawState = 2),
        )
        val activation = resumed.mutations.single() as XiaomiPlaybackMutation.Activate
        assertTrue(activation.newSession)

        val metadata = reducer.reduce(
            mediaEvent(
                session = 2,
                sequence = 5,
                title = "Song",
                trackId = "track-1",
                artwork = null,
            ),
        )
        val applied = metadata.mutations.single() as XiaomiPlaybackMutation.ApplyMediaInfo

        assertFalse(applied.replaceTrack)
        assertEquals("https://example.test/cover.webp", applied.mediaInfo.artworkUrl)
    }

    @Test
    fun mediaInfoPreservesOfficialTrackChangeDirectionType() {
        val reducer = XiaomiPlaybackReducer()
        reducer.reduce(playbackEvent(session = 1, sequence = 1, active = true, rawState = 2))

        val metadata = reducer.reduce(
            mediaEvent(
                session = 1,
                sequence = 2,
                title = "Previous song",
                trackId = "track-previous",
                artwork = null,
                metadataChangeType = 2,
            ),
        )
        val applied = metadata.mutations.single() as XiaomiPlaybackMutation.ApplyMediaInfo

        assertEquals(2, applied.mediaInfo.metadataChangeType)
    }

    private fun playbackEvent(
        session: Long,
        sequence: Long,
        active: Boolean,
        rawState: Int,
    ): WindowsBridgeXiaomiEvent = WindowsBridgeXiaomiEvent(
        eventName = "playback_state",
        payload = buildJsonObject {
            put("session_active", JsonPrimitive(active))
            put("raw_state", JsonPrimitive(rawState))
        },
        bridgeSequence = sequence,
        sessionSequence = session,
        rawJson = "{}",
    )

    private fun mediaEvent(
        session: Long,
        sequence: Long,
        title: String,
        trackId: String,
        artwork: String?,
        metadataChangeType: Int? = null,
    ): WindowsBridgeXiaomiEvent = WindowsBridgeXiaomiEvent(
        eventName = "media_info",
        payload = buildJsonObject {
            put("title", JsonPrimitive(title))
            put("track_id", JsonPrimitive(trackId))
            artwork?.let { put("artwork_url", JsonPrimitive(it)) }
            metadataChangeType?.let {
                put("metadata_change_type", JsonPrimitive(it))
            }
        },
        bridgeSequence = sequence,
        sessionSequence = session,
        rawJson = "{}",
    )
}
