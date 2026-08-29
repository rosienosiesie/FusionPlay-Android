package com.airplayreceiver.desktop.backend

import java.net.HttpURLConnection
import java.net.URI
import java.net.URL
import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.Path
import java.util.Locale
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

enum class LyricsOrigin {
    EMBEDDED,
    METADATA_URI,
    SIDECAR_URI,
}

data class LyricLine(
    val startMs: Long?,
    val text: String,
)

data class LyricsDocument(
    val lines: List<LyricLine>,
    val origin: LyricsOrigin,
    val sourceUri: String? = null,
) {
    val isSynchronized: Boolean = lines.any { it.startMs != null }

    /**
     * Returns the line whose timestamp is the latest one not after
     * [positionMs]. Untimed lyrics deliberately have no active line.
     */
    fun activeLineIndex(positionMs: Long): Int? {
        if (!isSynchronized || lines.isEmpty()) {
            return null
        }

        var low = 0
        var high = lines.lastIndex
        var match = -1
        val target = positionMs.coerceAtLeast(0L)
        while (low <= high) {
            val middle = (low + high).ushr(1)
            val timestamp = lines[middle].startMs
            if (timestamp == null || timestamp > target) {
                high = middle - 1
            } else {
                match = middle
                low = middle + 1
            }
        }
        return match.takeIf { it >= 0 }
    }
}

object LrcParser {
    private val timestamp = Regex(
        """\[(?:(\d{1,3}):)?(\d{1,3}):(\d{2})(?:[.:](\d{1,3}))?]""",
    )
    private val offset = Regex(
        """^\s*\[offset\s*:\s*([+-]?\d+)]\s*$""",
        RegexOption.IGNORE_CASE,
    )
    private val metadata = Regex(
        """^\s*\[(?:ar|al|ti|by|re|ve|length|au)\s*:.*]\s*$""",
        RegexOption.IGNORE_CASE,
    )
    private val enhancedWordTimestamp = Regex(
        """<\d{1,3}:\d{2}(?:[.:]\d{1,3})?>""",
    )

    fun parse(
        value: String,
        origin: LyricsOrigin = LyricsOrigin.EMBEDDED,
        sourceUri: String? = null,
    ): LyricsDocument? {
        val normalized = value
            .removePrefix("\uFEFF")
            .replace("\r\n", "\n")
            .replace('\r', '\n')
        if (normalized.isBlank()) {
            return null
        }

        val offsetMs = normalized
            .lineSequence()
            .mapNotNull { line -> offset.matchEntire(line)?.groupValues?.get(1) }
            .mapNotNull(String::toLongOrNull)
            .lastOrNull()
            ?: 0L

        val timed = mutableListOf<Pair<Long, String>>()
        val untimed = mutableListOf<String>()
        normalized.lineSequence().forEach { rawLine ->
            val line = rawLine.trim()
            if (
                line.isEmpty() ||
                offset.matches(line) ||
                metadata.matches(line)
            ) {
                return@forEach
            }

            val matches = timestamp.findAll(line).toList()
            val text = timestamp.replace(line, "")
                .let { enhancedWordTimestamp.replace(it, "") }
                .trim()
            if (matches.isEmpty()) {
                if (text.isNotEmpty()) {
                    untimed += text
                }
                return@forEach
            }
            if (text.isEmpty()) {
                return@forEach
            }

            matches.forEach { match ->
                parseTimestamp(match)?.let { timestampMs ->
                    timed += (timestampMs + offsetMs).coerceAtLeast(0L) to text
                }
            }
        }

        val lines = if (timed.isNotEmpty()) {
            // Translations commonly repeat the same timestamp. Keeping them in
            // one visual line prevents binary-search selection from highlighting
            // only the translation while hiding the original.
            timed.withIndex()
                .sortedWith(
                    compareBy<IndexedValue<Pair<Long, String>>> {
                        it.value.first
                    }.thenBy { it.index },
                )
                .groupBy(
                    keySelector = { it.value.first },
                    valueTransform = { it.value.second },
                )
                .map { (startMs, texts) ->
                    LyricLine(
                        startMs = startMs,
                        text = texts.distinct().joinToString("\n"),
                    )
                }
        } else {
            untimed.map { text -> LyricLine(startMs = null, text = text) }
        }

        return lines
            .takeIf(List<LyricLine>::isNotEmpty)
            ?.let {
                LyricsDocument(
                    lines = it,
                    origin = origin,
                    sourceUri = sourceUri,
                )
            }
    }

