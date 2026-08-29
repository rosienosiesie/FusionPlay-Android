package com.airplayreceiver.desktop.backend

import java.util.Locale

/**
 * Pure state transitions shared by [AppViewModel] and backend unit tests.
 */
internal object AppStateTransitions {
    private const val PAUSED_ZERO_SENTINEL_TOLERANCE_MS = 1_500L
    private const val AIRPLAY_SOURCE = "AirPlay"
    private const val DLNA_SOURCE = "DLNA"

    fun streamStarted(
        current: AppState,
        event: AppEvent.StreamStarted,
    ): AppState {
        val previous = SourcePlaybackProjection.playback(
            current,
            MediaSource.AIRPLAY,
        )
        val resumesExistingStream = previous.hasRetainableAirPlayMedia()
        val base = if (resumesExistingStream) {
            previous
        } else {
            PlaybackSnapshot()
        }
        val cached = SourcePlaybackProjection.cachePlayback(
            current = current,
            source = MediaSource.AIRPLAY,
            playback = base.copy(
                mediaUrl = null,
                mediaKind = null,
                protocol = AIRPLAY_SOURCE,
                qualityText = streamQualityText(event)
                    ?: base.qualityText,
                streamActive = true,
                isPlaying = if (resumesExistingStream) {
                    base.isPlaying
                } else {
                    true
                },
                sourceEpoch = event.epoch ?: base.sourceEpoch,
            ),
        )
        val withEvent = cached.copy(
            selectedCoreMediaSource = MediaSource.AIRPLAY,
            lastEvent = event,
        )
        val withRemoteControl = if (resumesExistingStream) {
            withEvent
        } else {
            SourcePlaybackProjection.cacheRemoteControl(
                current = withEvent,
                source = MediaSource.AIRPLAY,
                remoteControl = RemoteControlState(),
            )
        }
        return SourcePlaybackProjection.activate(
            withRemoteControl,
            MediaSource.AIRPLAY,
        )
    }

    fun sourceTakeover(
        current: AppState,
        event: AppEvent.SourceTakeover,
    ): AppState {
        val source = SourcePlaybackProjection.sourceForCoreId(event.source)
            ?: return current.copy(lastEvent = event)
        val previous = SourcePlaybackProjection.playback(current, source)
        val continuesAirPlaySession =
            source == MediaSource.AIRPLAY &&
                previous.hasRetainableAirPlayMedia()
        val playback = if (
            source == MediaSource.AIRPLAY &&
            !continuesAirPlaySession
        ) {
            PlaybackSnapshot(
                mediaKind = event.mediaKind
                    ?.takeUnless {
                        it.equals("unknown", ignoreCase = true)
                    },
                protocol = AIRPLAY_SOURCE,
                sourceEpoch = event.epoch,
            )
        } else {
            previous.copy(
                mediaKind = event.mediaKind
                    ?.takeUnless {
                        it.equals("unknown", ignoreCase = true)
                    }
                    ?: previous.mediaKind,
                protocol = when (source) {
                    MediaSource.AIRPLAY -> AIRPLAY_SOURCE
                    MediaSource.DLNA -> DLNA_SOURCE
                    MediaSource.XIAOMI_MIPLAY -> XIAOMI_PROTOCOL
                },
                sourceEpoch = event.epoch,
            )
        }
        val cached = SourcePlaybackProjection.cachePlayback(
            current = current.copy(
                selectedCoreMediaSource = source,
                lastEvent = event,
            ),
            source = source,
            playback = playback,
        )
        return if (
            continuesAirPlaySession ||
            source == MediaSource.XIAOMI_MIPLAY
        ) {
            // Xiaomi transport controls are provided by WindowsBridge rather
            // than by the core. The external takeover event must not erase
            // the reverse-control capabilities cached immediately before it.
            cached
        } else {
            SourcePlaybackProjection.cacheRemoteControl(
                current = cached,
                source = source,
                remoteControl = RemoteControlState(),
            )
        }
    }

