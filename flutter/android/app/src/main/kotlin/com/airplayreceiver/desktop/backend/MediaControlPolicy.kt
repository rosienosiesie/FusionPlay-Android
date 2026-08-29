package com.airplayreceiver.desktop.backend

data class MediaControlAvailability(
    val canPlayPause: Boolean,
    val canPrevious: Boolean,
    val canNext: Boolean,
    val canSeek: Boolean,
)

/**
 * One capability policy shared by the in-app controls, keyboard shortcuts,
 * notification actions and Android MediaSession.
 */
fun AppState.mediaControlAvailability(
    controlsLocalMedia: Boolean = false,
): MediaControlAvailability {
    val source = activeMediaSource ?: when {
        playback.protocol.equals("AirPlay", ignoreCase = true) ->
            MediaSource.AIRPLAY

        playback.protocol.equals("DLNA", ignoreCase = true) ->
            MediaSource.DLNA

        playback.protocol.equals(XIAOMI_PROTOCOL, ignoreCase = true) ->
            MediaSource.XIAOMI_MIPLAY

        else -> null
    }
    val hasMiPlaySession = source == MediaSource.XIAOMI_MIPLAY &&
        (
            activeMediaSource == MediaSource.XIAOMI_MIPLAY ||
                playback.streamActive ||
                playback.title != null ||
                playback.artist != null ||
                playback.durationMs != null
            )
    val commands = remoteControl.commands
    val remoteAvailable = remoteControl.available
    val remoteCanPlayPause = remoteAvailable &&
        (
            "play_pause" in commands ||
                "play" in commands ||
                "pause" in commands
            )

    return MediaControlAvailability(
        canPlayPause = controlsLocalMedia || hasMiPlaySession || remoteCanPlayPause,
        canPrevious = hasMiPlaySession ||
            (remoteAvailable && "previous_track" in commands),
        canNext = hasMiPlaySession ||
            (remoteAvailable && "next_track" in commands),
        canSeek = (playback.durationMs ?: 0L) > 0L &&
            (
                controlsLocalMedia ||
                    hasMiPlaySession ||
                    (remoteAvailable && "seek" in commands)
                ),
    )
}

fun PlaybackSnapshot.toggleCommand(): PlaybackCommand =
    if (isPlaying) PlaybackCommand.PAUSE else PlaybackCommand.PLAY