    private fun parseTimestamp(match: MatchResult): Long? {
        val hours = match.groupValues[1]
            .takeIf(String::isNotEmpty)
            ?.toLongOrNull()
        val minutes = match.groupValues[2].toLongOrNull() ?: return null
        val seconds = match.groupValues[3].toLongOrNull() ?: return null
        if (seconds >= 60 || (hours != null && minutes >= 60)) {
            return null
        }
        val fraction = match.groupValues[4]
        val fractionMs = when (fraction.length) {
            0 -> 0L
            1 -> fraction.toLong() * 100L
            2 -> fraction.toLong() * 10L
            else -> fraction.take(3).toLong()
        }
        return (
            (hours ?: 0L) * 3_600_000L +
                minutes * 60_000L +
                seconds * 1_000L +
                fractionMs
            )
    }
}

/**
 * Monotonic interpolation between authoritative media-position samples.
 *
 * All public positions use milliseconds. A new source token, seek, pause, or
 * resume replaces the anchor immediately, so old-source time can never leak
 * into the next track.
 */
class LyricsPlaybackClock {
    private var sourceToken: Any? = null
    private var anchorPositionMs: Long = 0L
    private var anchorNanos: Long = 0L
    private var playing: Boolean = false
    private var initialized: Boolean = false

    fun observe(
        positionMs: Long,
        isPlaying: Boolean,
        sourceToken: Any?,
        nowNanos: Long = System.nanoTime(),
    ) {
        val samplePositionMs = positionMs.coerceAtLeast(0L)
        val sourceChanged = !initialized || this.sourceToken != sourceToken
        val playbackStateChanged = initialized && playing != isPlaying
        if (sourceChanged || playbackStateChanged || !isPlaying) {
            reanchor(
                positionMs = samplePositionMs,
                isPlaying = isPlaying,
                sourceToken = sourceToken,
                nowNanos = nowNanos,
            )
            return
        }

        val estimatedPositionMs = positionAt(nowNanos)
        val correctionMs = samplePositionMs - estimatedPositionMs
        if (
            correctionMs >= SEEK_DISCONTINUITY_MS ||
            correctionMs <= -SEEK_DISCONTINUITY_MS ||
            correctionMs > 0L
        ) {
            reanchor(
                positionMs = samplePositionMs,
                isPlaying = true,
                sourceToken = sourceToken,
                nowNanos = nowNanos,
            )
        }
        // A small backwards correction while playing is normally a delayed or
        // duplicated network progress sample. Keeping the monotonic estimate
        // prevents every sample from dragging synchronized lyrics behind the
        // audio. Genuine seeks exceed the discontinuity threshold above.
    }

    private fun reanchor(
        positionMs: Long,
        isPlaying: Boolean,
        sourceToken: Any?,
        nowNanos: Long,
    ) {
        this.sourceToken = sourceToken
        anchorPositionMs = positionMs
        anchorNanos = nowNanos
        playing = isPlaying
        initialized = true
    }

    fun positionAt(nowNanos: Long = System.nanoTime()): Long {
        if (!initialized || !playing) {
            return anchorPositionMs
        }
        val elapsedMs = (nowNanos - anchorNanos)
            .coerceAtLeast(0L) / NANOS_PER_MILLISECOND
        return anchorPositionMs.saturatingAdd(elapsedMs)
    }

    fun belongsTo(token: Any?): Boolean = initialized && sourceToken == token

    private fun Long.saturatingAdd(other: Long): Long =
        if (Long.MAX_VALUE - this < other) Long.MAX_VALUE else this + other

    private companion object {
        const val NANOS_PER_MILLISECOND = 1_000_000L
        const val SEEK_DISCONTINUITY_MS = 3_000L
    }
}

data class LyricsRequest(
    val embeddedText: String? = null,
    val metadataUri: String? = null,
    val mediaUri: String? = null,
)

