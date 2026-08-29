package com.airplayreceiver.desktop.backend

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AirPlayPauseStateTest {
    @Test
    fun pausedAirPlayStreamRestartDoesNotFlashBackToPlaying() {
        val playback = PlaybackSnapshot(
            title = "Paused track",
            artist = "Artist",
            protocol = "AirPlay",
            durationMs = 180_000,
            positionMs = 52_000,
            isPlaying = false,
            streamActive = false,
            sourceEpoch = 40,
        )
        val controls = RemoteControlState(
            available = true,
            commands = setOf("play", "pause", "play_pause"),
            transport = "airplay2_mediaremote_experimental",
        )
        val current = AppState(
            activeMediaSource = MediaSource.AIRPLAY,
            selectedCoreMediaSource = MediaSource.AIRPLAY,
            playback = playback,
            remoteControl = controls,
            sourcePlaybackStates = mapOf(
                MediaSource.AIRPLAY to SourcePlaybackState(
                    playback = playback,
                    remoteControl = controls,
                ),
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
                reason = "airplay_audio_resume",
                rawJson = "{}",
            ),
        )
        val restarted = AppStateTransitions.streamStarted(
            takeover,
            AppEvent.StreamStarted(
                sourceCodec = "alac",
                sourceSampleRate = 44_100,
                sourceChannels = 2,
                sourceBits = 16,
                decodedSampleRate = 48_000,
                decodedChannels = 2,
                decodedBits = 32,
                rawJson = "{}",
                source = "airplay",
                epoch = 41,
            ),
        )

        assertFalse(restarted.playback.isPlaying)
        assertTrue(restarted.playback.streamActive)
        assertEquals(41L, restarted.playback.sourceEpoch)
        assertEquals("Paused track", restarted.playback.title)
        assertEquals(controls, restarted.remoteControl)
    }
}
