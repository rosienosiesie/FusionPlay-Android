package com.airplayreceiver.desktop.backend

import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

enum class MediaSource {
    AIRPLAY,
    DLNA,
    XIAOMI_MIPLAY,
}

internal object SourceTakeoverPolicy {
    fun shouldSuspend(
        previousSource: String?,
        newSource: String,
        candidateSource: String,
    ): Boolean {
        return previousSource.equals(candidateSource, ignoreCase = true) &&
            !newSource.equals(candidateSource, ignoreCase = true)
    }
}

/**
 * Matches decoder-control events against the session selected directly by
 * the media event collector.
 *
 * Core events and AppViewModel projections are consumed by independent
 * SharedFlow collectors. A DLNA Seek commonly follows Play immediately, so
 * consulting the asynchronously reduced AppViewModel state can reject the
 * valid Seek and leave the decoder at zero. The decoder collector therefore
 * tracks its own source/epoch and uses this gate for ordered follow-up events.
 */
internal object NetworkMediaEventGate {
    fun selectsCurrentResource(
        activeSource: MediaSource?,
        activeUrl: String?,
        activeEpoch: Long?,
        eventSource: MediaSource,
        eventUrl: String,
        eventEpoch: Long?,
    ): Boolean {
        return activeSource == eventSource &&
            activeUrl == eventUrl &&
            (
                eventEpoch == null ||
                    activeEpoch == null ||
                    eventEpoch == activeEpoch
                )
    }

    fun matches(
        activeSource: MediaSource?,
        activeEpoch: Long?,
        expectedSource: MediaSource,
        expectedWireSource: String,
        eventSource: String?,
        eventEpoch: Long?,
    ): Boolean {
        if (activeSource != expectedSource) {
            return false
        }
        if (
            eventSource != null &&
            !eventSource.equals(expectedWireSource, ignoreCase = true)
        ) {
            return false
        }
        return eventEpoch == null ||
            activeEpoch == null ||
            eventEpoch == activeEpoch
    }
}

data class MediaSourceSwitch(
    val previous: MediaSource?,
    val current: MediaSource,
)

/**
 * Serializes source takeovers so the last observed projection wins.
 *
 * The switch callback runs while the lock is held. It must first silence the
 * previous source, then return; only after that is [currentSource] committed.
 */
class MediaSourceArbiter {
    private val mutex = Mutex()
    private var currentSource: MediaSource? = null

    suspend fun activate(
        source: MediaSource,
        switchSource: suspend (MediaSourceSwitch) -> Unit,
    ): Boolean = mutex.withLock {
        if (currentSource == source) {
            return@withLock false
        }
        val transition = MediaSourceSwitch(
            previous = currentSource,
            current = source,
        )
        switchSource(transition)
        currentSource = source
        true
    }

    suspend fun deactivate(
        source: MediaSource,
        onDeactivated: suspend () -> Unit = {},
    ): Boolean = mutex.withLock {
        if (currentSource != source) {
            return@withLock false
        }
        onDeactivated()
        currentSource = null
        true
    }

    /**
     * Silences a source reported by the core even if the local projection
     * state has already moved on. The callback is intentionally unconditional:
     * a core takeover event is authoritative and must not be dropped because
     * an earlier UI event made [currentSource] temporarily stale.
     *
     * If [source] is still current, it is cleared after the callback. A
     * different current source is preserved.
     */
    suspend fun suspendObserved(
        source: MediaSource,
        onSuspended: suspend () -> Unit,
    ): Boolean = mutex.withLock {
        onSuspended()
        if (currentSource != source) {
            return@withLock false
        }
        currentSource = null
        true
    }

    suspend fun current(): MediaSource? = mutex.withLock { currentSource }
}