data class LoadedLyrics(
    val text: String,
    val uri: URI,
)

fun interface LyricsContentLoader {
    suspend fun load(uri: URI): LoadedLyrics?
}

class OfflineLyricsResolver(
    private val loader: LyricsContentLoader = JvmLyricsContentLoader(),
) {
    suspend fun resolve(request: LyricsRequest): LyricsDocument? {
        val embedded = request.embeddedText
            ?.takeIf(String::isNotBlank)
            ?.let { value ->
                LrcParser.parse(
                    value = value,
                    origin = LyricsOrigin.EMBEDDED,
                )
            }
        if (embedded?.isSynchronized == true) {
            return embedded
        }

        val candidates = buildList {
            resolveMetadataUri(
                raw = request.metadataUri,
                mediaUri = request.mediaUri,
            )?.let { add(it to LyricsOrigin.METADATA_URI) }
            deriveSidecarLyricsUri(request.mediaUri)
                ?.let { add(it to LyricsOrigin.SIDECAR_URI) }
        }.distinctBy { (uri, _) -> uri.normalize().toASCIIString() }

        for ((uri, origin) in candidates) {
            val loaded = try {
                loader.load(uri)
            } catch (exception: CancellationException) {
                throw exception
            } catch (_: Exception) {
                null
            } ?: continue
            LrcParser.parse(
                value = loaded.text,
                origin = origin,
                sourceUri = loaded.uri.toASCIIString(),
            )?.let { return it }
        }
        return embedded
    }

    fun hasExternalCandidate(request: LyricsRequest): Boolean =
        resolveMetadataUri(request.metadataUri, request.mediaUri) != null ||
            deriveSidecarLyricsUri(request.mediaUri) != null

    companion object {
        internal fun resolveMetadataUri(
            raw: String?,
            mediaUri: String?,
        ): URI? {
            val candidate = raw?.trim()?.takeIf(String::isNotEmpty) ?: return null
            val parsed = runCatching { URI(candidate) }.getOrNull() ?: return null
            val base = mediaUri
                ?.let { runCatching { URI(it) }.getOrNull() }
                ?.takeIf(URI::isAbsolute)
                ?: return null
            val resolved = if (parsed.isAbsolute) {
                parsed
            } else {
                base.resolve(parsed)
            }
            return resolved.takeIf {
                when {
                    base.scheme.equals("http", ignoreCase = true) ||
                        base.scheme.equals("https", ignoreCase = true) ->
                        resolved.scheme.equals("http", ignoreCase = true) ||
                            resolved.scheme.equals("https", ignoreCase = true)

                    base.scheme.equals("file", ignoreCase = true) ->
                        resolved.scheme.equals("file", ignoreCase = true) &&
                            isSafeFileSidecar(base, resolved)

                    else -> false
                }
            }
        }

        internal fun deriveSidecarLyricsUri(mediaUri: String?): URI? {
            val media = mediaUri
                ?.trim()
                ?.takeIf(String::isNotEmpty)
                ?.let { runCatching { URI(it) }.getOrNull() }
                ?.takeIf(URI::isAbsolute)
                ?: return null
            if (
                !media.scheme.equals("http", ignoreCase = true) &&
                !media.scheme.equals("https", ignoreCase = true) &&
                !media.scheme.equals("file", ignoreCase = true)
            ) {
                return null
            }

            val path = media.path
                ?.takeIf { it.isNotEmpty() && !it.endsWith('/') }
                ?: return null
            val slash = path.lastIndexOf('/')
            val dot = path.lastIndexOf('.')
            val stemEnd = dot.takeIf { it > slash } ?: path.length
            val sidecarPath = path.substring(0, stemEnd) + ".lrc"
            return runCatching {
                URI(
                    media.scheme,
                    media.userInfo,
                    media.host,
                    media.port,
                    sidecarPath,
                    media.query,
                    null,
                )
            }.getOrNull()
        }

        private fun isSafeFileSidecar(
            media: URI,
            lyrics: URI,
        ): Boolean {
            val mediaPath = runCatching { Path.of(media).normalize() }
                .getOrNull()
                ?: return false
            val lyricsPath = runCatching { Path.of(lyrics).normalize() }
                .getOrNull()
                ?: return false
            val mediaParent = mediaPath.parent ?: return false
            val lyricsParent = lyricsPath.parent ?: return false
            if (!mediaParent.toString().equals(lyricsParent.toString(), ignoreCase = true)) {
                return false
            }
            val mediaName = mediaPath.fileName?.toString() ?: return false
            val lyricsName = lyricsPath.fileName?.toString() ?: return false
            if (!lyricsName.endsWith(".lrc", ignoreCase = true)) {
                return false
            }
            val mediaStem = mediaName.substringBeforeLast('.', mediaName)
            val lyricsStem = lyricsName.substringBeforeLast('.', lyricsName)
            return mediaStem.equals(lyricsStem, ignoreCase = true)
        }
    }
}

