package com.airplayreceiver.desktop.bridge

/**
 * Reduces Xiaomi's two asynchronous event streams into ordered playback
 * mutations. New bridges attach monotonic bridge/session sequences. Older
 * Rust sidecars do not, so this reducer also synthesizes a session at the
 * inactive -> active boundary and reserves that same session for metadata
 * which arrives just before playback_state.
 */
class XiaomiPlaybackReducer(
    private val nowMillis: () -> Long = {
        System.nanoTime() / 1_000_000L
    },
) {
    private var lastEventSequence = 0L
    private var currentSessionSequence = 0L
    private var sessionActive = false
    private var currentMediaInfo: XiaomiMediaInfo? = null
    private var pendingMediaInfo: PendingMediaInfo? = null
    private var pendingSeek: PendingSeek? = null

    /**
     * Records a seek accepted by the native MiPlay control channel. Position
     * updates arrive independently from the command channel, so
     * one or two already-sampled positions can arrive after the command and
     * otherwise snap the UI back to the old position.
     *
     * Keep the requested position until the vendor reports a nearby position,
     * a different track/session starts, or the short acknowledgement window
     * expires. This is an ordering barrier only; it does not turn dispatch
     * into a confirmed command result.
     */
    fun beginSeek(positionMs: Long) {
        if (!sessionActive || currentSessionSequence <= 0L) {
            return
        }
        val target = positionMs.coerceAtLeast(0L)
        pendingSeek = PendingSeek(
            sessionSequence = currentSessionSequence,
            targetPositionMs = target,
            startedAtMs = nowMillis(),
        )
        currentMediaInfo = currentMediaInfo?.copy(positionMs = target)
    }

    fun reduce(event: WindowsBridgeXiaomiEvent): XiaomiReduction {
        val playbackState = event.toPlaybackStateOrNull()
        val mediaInfo = event.toMediaInfoOrNull()
        if (playbackState == null && mediaInfo == null) {
            return XiaomiReduction.ignored(
                reason = "unsupported_or_invalid_event",
                eventSequence = event.bridgeSequence,
                sessionSequence = event.sessionSequence,
            )
        }

        val eventSequence =
            playbackState?.eventSequence ?: mediaInfo?.eventSequence
        if (
            eventSequence != null &&
            eventSequence <= lastEventSequence
        ) {
            return XiaomiReduction.ignored(
                reason = "stale_event_sequence",
                eventSequence = eventSequence,
                sessionSequence =
                    playbackState?.sessionSequence ?: mediaInfo?.sessionSequence,
            )
        }
        if (eventSequence != null) {
            lastEventSequence = eventSequence
        }

        return if (playbackState != null) {
            reducePlaybackState(playbackState)
        } else {
            reduceMediaInfo(requireNotNull(mediaInfo))
        }
    }

    fun reset(resetEventSequence: Boolean = false): XiaomiReduction {
        val oldSession = currentSessionSequence.takeIf { it > 0L }
        if (resetEventSequence) {
            lastEventSequence = 0L
        }
        currentSessionSequence = 0L
        sessionActive = false
        currentMediaInfo = null
        pendingMediaInfo = null
        pendingSeek = null
        return XiaomiReduction(
            mutations = oldSession
                ?.let { listOf(XiaomiPlaybackMutation.Deactivate(it)) }
                .orEmpty(),
            outcome = XiaomiReductionOutcome.APPLIED,
            reason = "reducer_reset",
            eventSequence = null,
            sessionSequence = oldSession,
        )
    }

    private fun reducePlaybackState(
        playbackState: XiaomiPlaybackState,
    ): XiaomiReduction {
        val explicitSession = playbackState.sessionSequence
        val resolvedSession = if (playbackState.sessionActive) {
            explicitSession ?: resolveLegacyActiveSession()
        } else {
            explicitSession
                ?: currentSessionSequence.takeIf { it > 0L }
                ?: return XiaomiReduction.ignored(
                    reason = "duplicate_inactive",
                    eventSequence = playbackState.eventSequence,
                    sessionSequence = null,
                )
        }

        if (resolvedSession < currentSessionSequence) {
            return XiaomiReduction.ignored(
                reason = "stale_session",
                eventSequence = playbackState.eventSequence,
                sessionSequence = resolvedSession,
            )
        }

        if (!playbackState.sessionActive) {
            if (
                !sessionActive ||
                resolvedSession != currentSessionSequence
            ) {
                return XiaomiReduction.ignored(
                    reason = "duplicate_or_future_inactive",
                    eventSequence = playbackState.eventSequence,
                    sessionSequence = resolvedSession,
                )
            }
            sessionActive = false
            pendingSeek = null
            pendingMediaInfo = pendingMediaInfo
                ?.takeIf { it.sessionSequence > resolvedSession }
            return XiaomiReduction(
                mutations = listOf(
                    XiaomiPlaybackMutation.Deactivate(resolvedSession),
                ),
                outcome = XiaomiReductionOutcome.APPLIED,
                reason = "session_inactive",
                eventSequence = playbackState.eventSequence,
                sessionSequence = resolvedSession,
            )
        }

        val startsNewSession =
            !sessionActive || resolvedSession > currentSessionSequence
        if (startsNewSession) {
            currentSessionSequence = resolvedSession
            sessionActive = true
            pendingSeek = null
        }

        val mutations = mutableListOf<XiaomiPlaybackMutation>(
            XiaomiPlaybackMutation.Activate(
                sessionSequence = resolvedSession,
                sourceName = playbackState.sourceName,
                rawState = playbackState.rawState,
                newSession = startsNewSession,
            ),
        )
        val pending = pendingMediaInfo
            ?.takeIf { it.sessionSequence == resolvedSession }
        if (pending != null) {
            pendingMediaInfo = null
            currentMediaInfo = pending.mediaInfo
            mutations += XiaomiPlaybackMutation.ApplyMediaInfo(
                sessionSequence = resolvedSession,
                mediaInfo = pending.mediaInfo,
                replaceTrack = pending.replaceTrack,
            )
        } else {
            pendingMediaInfo = pendingMediaInfo
                ?.takeIf { it.sessionSequence > resolvedSession }
        }

        return XiaomiReduction(
            mutations = mutations,
            outcome = XiaomiReductionOutcome.APPLIED,
            reason = if (startsNewSession) {
                if (pending == null) {
                    "session_activated"
                } else {
                    "session_activated_with_cached_media"
                }
            } else {
                "active_state_updated"
            },
            eventSequence = playbackState.eventSequence,
            sessionSequence = resolvedSession,
        )
    }

    private fun reduceMediaInfo(mediaInfo: XiaomiMediaInfo): XiaomiReduction {
        val explicitSession = mediaInfo.sessionSequence
        val resolvedSession = explicitSession ?: when {
            sessionActive -> currentSessionSequence
            pendingMediaInfo != null ->
                requireNotNull(pendingMediaInfo).sessionSequence

            else -> currentSessionSequence + 1L
        }

        if (
            explicitSession != null &&
            resolvedSession < currentSessionSequence
        ) {
            return XiaomiReduction.ignored(
                reason = "stale_session",
                eventSequence = mediaInfo.eventSequence,
                sessionSequence = resolvedSession,
            )
        }

        if (!sessionActive || resolvedSession > currentSessionSequence) {
            if (
                explicitSession != null &&
                resolvedSession <= currentSessionSequence
            ) {
                return XiaomiReduction.ignored(
                    reason = "late_media_for_inactive_session",
                    eventSequence = mediaInfo.eventSequence,
                    sessionSequence = resolvedSession,
                )
            }
            val previous = pendingMediaInfo
                ?.takeIf { it.sessionSequence == resolvedSession }
                ?.mediaInfo
                ?: currentMediaInfo
            val replaceTrack = isNewTrack(previous, mediaInfo)
            val merged = mergeMediaInfo(previous, mediaInfo, replaceTrack)
            pendingMediaInfo = PendingMediaInfo(
                sessionSequence = resolvedSession,
                mediaInfo = merged,
                replaceTrack = replaceTrack ||
                    pendingMediaInfo
                        ?.takeIf { it.sessionSequence == resolvedSession }
                        ?.replaceTrack == true,
            )
            return XiaomiReduction(
                mutations = emptyList(),
                outcome = XiaomiReductionOutcome.CACHED,
                reason = "media_cached_until_active",
                eventSequence = mediaInfo.eventSequence,
                sessionSequence = resolvedSession,
            )
        }

        if (resolvedSession < currentSessionSequence) {
            return XiaomiReduction.ignored(
                reason = "stale_session",
                eventSequence = mediaInfo.eventSequence,
                sessionSequence = resolvedSession,
            )
        }

        val replaceTrack = isNewTrack(currentMediaInfo, mediaInfo)
        val reconciledMediaInfo = reconcilePendingSeek(
            incoming = mediaInfo,
            replaceTrack = replaceTrack,
            sessionSequence = resolvedSession,
        )
        val mergedMediaInfo = mergeMediaInfo(
            currentMediaInfo,
            reconciledMediaInfo,
            replaceTrack,
        )
        currentMediaInfo = mergedMediaInfo
        return XiaomiReduction(
            mutations = listOf(
                XiaomiPlaybackMutation.ApplyMediaInfo(
                    sessionSequence = resolvedSession,
                    mediaInfo = mergedMediaInfo,
                    replaceTrack = replaceTrack,
                ),
            ),
            outcome = XiaomiReductionOutcome.APPLIED,
            reason = if (replaceTrack) {
                "new_track_applied"
            } else {
                "media_update_applied"
            },
            eventSequence = mediaInfo.eventSequence,
            sessionSequence = resolvedSession,
        )
    }

    private fun resolveLegacyActiveSession(): Long {
        if (sessionActive && currentSessionSequence > 0L) {
            return currentSessionSequence
        }
        return pendingMediaInfo
            ?.sessionSequence
            ?.takeIf { it > currentSessionSequence }
            ?: (currentSessionSequence + 1L)
    }

    private fun isNewTrack(
        previous: XiaomiMediaInfo?,
        incoming: XiaomiMediaInfo,
    ): Boolean {
        val hasIncomingIdentity =
            incoming.trackId != null ||
                incoming.title != null ||
                incoming.artist != null ||
                incoming.album != null
        if (!hasIncomingIdentity) {
            return false
        }
        if (previous == null) {
            return true
        }
        if (
            incoming.trackId != null &&
            previous.trackId != null &&
            incoming.trackId != previous.trackId &&
            // A legacy sidecar may first synthesize `metadata:*` from the
            // title and then publish the phone's real mAudioId in a partial
            // update. Treat that as identity promotion, not a second song,
            // unless the human-readable metadata also changed.
            !(incoming.trackId.isSyntheticMetadataIdentity() xor
                previous.trackId.isSyntheticMetadataIdentity())
        ) {
            return true
        }
        if (
            incoming.title != null &&
            !incoming.title.equals(previous.title)
        ) {
            return true
        }
        if (
            incoming.title != null &&
            incoming.artist != null &&
            previous.artist != null &&
            !incoming.artist.equals(previous.artist)
        ) {
            return true
        }
        return incoming.title != null &&
            incoming.album != null &&
            previous.album != null &&
            !incoming.album.equals(previous.album)
    }

    private fun String.isSyntheticMetadataIdentity(): Boolean =
        startsWith("metadata:")

    private fun mergeMediaInfo(
        previous: XiaomiMediaInfo?,
        incoming: XiaomiMediaInfo,
        replaceTrack: Boolean,
    ): XiaomiMediaInfo {
        if (previous == null || replaceTrack) {
            return incoming
        }
        return incoming.copy(
            trackId = incoming.trackId ?: previous.trackId,
            title = incoming.title ?: previous.title,
            artist = incoming.artist ?: previous.artist,
            album = incoming.album ?: previous.album,
            artworkUrl = incoming.artworkUrl ?: previous.artworkUrl,
            durationMs = incoming.durationMs ?: previous.durationMs,
            positionMs = incoming.positionMs ?: previous.positionMs,
            codec = incoming.codec ?: previous.codec,
            bitrateBps = incoming.bitrateBps ?: previous.bitrateBps,
            sampleRate = incoming.sampleRate ?: previous.sampleRate,
            bitsPerSample =
                incoming.bitsPerSample ?: previous.bitsPerSample,
            channels = incoming.channels ?: previous.channels,
            metadataChangeType =
                incoming.metadataChangeType ?: previous.metadataChangeType,
        )
    }

    private fun reconcilePendingSeek(
        incoming: XiaomiMediaInfo,
        replaceTrack: Boolean,
        sessionSequence: Long,
    ): XiaomiMediaInfo {
        val pending = pendingSeek ?: return incoming
        if (
            replaceTrack ||
            pending.sessionSequence != sessionSequence
        ) {
            pendingSeek = null
            return incoming
        }

        val elapsedMs = (nowMillis() - pending.startedAtMs)
            .coerceAtLeast(0L)
        if (elapsedMs >= SEEK_ACKNOWLEDGEMENT_WINDOW_MS) {
            pendingSeek = null
            return incoming
        }

        val position = incoming.positionMs
        if (
            position != null &&
            kotlin.math.abs(position - pending.targetPositionMs) <=
            SEEK_CONFIRMATION_TOLERANCE_MS
        ) {
            pendingSeek = null
            return incoming
        }

        return incoming.copy(
            positionMs = pending.targetPositionMs,
        )
    }

    private data class PendingMediaInfo(
        val sessionSequence: Long,
        val mediaInfo: XiaomiMediaInfo,
        val replaceTrack: Boolean,
    )

    private data class PendingSeek(
        val sessionSequence: Long,
        val targetPositionMs: Long,
        val startedAtMs: Long,
    )

    private companion object {
        const val SEEK_CONFIRMATION_TOLERANCE_MS = 3_000L
        const val SEEK_ACKNOWLEDGEMENT_WINDOW_MS = 5_000L
    }
}

sealed interface XiaomiPlaybackMutation {
    data class Activate(
        val sessionSequence: Long,
        val sourceName: String?,
        val rawState: Int?,
        val newSession: Boolean,
    ) : XiaomiPlaybackMutation

    data class ApplyMediaInfo(
        val sessionSequence: Long,
        val mediaInfo: XiaomiMediaInfo,
        val replaceTrack: Boolean,
    ) : XiaomiPlaybackMutation

    data class Deactivate(
        val sessionSequence: Long,
    ) : XiaomiPlaybackMutation
}

enum class XiaomiReductionOutcome {
    APPLIED,
    CACHED,
    IGNORED,
}

data class XiaomiReduction(
    val mutations: List<XiaomiPlaybackMutation>,
    val outcome: XiaomiReductionOutcome,
    val reason: String,
    val eventSequence: Long?,
    val sessionSequence: Long?,
) {
    companion object {
        fun ignored(
            reason: String,
            eventSequence: Long?,
            sessionSequence: Long?,
        ) = XiaomiReduction(
            mutations = emptyList(),
            outcome = XiaomiReductionOutcome.IGNORED,
            reason = reason,
            eventSequence = eventSequence,
            sessionSequence = sessionSequence,
        )
    }
}
