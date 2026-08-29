package com.fusionplay.android.media

import org.junit.Assert.assertFalse
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class LatestRequestGateTest {
    @Test
    fun onlyTheNewestRequestCanCommit() {
        val gate = LatestRequestGate()
        val first = gate.begin()
        val second = gate.begin()
        var commits = 0

        assertFalse(gate.commitIfCurrent(first) { commits++ })
        assertTrue(gate.commitIfCurrent(second) { commits++ })
        assertEquals(1, commits)
    }

    @Test
    fun invalidationRejectsAnInFlightRequest() {
        val gate = LatestRequestGate()
        val request = gate.begin()

        gate.invalidate()

        assertFalse(gate.commitIfCurrent(request) { error("stale request committed") })
    }
}
