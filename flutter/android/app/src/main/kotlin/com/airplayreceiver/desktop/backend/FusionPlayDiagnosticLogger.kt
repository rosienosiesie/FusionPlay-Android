package com.airplayreceiver.desktop.backend

import java.nio.charset.StandardCharsets
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.nio.file.StandardOpenOption
import java.time.Clock
import java.util.UUID
import java.util.concurrent.atomic.AtomicLong
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

/**
 * Persistent, privacy-aware diagnostics for the Android application and the
 * AirPlay/DLNA core. Xiaomi bridge diagnostics use the same privacy filter and
 * are tagged so exports can split every receiver service into its own JSONL.
 */
class FusionPlayDiagnosticLogger(
    private val logDirectory: Path = defaultLogDirectory(),
    private val clock: Clock = Clock.systemUTC(),
    private val maximumFileBytes: Long = DEFAULT_MAXIMUM_FILE_BYTES,
    private val fileCount: Int = DEFAULT_FILE_COUNT,
) {
    private val gate = Any()
    private val runId = FusionPlayRunIdentity.runId
    private val sequence = AtomicLong()
    private val lastProgressWriteNanos = mutableMapOf<String, Long>()
    private val logPath = logDirectory.resolve(LOG_FILE_NAME)

    init {
        require(maximumFileBytes > 0L)
        require(fileCount > 0)
    }

    fun write(
        component: String,
        event: String,
        outcome: String,
        details: Map<String, Any?> = emptyMap(),
    ) {
        val payload = buildJsonObject {
            put("schema_version", 1)
            put("timestamp_utc", clock.instant().toString())
            put("monotonic_ms", System.nanoTime() / 1_000_000L)
            put("run_id", runId)
            put("sequence", sequence.incrementAndGet())
            put("process_id", android.os.Process.myPid().toLong())
            put("thread", safeToken(Thread.currentThread().name))
            put("component", safeToken(component))
            put("event", safeToken(event))
            put("outcome", safeToken(outcome))
            put(
                "level",
                when (outcome.lowercase()) {
                    "failure", "error" -> "error"
                    "warning" -> "warning"
                    else -> "info"
                },
            )
            details.forEach { (key, value) ->
                val safeKey = safeKey(key)
                if (safeKey != null && isSafeDetailKey(safeKey)) {
                    put(safeKey, safeJsonValue(value))
                }
            }
        }
        val line = payload.toString() + System.lineSeparator()
        val encodedLength = line.toByteArray(StandardCharsets.UTF_8).size

        runCatching {
            synchronized(gate) {
                Files.createDirectories(logDirectory)
                val currentLength = if (Files.isRegularFile(logPath)) {
                    Files.size(logPath)
                } else {
                    0L
                }
                if (
                    currentLength > 0L &&
                    currentLength + encodedLength > maximumFileBytes
                ) {
                    rotate()
                }
                Files.write(
                    logPath,
                    line.toByteArray(StandardCharsets.UTF_8),
                    StandardOpenOption.CREATE,
                    StandardOpenOption.APPEND,
                )
            }
        }
    }

    /**
     * Copies a consistent point-in-time view of every active and rotated log.
     * The same gate used by writes and rotation prevents an export from
     * observing half-moved files.
     */
    fun copySnapshotTo(destination: Path): List<Path> = synchronized(gate) {
        Files.createDirectories(destination)
        buildList {
            val candidates = buildList {
                add(logPath)
                for (index in 1 until fileCount) add(rotatedPath(index))
            }
            candidates.forEach { source ->
                if (Files.isRegularFile(source)) {
                    val target = destination.resolve(source.fileName.toString())
                    Files.copy(
                        source,
                        target,
                        StandardCopyOption.REPLACE_EXISTING,
                    )
                    add(target)
                }
            }
        }
    }

    fun sanitizeLineForExport(value: String): String = sanitizeText(value)

    fun writeApplicationMessage(
        level: AppLogLevel,
        message: String,
    ) {
        write(
            component = "application",
            event = "message",
            outcome = when (level) {
                AppLogLevel.ERROR -> "failure"
                AppLogLevel.WARNING -> "warning"
                AppLogLevel.INFO -> "info"
            },
            details = mapOf(
                "level" to level.name,
                "message" to sanitizeText(message),
            ),
        )
    }

    fun writeCoreProcessLog(log: CoreProcessLog) {
        val component = when {
            log.message.contains("airplay", ignoreCase = true) -> "airplay"
            log.message.contains("dlna", ignoreCase = true) -> "dlna"
            else -> "core_process"
        }
        write(
            component = component,
            event = "process_output",
            outcome = if (log.level == CoreLogLevel.ERROR) {
                "failure"
            } else {
                "info"
            },
            details = mapOf(
                "level" to log.level.name,
                "source" to log.source.name,
                "message" to sanitizeText(log.message),
            ),
        )
    }

    fun writeCoreProcessExit(exit: CoreProcessExit) {
        write(
            component = "core_process",
            event = "process_exit",
            outcome = if (exit.expected || exit.exitCode == 0) {
                "success"
            } else {
                "failure"
            },
            details = mapOf(
                "exit_code" to exit.exitCode,
                "expected" to exit.expected,
            ),
        )
    }

    fun writeCoreEvent(event: AppEvent) {
        val source = eventSource(event)
        if (event is AppEvent.Progress) {
            val progressKey = "${source.orEmpty()}:${event.epoch ?: 0L}"
            val now = System.nanoTime()
            val shouldWrite = synchronized(gate) {
                val previous = lastProgressWriteNanos[progressKey]
                if (
                    previous != null &&
                    now - previous < PROGRESS_SAMPLE_NANOS
                ) {
                    false
                } else {
                    lastProgressWriteNanos[progressKey] = now
                    true
                }
            }
            if (!shouldWrite) {
                return
            }
        }
        val details = mutableMapOf<String, Any?>(
            "event_type" to event.type,
            "source" to source,
            "epoch" to eventEpoch(event),
        )
        when (event) {
            is AppEvent.Status -> {
                details["state"] = event.state
                details["message"] = sanitizeText(event.message)
            }
            is AppEvent.ReceiverReady -> {
                details["name_present"] = !event.name.isNullOrBlank()
                details["port"] = event.port
                details["device_id_present"] =
                    !event.deviceId.isNullOrBlank()
            }
            is AppEvent.OutputDevice -> {
                details["is_default"] = event.isDefault
                details["sample_rate"] = event.sampleRate
                details["channels"] = event.channels
                details["sample_format"] = event.sampleFormat
                details["bits_per_sample"] = event.bitsPerSample
            }
            is AppEvent.ClientConnected ->
                details["address_present"] = !event.address.isNullOrBlank()
            is AppEvent.ClientDisconnected ->
                details["address_present"] = !event.address.isNullOrBlank()
            is AppEvent.StreamStarted -> {
                details["source_codec"] = event.sourceCodec
                details["source_sample_rate"] = event.sourceSampleRate
                details["source_channels"] = event.sourceChannels
                details["source_bits"] = event.sourceBits
                details["decoded_sample_rate"] = event.decodedSampleRate
                details["decoded_channels"] = event.decodedChannels
                details["decoded_bits"] = event.decodedBits
            }
            is AppEvent.SourceTakeover -> {
                details["media_kind"] = event.mediaKind
                details["previous_source"] = event.previousSource
                details["previous_media_kind"] = event.previousMediaKind
                details["previous_epoch"] = event.previousEpoch
                details["reason"] = event.reason
            }
            is AppEvent.NowPlaying -> {
                details["title_present"] = !event.title.isNullOrBlank()
                details["title_length"] = event.title?.length
                details["artist_present"] = !event.artist.isNullOrBlank()
                details["artist_length"] = event.artist?.length
                details["album_present"] = !event.album.isNullOrBlank()
                details["album_length"] = event.album?.length
                details["duration_ms"] = event.durationMs
            }
            is AppEvent.CoverArt ->
                details["artwork_present"] = !event.path.isNullOrBlank()
            is AppEvent.Progress -> {
                details["position_ms"] = event.positionMs
                details["duration_ms"] = event.durationMs
            }
            is AppEvent.PlaybackState ->
                details["playing"] = event.playing
            is AppEvent.VideoPlay -> {
                details["url_present"] = event.url.isNotBlank()
                details["start_position_ms"] = event.startPositionMs
            }
            is AppEvent.VideoSeek ->
                details["position_ms"] = event.positionMs
            is AppEvent.VideoRate ->
                details["rate"] = event.rate
            is AppEvent.DlnaReady ->
                details["port"] = event.port
            is AppEvent.DlnaMedia -> {
                details["url_present"] = event.url.isNotBlank()
                details["title_present"] = !event.title.isNullOrBlank()
                details["title_length"] = event.title?.length
                details["artist_present"] = !event.artist.isNullOrBlank()
                details["artist_length"] = event.artist?.length
                details["album_present"] = !event.album.isNullOrBlank()
                details["album_length"] = event.album?.length
                details["artwork_present"] =
                    !event.artworkUrl.isNullOrBlank()
                details["content_type"] = event.contentType
                details["media_kind"] = event.mediaKind
                details["duration_ms"] = event.durationMs
                details["start_position_ms"] = event.startPositionMs
                details["bitrate_bps"] = event.bitrateBps
                details["sample_rate"] = event.sampleRate
                details["bits_per_sample"] = event.bitsPerSample
                details["channels"] = event.channels
            }
            is AppEvent.DlnaSeek ->
                details["position_ms"] = event.positionMs
            is AppEvent.DlnaRate ->
                details["rate"] = event.rate
            is AppEvent.DlnaVolume -> {
                details["volume_percent"] = event.percent
                details["muted"] = event.muted
            }
            is AppEvent.RemoteControlAvailable -> {
                details["commands"] = event.commands.sorted().joinToString(",")
                details["transport"] = event.transport
                details["experimental"] = event.experimental
            }
            is AppEvent.RemoteControlUnavailable ->
                details["reason"] = event.reason
            is AppEvent.CommandResult -> {
                details["command"] = event.command
                details["succeeded"] = event.succeeded
                details["message"] = event.message?.let(::sanitizeText)
            }
            is AppEvent.Error ->
                details["message"] = sanitizeText(event.message)
            is AppEvent.Log -> {
                details["level"] = event.level
                details["message"] = sanitizeText(event.message)
            }
            is AppEvent.Unknown ->
                details["unknown_type"] = event.type
            else -> Unit
        }
        write(
            component = componentFor(source, event),
            event = event.type,
            outcome = eventOutcome(event),
            details = details,
        )
    }

    private fun rotate() {
        if (fileCount == 1) {
            Files.deleteIfExists(logPath)
            return
        }
        Files.deleteIfExists(rotatedPath(fileCount - 1))
        for (index in (fileCount - 2) downTo 1) {
            val source = rotatedPath(index)
            if (Files.isRegularFile(source)) {
                Files.move(
                    source,
                    rotatedPath(index + 1),
                    StandardCopyOption.REPLACE_EXISTING,
                )
            }
        }
        if (Files.isRegularFile(logPath)) {
            Files.move(
                logPath,
                rotatedPath(1),
                StandardCopyOption.REPLACE_EXISTING,
            )
        }
    }

    private fun rotatedPath(index: Int): Path =
        logPath.resolveSibling("$LOG_FILE_NAME.$index")

    private fun eventSource(event: AppEvent): String? = when (event) {
        is AppEvent.StreamStarted -> event.source
        is AppEvent.StreamStopped -> event.source
        is AppEvent.SourceTakeover -> event.source
        is AppEvent.NowPlaying -> event.source
        is AppEvent.CoverArt -> event.source
        is AppEvent.Volume -> event.source
        is AppEvent.Progress -> event.source
        is AppEvent.PlaybackState -> event.source
        is AppEvent.VideoPlay -> event.source ?: "airplay"
        is AppEvent.VideoSeek -> event.source ?: "airplay"
        is AppEvent.VideoRate -> event.source ?: "airplay"
        is AppEvent.VideoStop -> event.source ?: "airplay"
        is AppEvent.DlnaMedia -> event.source ?: "dlna"
        is AppEvent.DlnaSeek -> event.source ?: "dlna"
        is AppEvent.DlnaRate -> event.source ?: "dlna"
        is AppEvent.DlnaStop -> event.source ?: "dlna"
        is AppEvent.DlnaVolume -> event.source ?: "dlna"
        is AppEvent.RemoteControlAvailable -> event.source
        is AppEvent.RemoteControlUnavailable -> event.source
        is AppEvent.ReceiverReady,
        is AppEvent.ClientConnected,
        is AppEvent.ClientDisconnected,
        -> "airplay"
        is AppEvent.DlnaReady -> "dlna"
        else -> null
    }

    private fun eventEpoch(event: AppEvent): Long? = when (event) {
        is AppEvent.StreamStarted -> event.epoch
        is AppEvent.StreamStopped -> event.epoch
        is AppEvent.SourceTakeover -> event.epoch
        is AppEvent.NowPlaying -> event.epoch
        is AppEvent.CoverArt -> event.epoch
        is AppEvent.Volume -> event.epoch
        is AppEvent.Progress -> event.epoch
        is AppEvent.PlaybackState -> event.epoch
        is AppEvent.VideoPlay -> event.epoch
        is AppEvent.VideoSeek -> event.epoch
        is AppEvent.VideoRate -> event.epoch
        is AppEvent.VideoStop -> event.epoch
        is AppEvent.DlnaMedia -> event.epoch
        is AppEvent.DlnaSeek -> event.epoch
        is AppEvent.DlnaRate -> event.epoch
        is AppEvent.DlnaStop -> event.epoch
        is AppEvent.DlnaVolume -> event.epoch
        is AppEvent.RemoteControlAvailable -> event.epoch
        is AppEvent.RemoteControlUnavailable -> event.epoch
        else -> null
    }

    private fun componentFor(source: String?, event: AppEvent): String {
        return when (source?.lowercase()) {
            "airplay" -> "airplay"
            "dlna" -> "dlna"
            "xiaomi", "xiaomi_miplay", "miplay" -> "xiaomi_miplay"
            else -> when (event) {
                is AppEvent.DlnaReady,
                is AppEvent.DlnaMedia,
                is AppEvent.DlnaSeek,
                is AppEvent.DlnaRate,
                is AppEvent.DlnaStop,
                is AppEvent.DlnaVolume,
                -> "dlna"
                is AppEvent.VideoPlay,
                is AppEvent.VideoSeek,
                is AppEvent.VideoRate,
                is AppEvent.VideoStop,
                -> "airplay"
                else -> "core"
            }
        }
    }

    private fun eventOutcome(event: AppEvent): String = when (event) {
        is AppEvent.Error -> "failure"
        is AppEvent.CommandResult ->
            if (event.succeeded) "success" else "failure"
        is AppEvent.Status ->
            if (event.state.equals("error", ignoreCase = true)) {
                "failure"
            } else {
                "observed"
            }
        is AppEvent.Log ->
            if (event.level.equals("error", ignoreCase = true)) {
                "failure"
            } else {
                "observed"
            }
        else -> "observed"
    }

    private fun safeJsonValue(value: Any?) = when (value) {
        null -> JsonNull
        is Boolean -> JsonPrimitive(value)
        is Byte -> JsonPrimitive(value)
        is Short -> JsonPrimitive(value)
        is Int -> JsonPrimitive(value)
        is Long -> JsonPrimitive(value)
        is Float -> JsonPrimitive(value)
        is Double -> JsonPrimitive(value)
        is Number -> JsonPrimitive(value.toString())
        else -> JsonPrimitive(sanitizeText(value.toString()))
    }

    private fun safeKey(value: String): String? {
        val result = value
            .take(64)
            .map { character ->
                if (
                    character.isLetterOrDigit() ||
                    character == '_' ||
                    character == '-'
                ) {
                    character.lowercaseChar()
                } else {
                    '_'
                }
            }
            .joinToString("")
        return result.ifBlank { null }
    }

    private fun isSafeDetailKey(key: String): Boolean {
        if (
            key.endsWith("_present") ||
            key.endsWith("_length")
        ) {
            return true
        }
        return SENSITIVE_KEY_FRAGMENTS.none(key::contains)
    }

    private fun safeToken(value: String): String =
        sanitizeText(value)
            .take(128)
            .map { character ->
                if (
                    character.isLetterOrDigit() ||
                    character in "_-.:"
                ) {
                    character.lowercaseChar()
                } else {
                    '_'
                }
            }
            .joinToString("")
            .ifBlank { "unknown" }

    private fun sanitizeText(value: String): String {
        return value
            .take(MAXIMUM_TEXT_LENGTH)
            .replace(USER_PROFILE_PATTERN, "<USER_PROFILE>")
            .replace(IPV4_PATTERN, "<IP_ADDRESS>")
            .replace(MAC_ADDRESS_PATTERN, "<MAC_ADDRESS>")
            .replace(URL_PATTERN, "<URL>")
            .replace(SECRET_PATTERN, "$1=<REDACTED>")
    }

    companion object {
        const val LOG_FILE_NAME = "fusionplay.jsonl"
        private const val DEFAULT_MAXIMUM_FILE_BYTES = 4L * 1024L * 1024L
        private const val DEFAULT_FILE_COUNT = 5
        private const val MAXIMUM_TEXT_LENGTH = 1_024
        private const val PROGRESS_SAMPLE_NANOS = 3_000_000_000L
        private val SENSITIVE_KEY_FRAGMENTS = setOf(
            "account",
            "album",
            "artist",
            "artwork_url",
            "base64",
            "password",
            "raw",
            "title",
            "token",
        )
        private val USER_PROFILE_PATTERN =
            Regex("""(?i)[A-Z]:\\Users\\[^\\\r\n]+""")
        private val IPV4_PATTERN =
            Regex(
                """(?<!\d)(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}""" +
                    """(?:25[0-5]|2[0-4]\d|1?\d?\d)(?!\d)""",
            )
        private val MAC_ADDRESS_PATTERN =
            Regex(
                """(?i)(?<![0-9a-f])(?:[0-9a-f]{2}[:-]){5}""" +
                    """[0-9a-f]{2}(?![0-9a-f])""",
            )
        private val URL_PATTERN =
            Regex("""(?i)\b(?:https?|rtsp)://[^\s"']+""")
        private val SECRET_PATTERN =
            Regex(
                """(?i)\b(password|token|authorization|cookie|account)""" +
                    """\s*[:=]\s*[^\s,;]+""",
            )

        fun defaultLogDirectory(): Path =
            SettingsStore.localAppDataDirectory()
                .resolve(SettingsStore.APP_DIRECTORY_NAME)
                .resolve("Logs")
    }
}

internal object FusionPlayRunIdentity {
    val runId: String = System.getenv("FUSIONPLAY_RUN_ID")
        ?.trim()
        ?.takeIf { it.matches(Regex("""[A-Za-z0-9_-]{8,64}""")) }
        ?: UUID.randomUUID().toString().replace("-", "")
}