    /**
     * Records a connected Xiaomi session as paused and exposes its cached
     * metadata only when no other source is actively using the foreground.
     *
     * Keeping the decision and projection in one pure transition lets
     * [AppViewModel] apply it through a single atomic StateFlow update.
     */
    fun pauseConnectedXiaomiAndExposeIfForegroundIdle(
        current: AppState,
    ): AppState {
        val shouldExpose = shouldExposeConnectedXiaomiSession(
            activeSource = current.activeMediaSource,
            foregroundPlaying = current.playback.isPlaying,
        )
        val paused = SourcePlaybackProjection.markPaused(
            current = current,
            source = MediaSource.XIAOMI_MIPLAY,
        )
        return if (shouldExpose) {
            SourcePlaybackProjection.activate(
                current = paused,
                source = MediaSource.XIAOMI_MIPLAY,
            )
        } else {
            paused
        }
    }

    fun streamStopped(
        current: AppState,
        event: AppEvent.StreamStopped,
    ): AppState {
        if (!eventMatchesSource(current, MediaSource.AIRPLAY, event.epoch)) {
            return current.copy(lastEvent = event)
        }
        return SourcePlaybackProjection.updatePlayback(
            current = current.copy(lastEvent = event),
            source = MediaSource.AIRPLAY,
        ) {
            it.copy(
                streamActive = false,
                isPlaying = false,
            )
        }
    }

    fun airPlayClientDisconnected(
        current: AppState,
        event: AppEvent.ClientDisconnected,
    ): AppState {
        val disconnectedAddress = event.address?.trim()
        val connectedAddress = current.connectedClient?.trim()
        val disconnectsCurrentClient =
            disconnectedAddress.isNullOrEmpty() ||
                connectedAddress.isNullOrEmpty() ||
                disconnectedAddress.equals(
                    connectedAddress,
                    ignoreCase = true,
                )
        if (!disconnectsCurrentClient) {
            return current.copy(lastEvent = event)
        }
        return SourcePlaybackProjection.remove(
            current = current.copy(
                connectedClient = null,
                lastEvent = event,
            ),
            source = MediaSource.AIRPLAY,
        )
    }

    fun nowPlaying(
        current: AppState,
        event: AppEvent.NowPlaying,
    ): AppState {
        val source = eventSource(current, event.source)
            ?: return current.copy(lastEvent = event)
        if (!eventMatchesSource(current, source, event.epoch)) {
            return current.copy(lastEvent = event)
        }
        return SourcePlaybackProjection.updatePlayback(
            current = current.copy(lastEvent = event),
            source = source,
        ) { playback ->
            val title = event.title.nonBlankOrNull()
            val artist = event.artist.nonBlankOrNull()
            val album = event.album.nonBlankOrNull()
            val genre = event.genre.nonBlankOrNull()
            val trackChanged =
                title != null &&
                    playback.title != null &&
                    title != playback.title
            if (trackChanged) {
                playback.copy(
                    title = title,
                    artist = artist,
                    album = album,
                    genre = genre,
                    durationMs = event.durationMs?.takeIf { it > 0 },
                    positionMs = 0L,
                    sourceEpoch = event.epoch ?: playback.sourceEpoch,
                )
            } else {
                playback.copy(
                    title = title ?: playback.title,
                    artist = artist ?: playback.artist,
                    album = album ?: playback.album,
                    genre = genre ?: playback.genre,
                    durationMs = event.durationMs
                        ?.takeIf { it > 0 }
                        ?: playback.durationMs,
                    sourceEpoch = event.epoch ?: playback.sourceEpoch,
                )
            }
        }
    }

