package com.airplayreceiver.desktop.backend

/**
 * Keeps every connected source's last media snapshot while exposing only the
 * foreground source through AppState.playback/remoteControl.
 */
internal object SourcePlaybackProjection {
    fun playback(
        current: AppState,
        source: MediaSource,
    ): PlaybackSnapshot =
        current.sourcePlaybackStates[source]?.playback
            ?: if (
                current.activeMediaSource == source ||
                (
                    current.activeMediaSource == null &&
                        sourceForProtocol(current.playback.protocol) == source
                    )
            ) {
                current.playback
            } else {
                emptyPlayback(source)
            }

    fun cachePlayback(
        current: AppState,
        source: MediaSource,
        playback: PlaybackSnapshot,
    ): AppState {
        val existing = current.sourcePlaybackStates[source]
            ?: SourcePlaybackState()
        val updatedSources = current.sourcePlaybackStates + (
            source to existing.copy(playback = playback)
            )
        return if (current.activeMediaSource == source) {
            current.copy(
                playback = playback,
                sourcePlaybackStates = updatedSources,
            )
        } else {
            current.copy(sourcePlaybackStates = updatedSources)
        }
    }

    fun updatePlayback(
        current: AppState,
        source: MediaSource,
        transform: (PlaybackSnapshot) -> PlaybackSnapshot,
    ): AppState = cachePlayback(
        current = current,
        source = source,
        playback = transform(playback(current, source)),
    )

    fun cacheRemoteControl(
        current: AppState,
        source: MediaSource,
        remoteControl: RemoteControlState,
    ): AppState {
        val existing = current.sourcePlaybackStates[source]
            ?: SourcePlaybackState(playback = playback(current, source))
        val updatedSources = current.sourcePlaybackStates + (
            source to existing.copy(remoteControl = remoteControl)
            )
        return if (current.activeMediaSource == source) {
            current.copy(
                remoteControl = remoteControl,
                sourcePlaybackStates = updatedSources,
            )
        } else {
            current.copy(sourcePlaybackStates = updatedSources)
        }
    }

    fun activate(
        current: AppState,
        source: MediaSource,
    ): AppState {
        val sourceState = current.sourcePlaybackStates[source]
            ?: SourcePlaybackState(playback = playback(current, source))
        val updatedSources = current.sourcePlaybackStates + (
            source to sourceState
            )
        return current.copy(
            activeMediaSource = source,
            playback = sourceState.playback,
            remoteControl = sourceState.remoteControl,
            sourcePlaybackStates = updatedSources,
        )
    }

    fun markPaused(
        current: AppState,
        source: MediaSource,
    ): AppState = updatePlayback(current, source) {
        it.copy(isPlaying = false)
    }

    fun remove(
        current: AppState,
        source: MediaSource,
    ): AppState {
        val updatedSources = current.sourcePlaybackStates - source
        return if (current.activeMediaSource == source) {
            current.copy(
                playback = PlaybackSnapshot(),
                remoteControl = RemoteControlState(),
                activeMediaSource = null,
                selectedCoreMediaSource = current.selectedCoreMediaSource
                    .takeUnless { it == source },
                sourcePlaybackStates = updatedSources,
            )
        } else {
            current.copy(
                selectedCoreMediaSource = current.selectedCoreMediaSource
                    .takeUnless { it == source },
                sourcePlaybackStates = updatedSources,
            )
        }
    }

    fun sourceForProtocol(protocol: String?): MediaSource? = when {
        protocol.equals("AirPlay", ignoreCase = true) -> MediaSource.AIRPLAY
        protocol.equals("DLNA", ignoreCase = true) -> MediaSource.DLNA
        protocol.equals(XIAOMI_PROTOCOL, ignoreCase = true) ->
            MediaSource.XIAOMI_MIPLAY

        else -> null
    }

    fun sourceForCoreId(source: String?): MediaSource? =
        when (source?.trim()?.lowercase()) {
            "airplay" -> MediaSource.AIRPLAY
            "dlna" -> MediaSource.DLNA
            "xiaomi_miplay" -> MediaSource.XIAOMI_MIPLAY
            else -> null
        }

    private fun emptyPlayback(source: MediaSource): PlaybackSnapshot =
        PlaybackSnapshot(
            protocol = when (source) {
                MediaSource.AIRPLAY -> "AirPlay"
                MediaSource.DLNA -> "DLNA"
                MediaSource.XIAOMI_MIPLAY -> XIAOMI_PROTOCOL
            },
        )
}
