package com.airplayreceiver.desktop.backend

/**
 * Decides whether a playback-state event is still the transport state exposed
 * by [AppState].
 *
 * Core events are reduced before they are forwarded to the playback side-effect
 * collector. That collector can suspend while changing the active decoder, so
 * a newer event may already have been reduced by the time an older event is
 * handled there. Source/epoch matching alone cannot distinguish
 * play -> pause -> play within one session. Comparing the reduced transport
 * value prevents the delayed pause side effect from overwriting the latest
 * resume. A playing event must also still own the foreground projection.
 */
internal object CorePlaybackStateSideEffect {
    fun isCurrent(
        current: AppState,
        event: AppEvent.PlaybackState,
    ): Boolean {
        val source = SourcePlaybackProjection.sourceForCoreId(event.source)
            ?: return false
        val playback = SourcePlaybackProjection.playback(current, source)
        if (
            event.epoch != null &&
            playback.sourceEpoch != null &&
            event.epoch != playback.sourceEpoch
        ) {
            return false
        }
        if (playback.isPlaying != event.playing) {
            return false
        }
        return !event.playing || current.activeMediaSource == source
    }
}