    fun playbackState(
        current: AppState,
        event: AppEvent.PlaybackState,
    ): AppState {
        val source = eventSource(current, event.source)
            ?: return current.copy(lastEvent = event)
        if (!eventMatchesSource(current, source, event.epoch)) {
            return current.copy(lastEvent = event)
        }
        val cached = SourcePlaybackProjection.updatePlayback(
            current = current.copy(lastEvent = event),
            source = source,
        ) { playback ->
            playback.copy(
                isPlaying = event.playing,
                streamActive = playback.streamActive || event.playing,
                sourceEpoch = event.epoch ?: playback.sourceEpoch,
            )
        }
        return if (event.playing) {
            SourcePlaybackProjection.activate(cached, source)
        } else {
            cached
        }
    }

    fun remoteControlAvailable(
        current: AppState,
        event: AppEvent.RemoteControlAvailable,
    ): AppState {
        val source = eventSource(current, event.source)
            ?: return current.copy(lastEvent = event)
        if (!eventMatchesSource(current, source, event.epoch)) {
            return current.copy(lastEvent = event)
        }
        val currentWithEpoch = SourcePlaybackProjection.updatePlayback(
            current = current.copy(lastEvent = event),
            source = source,
        ) {
            it.copy(sourceEpoch = event.epoch ?: it.sourceEpoch)
        }
        return SourcePlaybackProjection.cacheRemoteControl(
            current = currentWithEpoch,
            source = source,
            remoteControl = RemoteControlState(
                available = event.commands.isNotEmpty(),
                commands = event.commands,
                transport = event.transport,
                experimental = event.experimental,
            ),
        )
    }

    fun remoteControlUnavailable(
        current: AppState,
        event: AppEvent.RemoteControlUnavailable,
    ): AppState {
        val source = eventSource(current, event.source)
            ?: return current.copy(lastEvent = event)
        if (!eventMatchesSource(current, source, event.epoch)) {
            return current.copy(lastEvent = event)
        }
        val currentWithEpoch = SourcePlaybackProjection.updatePlayback(
            current = current.copy(lastEvent = event),
            source = source,
        ) {
            it.copy(sourceEpoch = event.epoch ?: it.sourceEpoch)
        }
        return SourcePlaybackProjection.cacheRemoteControl(
            current = currentWithEpoch,
            source = source,
            remoteControl = RemoteControlState(),
        )
    }

    fun progress(
        current: AppState,
        event: AppEvent.Progress,
    ): AppState {
        val source = SourcePlaybackProjection.sourceForCoreId(event.source)
            ?: current.selectedCoreMediaSource
            ?: current.activeMediaSource
            ?: SourcePlaybackProjection.sourceForProtocol(
                current.playback.protocol,
            )
        if (source == null) {
            val preservePausedPosition =
                !current.playback.isPlaying &&
                    event.positionMs <= 1L &&
                    current.playback.positionMs >
                    PAUSED_ZERO_SENTINEL_TOLERANCE_MS
            return current.copy(
                playback = current.playback.copy(
                    positionMs = if (preservePausedPosition) {
                        current.playback.positionMs
                    } else {
                        event.positionMs.coerceAtLeast(0)
                    },
                    durationMs = event.durationMs.takeIf { it > 0 }
                        ?: current.playback.durationMs,
                ),
                lastEvent = event,
            )
        }
        val playback = SourcePlaybackProjection.playback(current, source)
        if (
            event.epoch != null &&
            playback.sourceEpoch != null &&
            playback.sourceEpoch != event.epoch
        ) {
            return current.copy(lastEvent = event)
        }
        val preservePausedPosition =
            !playback.isPlaying &&
                event.positionMs <= 1L &&
                playback.positionMs > PAUSED_ZERO_SENTINEL_TOLERANCE_MS
        return SourcePlaybackProjection.cachePlayback(
            current = current.copy(lastEvent = event),
            source = source,
            playback = playback.copy(
                positionMs = if (preservePausedPosition) {
                    playback.positionMs
                } else {
                    event.positionMs.coerceAtLeast(0)
                },
                durationMs = event.durationMs.takeIf { it > 0 }
                    ?: playback.durationMs,
                sourceEpoch = event.epoch ?: playback.sourceEpoch,
            ),
        )
    }

