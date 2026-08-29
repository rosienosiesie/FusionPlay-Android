package com.airplayreceiver.desktop.backend

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class XiaomiTakeoverGateTest {
    @Test
    fun replacementSessionCannotReopenMiPlayDuringAirPlayTakeover() {
        var nowMillis = 10_000L
        val gate = XiaomiTakeoverGate { nowMillis }

        gate.arm(sourceWasPlaying = true)

        assertFalse(gate.acceptPlaying())
        nowMillis += 1_499L
        assertFalse(gate.acceptPlaying())
        nowMillis += 1L
        assertTrue(gate.acceptPlaying())
    }

    @Test
    fun confirmedPauseReleasesTakeoverSuppression() {
        val gate = XiaomiTakeoverGate { 10_000L }

        gate.arm(sourceWasPlaying = true)
        gate.confirmPaused()

        assertTrue(gate.acceptPlaying())
    }

    @Test
    fun idleMiPlaySourceIsNotSuppressed() {
        val gate = XiaomiTakeoverGate { 10_000L }

        gate.arm(sourceWasPlaying = false)

        assertTrue(gate.acceptPlaying())
    }
}
