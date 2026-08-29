package com.airplayreceiver.desktop.backend

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class AirPlayMetadataContinuityTest {
    @Test
    fun trackTakeoverKeepsVisualMetadataAndControlsUntilFreshEventsArrive() {
        val playback = PlaybackSnapshot(
            title = "Old title",
            artist = "Old artist",
            album = "Old album",
            coverArt = "/old-cover.png",
            protocol = "AirPlay",
            durationMs = 180_000,
            positionMs = 72_000,
            isPlaying = true,
            streamActive = true,
            sourceEpoch = 40,
        )
        val controls = RemoteControlState(
            available = true,
            commands = setOf("play", "pause", "previous_track", "next_track"),
        )
        val current = AppState(
            activeMediaSource = MediaSource.AIRPLAY,
            selectedCoreMediaSource = MediaSource.AIRPLAY,
            playback = playback,
            remoteControl = controls,
            sourcePlaybackStates = mapOf(
                MediaSource.AIRPLAY to SourcePlaybackState(playback, controls),
            ),
        )

        val takeover = AppStateTransitions.sourceTakeover(
            current,
            AppEvent.SourceTakeover(
                source = "airplay",
                mediaKind = "audio",
                epoch = 41,
                previousSource = "airplay",
                previousMediaKind = "audio",
                previousEpoch = 40,
                reason = "airplay_audio_replacement",
                rawJson = "{}",
            ),
        )
        val metadata = AppStateTransitions.nowPlaying(
            takeover,
            AppEvent.NowPlaying(
                title = "New title",
                artist = "New artist",
                album = "New album",
                genre = null,
                durationMs = 200_000,
                rawJson = "{}",
                source = "airplay",
                epoch = 41,
            ),
        )

        assertEquals("New title", metadata.playback.title)
        assertEquals("/old-cover.png", metadata.playback.coverArt)
        assertEquals(controls, metadata.remoteControl)
        assertTrue(metadata.remoteControl.available)
    }
}