    fun networkProgress(
        current: AppState,
        positionMs: Long,
        durationMs: Long,
        playing: Boolean,
        source: MediaSource? = current.activeMediaSource,
        sourceEpoch: Long? = null,
    ): AppState {
        val resolvedSource = source
            ?: SourcePlaybackProjection.sourceForProtocol(
                current.playback.protocol,
            )
        if (resolvedSource == null) {
            if (current.playback.mediaUrl == null) {
                return current
            }
            val preservePausedPosition =
                !playing &&
                    positionMs <= 1L &&
                    current.playback.positionMs >
                    PAUSED_ZERO_SENTINEL_TOLERANCE_MS
            return current.copy(
                playback = current.playback.copy(
                    positionMs = if (preservePausedPosition) {
                        current.playback.positionMs
                    } else {
                        positionMs.coerceAtLeast(0)
                    },
                    durationMs = durationMs.takeIf { it > 0 }
                        ?: current.playback.durationMs,
                    isPlaying = playing,
                ),
            )
        }
        val playback = SourcePlaybackProjection.playback(
            current,
            resolvedSource,
        )
        if (playback.mediaUrl == null) {
            return current
        }
        if (
            sourceEpoch != null &&
            playback.sourceEpoch != null &&
            sourceEpoch != playback.sourceEpoch
        ) {
            return current
        }

        val preservePausedPosition =
            !playing &&
                positionMs <= 1L &&
                playback.positionMs > PAUSED_ZERO_SENTINEL_TOLERANCE_MS
        return SourcePlaybackProjection.cachePlayback(
            current = current,
            source = resolvedSource,
            playback = playback.copy(
                positionMs = if (preservePausedPosition) {
                    playback.positionMs
                } else {
                    positionMs.coerceAtLeast(0)
                },
                durationMs = durationMs.takeIf { it > 0 }
                    ?: playback.durationMs,
                isPlaying = playing,
            ),
        )
    }

    fun dlnaMedia(
        current: AppState,
        event: AppEvent.DlnaMedia,
    ): AppState {
        val previous = SourcePlaybackProjection.playback(
            current,
            MediaSource.DLNA,
        )
        val sameMedia =
            previous.mediaUrl == event.url &&
                (
                    previous.sourceEpoch == null ||
                        event.epoch == null ||
                        previous.sourceEpoch == event.epoch
                    )
        val preserveKnownPosition =
            sameMedia &&
                event.startPositionMs <= 1L &&
                previous.positionMs > PAUSED_ZERO_SENTINEL_TOLERANCE_MS
        val embeddedLyrics = event.lyricsText?.let {
            LrcParser.parse(
                value = it,
                origin = LyricsOrigin.EMBEDDED,
            )
        }
        val hasExternalLyrics = OfflineLyricsResolver()
            .hasExternalCandidate(
                LyricsRequest(
                    metadataUri = event.lyricsUri,
                    mediaUri = event.url,
                ),
            )
        return SourcePlaybackProjection.cachePlayback(
            current = current.copy(
                selectedCoreMediaSource = MediaSource.DLNA,
                lastEvent = event,
            ),
            source = MediaSource.DLNA,
            playback = PlaybackSnapshot(
                title = event.title ?: previous.title.takeIf { sameMedia },
                artist = event.artist ?: previous.artist.takeIf { sameMedia },
                album = event.album ?: previous.album.takeIf { sameMedia },
                coverArt = event.artworkUrl
                    ?: previous.coverArt.takeIf { sameMedia },
                mediaUrl = event.url,
                mediaKind = event.mediaKind,
                protocol = DLNA_SOURCE,
                qualityText = dlnaQualityText(event)
                    ?: previous.qualityText.takeIf { sameMedia },
                durationMs = event.durationMs
                    ?.takeIf { it > 0 }
                    ?: previous.durationMs.takeIf { sameMedia },
                positionMs = if (preserveKnownPosition) {
                    previous.positionMs
                } else {
                    event.startPositionMs.coerceAtLeast(0)
                },
                isPlaying = previous.isPlaying && sameMedia,
                streamActive = true,
                sourceEpoch = event.epoch
                    ?: previous.sourceEpoch.takeIf { sameMedia },
                lyrics = embeddedLyrics
                    ?: previous.lyrics.takeIf { sameMedia },
                lyricsLoading =
                    if (embeddedLyrics == null && sameMedia) {
                        previous.lyricsLoading
                    } else {
                        embeddedLyrics?.isSynchronized != true &&
                            hasExternalLyrics
                    },
            ),
        )
    }

