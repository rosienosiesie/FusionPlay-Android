package com.airplayreceiver.desktop.backend

import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Test

class MediaSourceArbiterTest {
    @Test
    fun airPlayToMiPlaySilencesAirPlayBeforeCommittingMiPlay() = runBlocking {
        val arbiter = MediaSourceArbiter()
        arbiter.activate(MediaSource.AIRPLAY) {}
        val observations = mutableListOf<String>()

        arbiter.activate(MediaSource.XIAOMI_MIPLAY) { transition ->
            assertEquals(MediaSource.AIRPLAY, transition.previous)
            observations += "airplay_paused"
        }
        observations += "miplay_active"

        assertEquals(
            listOf("airplay_paused", "miplay_active"),
            observations,
        )
        assertEquals(MediaSource.XIAOMI_MIPLAY, arbiter.current())
    }

    @Test
    fun miPlayToAirPlaySilencesMiPlayBeforeCommittingAirPlay() = runBlocking {
        val arbiter = MediaSourceArbiter()
        arbiter.activate(MediaSource.XIAOMI_MIPLAY) {}
        val observations = mutableListOf<String>()

        arbiter.activate(MediaSource.AIRPLAY) { transition ->
            assertEquals(MediaSource.XIAOMI_MIPLAY, transition.previous)
            observations += "miplay_paused"
        }
        observations += "airplay_active"

        assertEquals(
            listOf("miplay_paused", "airplay_active"),
            observations,
        )
        assertEquals(MediaSource.AIRPLAY, arbiter.current())
    }

    @Test
    fun coreTakeoverFromMiPlayAlwaysRequiresMiPlaySuspension() {
        assertEquals(
            true,
            SourceTakeoverPolicy.shouldSuspend(
                previousSource = "xiaomi_miplay",
                newSource = "airplay",
                candidateSource = "xiaomi_miplay",
            ),
        )
    }
}
