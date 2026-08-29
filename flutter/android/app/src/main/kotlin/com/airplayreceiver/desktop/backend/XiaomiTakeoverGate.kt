package com.airplayreceiver.desktop.backend

/**
 * Rejects the stale "playing" observation that can race with a pause sent
 * during source takeover. A confirmed pause, an explicit resume, or a new
 * Xiaomi session opens the gate again.
 */
class XiaomiTakeoverGate(
    private val monotonicMillis: () -> Long = {
        System.nanoTime() / 1_000_000L
    },
) {
    private var suppressedUntilMillis: Long? = null

    @Synchronized
    fun arm(sourceWasPlaying: Boolean) {
        if (sourceWasPlaying) {
            suppressedUntilMillis = monotonicMillis() + TAKEOVER_SETTLING_MILLIS
        }
    }

    @Synchronized
    fun acceptPlaying(): Boolean {
        val deadline = suppressedUntilMillis ?: return true
        if (monotonicMillis() >= deadline) {
            suppressedUntilMillis = null
            return true
        }
        // A replacement MiPlay media session is not automatically a fresh
        // user intent. Track/session churn can race with AirPlay takeover and
        // used to reopen output immediately after the pause command.
        return false
    }

    @Synchronized
    fun confirmPaused() {
        suppressedUntilMillis = null
    }

    @Synchronized
    fun explicitResume() {
        suppressedUntilMillis = null
    }

    @Synchronized
    fun reset() {
        suppressedUntilMillis = null
    }

    private companion object {
        const val TAKEOVER_SETTLING_MILLIS = 1_500L
    }
}