    fun isAirPlay(state: AppState): Boolean =
        state.playback.protocol.equals(AIRPLAY_SOURCE, ignoreCase = true)

    fun isDlna(state: AppState): Boolean =
        state.playback.protocol.equals(DLNA_SOURCE, ignoreCase = true)

    fun commandSource(state: AppState): String? =
        when (state.activeMediaSource) {
            MediaSource.DLNA -> DLNA_SOURCE
            MediaSource.AIRPLAY -> AIRPLAY_SOURCE
            MediaSource.XIAOMI_MIPLAY -> null
            null -> when {
                state.playback.protocol.equals(DLNA_SOURCE, ignoreCase = true) ->
                    DLNA_SOURCE

                state.playback.protocol.equals(AIRPLAY_SOURCE, ignoreCase = true) ->
                    AIRPLAY_SOURCE

                else -> null
            }
        }

    fun streamQualityText(event: AppEvent.StreamStarted): String? {
        val parts = buildList {
            event.sourceCodec
                ?.takeIf(String::isNotBlank)
                ?.uppercase(Locale.ROOT)
                ?.let(::add)
            // Never substitute decoded_bits when source_bits is unknown. AAC
            // commonly decodes to 32-bit float, which is not its source depth.
            event.sourceBits
                ?.takeIf { it > 0 }
                ?.let { add("$it-bit") }
            event.sourceSampleRate
                ?.takeIf { it > 0 }
                ?.let { rate ->
                    add(
                        if (rate % 1_000 == 0) {
                            "${rate / 1_000} kHz"
                        } else {
                            String.format(
                                Locale.ROOT,
                                "%.1f kHz",
                                rate / 1_000.0,
                            )
                        },
                    )
                }
            event.sourceChannels
                ?.takeIf { it > 0 }
                ?.let { channels ->
                    add(
                        when (channels) {
                            1 -> "单声道"
                            2 -> "立体声"
                            else -> "$channels 声道"
                        },
                    )
                }
        }
        return parts.takeIf(List<String>::isNotEmpty)?.joinToString(" · ")
    }

    fun dlnaQualityText(event: AppEvent.DlnaMedia): String? {
        val parts = buildList {
            dlnaCodec(
                contentType = event.contentType,
                url = event.url,
            )?.let(::add)
            event.bitsPerSample
                ?.takeIf { it > 0 }
                ?.let { add("$it-bit") }
            event.sampleRate
                ?.takeIf { it > 0 }
                ?.let { add(formatSampleRate(it)) }
            event.channels
                ?.takeIf { it > 0 }
                ?.let { channels ->
                    add(
                        when (channels) {
                            1 -> "单声道"
                            2 -> "立体声"
                            else -> "$channels 声道"
                        },
                    )
                }
            event.bitrateBps
                ?.takeIf { it > 0 }
                ?.let { add(formatBitrate(it)) }
        }
        return parts.takeIf(List<String>::isNotEmpty)?.joinToString(" · ")
    }