class JvmLyricsContentLoader(
    private val maxBytes: Int = DEFAULT_MAX_BYTES,
) : LyricsContentLoader {
    override suspend fun load(uri: URI): LoadedLyrics? =
        withContext(Dispatchers.IO) {
            when {
                uri.scheme.equals("http", ignoreCase = true) ||
                    uri.scheme.equals("https", ignoreCase = true) ->
                    loadHttp(uri)

                uri.scheme.equals("file", ignoreCase = true) ->
                    loadFile(uri)

                else -> null
            }
        }

    private fun loadHttp(uri: URI): LoadedLyrics? {
        val connection = (URL(uri.toString()).openConnection() as HttpURLConnection).apply {
            connectTimeout = 4_000
            readTimeout = 7_000
            instanceFollowRedirects = true
            setRequestProperty(
                "Accept",
                "text/plain, text/lrc, application/lrc, application/octet-stream;q=0.5",
            )
        }
        try {
            if (connection.responseCode !in 200..299) {
                return null
            }
            val bytes = connection.inputStream.use { it.readBytes() }
            if (bytes.isEmpty() || bytes.size > maxBytes) {
                return null
            }
            val declaredCharset = (connection.contentType ?: "")
                .substringAfter("charset=", "")
                .substringBefore(';')
                .trim()
                .trim('"', '\'')
                .takeIf(String::isNotEmpty)
            return LoadedLyrics(
                text = decodeLyrics(bytes, declaredCharset),
                uri = uri,
            )
        } finally {
            connection.disconnect()
        }
    }

    private fun loadFile(uri: URI): LoadedLyrics? {
        val path = runCatching { Path.of(uri) }.getOrNull() ?: return null
        val size = runCatching { Files.size(path) }.getOrNull() ?: return null
        if (size <= 0L || size > maxBytes) {
            return null
        }
        return LoadedLyrics(
            text = decodeLyrics(Files.readAllBytes(path), null),
            uri = uri,
        )
    }

    private fun decodeLyrics(
        bytes: ByteArray,
        declaredCharset: String?,
    ): String {
        if (bytes.startsWith(byteArrayOf(0xEF.toByte(), 0xBB.toByte(), 0xBF.toByte()))) {
            return String(bytes, 3, bytes.size - 3, StandardCharsets.UTF_8)
        }
        if (bytes.startsWith(byteArrayOf(0xFF.toByte(), 0xFE.toByte()))) {
            return String(bytes, 2, bytes.size - 2, StandardCharsets.UTF_16LE)
        }
        if (bytes.startsWith(byteArrayOf(0xFE.toByte(), 0xFF.toByte()))) {
            return String(bytes, 2, bytes.size - 2, StandardCharsets.UTF_16BE)
        }

        declaredCharset
            ?.let { runCatching { java.nio.charset.Charset.forName(it) }.getOrNull() }
            ?.let { return String(bytes, it) }

        return try {
            val decoder = StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
            decoder.decode(ByteBuffer.wrap(bytes)).toString()
        } catch (_: CharacterCodingException) {
            String(bytes, java.nio.charset.Charset.forName("GB18030"))
        }
    }

    private fun ByteArray.startsWith(prefix: ByteArray): Boolean =
        size >= prefix.size && prefix.indices.all { this[it] == prefix[it] }

    private companion object {
        const val DEFAULT_MAX_BYTES = 2 * 1024 * 1024
    }
}