    fun mergeDlnaProbedQuality(
        current: AppState,
        mediaUrl: String,
        epoch: Long?,
        qualityText: String,
    ): AppState {
        val playback = SourcePlaybackProjection.playback(
            current,
            MediaSource.DLNA,
        )
        val sameEpoch =
            epoch == null ||
                playback.sourceEpoch == null ||
                playback.sourceEpoch == epoch
        if (playback.mediaUrl != mediaUrl || !sameEpoch) {
            return current
        }
        return SourcePlaybackProjection.cachePlayback(
            current = current,
            source = MediaSource.DLNA,
            playback = playback.copy(qualityText = qualityText),
        )
    }

    private fun eventSource(
        current: AppState,
        source: String?,
    ): MediaSource? =
        SourcePlaybackProjection.sourceForCoreId(source)
            ?: current.selectedCoreMediaSource
            ?: current.activeMediaSource

    private fun eventMatchesSource(
        current: AppState,
        source: MediaSource,
        epoch: Long?,
    ): Boolean {
        val currentEpoch = SourcePlaybackProjection
            .playback(current, source)
            .sourceEpoch
        return epoch == null || currentEpoch == null || epoch == currentEpoch
    }

    /**
     * AirPlay 2 can replace only its audio stream while keeping the logical
     * sender and MediaRemote session alive. Any retained media field marks
     * that replacement as a resume, even after streamActive became false.
     */
    private fun PlaybackSnapshot.hasRetainableAirPlayMedia(): Boolean =
        streamActive ||
            protocol.equals(AIRPLAY_SOURCE, ignoreCase = true) ||
            sourceEpoch != null ||
            title != null ||
            artist != null ||
            album != null ||
            genre != null ||
            coverArt != null ||
            durationMs != null ||
            positionMs > 0L ||
            trackIdentity != null ||
            lyrics != null

    private fun String?.nonBlankOrNull(): String? =
        this?.takeIf(String::isNotBlank)

    private fun dlnaCodec(
        contentType: String?,
        url: String,
    ): String? {
        val mime = contentType
            ?.substringBefore(';')
            ?.trim()
            ?.lowercase(Locale.ROOT)
        val extension = url
            .substringBefore('?')
            .substringBefore('#')
            .substringAfterLast('/', missingDelimiterValue = "")
            .substringAfterLast('.', missingDelimiterValue = "")
            .lowercase(Locale.ROOT)
        return when {
            mime == "audio/flac" || mime == "audio/x-flac" ||
                extension == "flac" -> "FLAC"
            mime == "audio/mpeg" && extension == "mp3" -> "MP3"
            mime == "audio/mpeg" -> "MPEG Audio"
            mime == "audio/aac" || mime == "audio/aacp" ||
                extension == "aac" -> "AAC"
            mime == "audio/alac" || mime == "audio/x-alac" ||
                extension == "alac" -> "ALAC"
            mime == "audio/wav" || mime == "audio/x-wav" ||
                extension == "wav" -> "WAV"
            mime == "audio/ogg" || extension == "ogg" -> "OGG"
            mime == "audio/opus" || extension == "opus" -> "OPUS"
            extension == "m4a" -> "M4A"
            mime != null -> mime.substringAfter('/').uppercase(Locale.ROOT)
            extension.isNotBlank() -> extension.uppercase(Locale.ROOT)
            else -> null
        }
    }

    private fun formatSampleRate(rate: Int): String =
        if (rate % 1_000 == 0) {
            "${rate / 1_000} kHz"
        } else {
            String.format(
                Locale.ROOT,
                "%.1f kHz",
                rate / 1_000.0,
            )
        }

    private fun formatBitrate(bitrateBps: Long): String =
        when {
            bitrateBps >= 1_000_000L -> String.format(
                Locale.ROOT,
                "%.2f Mbps",
                bitrateBps / 1_000_000.0,
            ).replace(".00 ", " ")
            bitrateBps >= 1_000L -> String.format(
                Locale.ROOT,
                "%.0f kbps",
                bitrateBps / 1_000.0,
            )
            else -> "$bitrateBps bps"
        }
}
