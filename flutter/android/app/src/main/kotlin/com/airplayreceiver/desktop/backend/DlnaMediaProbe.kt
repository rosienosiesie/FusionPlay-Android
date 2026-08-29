package com.airplayreceiver.desktop.backend

import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.URI
import java.net.URL
import java.util.Locale
import kotlin.math.roundToLong
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

internal data class DlnaProbedQuality(
    val contentType: String? = null,
    val bitrateBps: Long? = null,
    val sampleRate: Int? = null,
    val bitsPerSample: Int? = null,
    val channels: Int? = null,
) {
    val isEmpty: Boolean
        get() = contentType == null &&
            bitrateBps == null &&
            sampleRate == null &&
            bitsPerSample == null &&
            channels == null
}

/**
 * Bounded, read-only probe for DLNA senders that omit technical `<res>`
 * attributes. Values come only from HTTP headers, static-file length/duration,
 * or a recognized container/codec header; unknown values remain null.
 */
internal class DlnaMediaProbe {
    suspend fun probe(
        url: String,
        durationMs: Long?,
    ): DlnaProbedQuality = withContext(Dispatchers.IO) {
        val uri = runCatching { URI(url.trim()) }.getOrNull()
            ?: return@withContext DlnaProbedQuality()
        if (
            !uri.isAbsolute ||
            uri.scheme?.lowercase(Locale.ROOT) !in SUPPORTED_SCHEMES
        ) {
            return@withContext DlnaProbedQuality()
        }

        try {
            probeHttp(uri.toURL(), durationMs)
        } catch (cancelled: CancellationException) {
            throw cancelled
        } catch (_: Exception) {
            DlnaProbedQuality()
        }
    }

    private fun probeHttp(
        initialUrl: URL,
        durationMs: Long?,
    ): DlnaProbedQuality {
        var currentUrl = initialUrl
        repeat(MAX_REDIRECTS + 1) {
            val connection = currentUrl.openConnection() as? HttpURLConnection
                ?: return DlnaProbedQuality()
            try {
                connection.instanceFollowRedirects = false
                connection.connectTimeout = CONNECT_TIMEOUT_MS
                connection.readTimeout = READ_TIMEOUT_MS
                connection.requestMethod = "GET"
                connection.setRequestProperty(
                    "Range",
                    "bytes=0-${MAX_PROBE_BYTES - 1}",
                )
                connection.setRequestProperty("Accept-Encoding", "identity")
                connection.setRequestProperty("User-Agent", USER_AGENT)
                val responseCode = connection.responseCode
                if (responseCode in 300..399) {
                    val redirect = connection.getHeaderField("Location")
                        ?: return DlnaProbedQuality()
                    val redirected = runCatching {
                        currentUrl.toURI().resolve(redirect).toURL()
                    }.getOrNull() ?: return DlnaProbedQuality()
                    if (
                        redirected.protocol.lowercase(Locale.ROOT) !in
                        SUPPORTED_SCHEMES
                    ) {
                        return DlnaProbedQuality()
                    }
                    currentUrl = redirected
                    return@repeat
                }
                if (responseCode !in 200..299) {
                    return DlnaProbedQuality()
                }
                val bytes = connection.inputStream.use { input ->
                    val output = ByteArrayOutputStream(MAX_PROBE_BYTES)
                    val buffer = ByteArray(16 * 1024)
                    while (output.size() < MAX_PROBE_BYTES) {
                        val count = input.read(
                            buffer,
                            0,
                            minOf(buffer.size, MAX_PROBE_BYTES - output.size()),
                        )
                        if (count <= 0) {
                            break
                        }
                        output.write(buffer, 0, count)
                    }
                    output.toByteArray()
                }
                return DlnaMediaProbeParser.parse(
                    prefix = bytes,
                    headers = connection.headerFields.entries
                        .filter { it.key != null }
                        .associate { entry ->
                            entry.key!!.lowercase(Locale.ROOT) to entry.value
                        },
                    responseCode = responseCode,
                    durationMs = durationMs,
                )
            } finally {
                connection.disconnect()
            }
        }
        return DlnaProbedQuality()
    }

    private companion object {
        const val CONNECT_TIMEOUT_MS = 2_500
        const val READ_TIMEOUT_MS = 3_500
        const val MAX_PROBE_BYTES = 256 * 1024
        const val MAX_REDIRECTS = 3
        const val USER_AGENT = "FusionPlay/1.0 DLNA-MediaProbe"
        val SUPPORTED_SCHEMES = setOf("http", "https")
    }
}

internal object DlnaMediaProbeParser {
    fun parse(
        prefix: ByteArray,
        headers: Map<String, List<String>>,
        responseCode: Int,
        durationMs: Long?,
    ): DlnaProbedQuality {
        val normalizedHeaders = headers.mapKeys { (name, _) ->
            name.lowercase(Locale.ROOT)
        }
        val rawContentType = header(normalizedHeaders, "content-type")
        val declaredContentType = rawContentType
            ?.substringBefore(';')
            ?.trim()
            ?.takeIf(String::isNotEmpty)
        val container = parseContainer(prefix)
        val headerBitrate = parseBitrateHeader(normalizedHeaders)
        val totalLength = responseTotalLength(
            headers = normalizedHeaders,
            responseCode = responseCode,
        )
        val averageBitrate = if (
            totalLength != null &&
            durationMs != null &&
            durationMs > 0L &&
            !isPlaylist(declaredContentType)
        ) {
            safeAverageBitrate(totalLength, durationMs)
        } else {
            null
        }
        val resolvedContentType =
            if (
                container.contentType == "application/mp4" &&
                !isGenericContentType(declaredContentType)
            ) {
                declaredContentType ?: container.contentType
            } else {
                container.contentType ?: declaredContentType
            }

        return DlnaProbedQuality(
            contentType = resolvedContentType,
            bitrateBps = container.bitrateBps
                ?: headerBitrate
                ?: averageBitrate,
            sampleRate = container.sampleRate
                ?: parseFrequencyHeader(normalizedHeaders)
                ?: parseContentTypePositiveInt(rawContentType, "rate"),
            bitsPerSample = container.bitsPerSample
                ?: parsePositiveIntHeader(
                    normalizedHeaders,
                    "x-audio-bits",
                    "x-bits-per-sample",
                    "bits-per-sample",
                )
                ?: parseContentTypeBitDepth(
                    declaredContentType,
                    rawContentType,
                ),
            channels = container.channels
                ?: parsePositiveIntHeader(
                    normalizedHeaders,
                    "x-audio-channels",
                    "x-channels",
                    "audio-channels",
                )
                ?: parseContentTypePositiveInt(
                    rawContentType,
                    "channels",
                ),
        )
    }

    private fun parseContainer(bytes: ByteArray): DlnaProbedQuality {
        return parseWave(bytes)
            ?: parseFlac(bytes)
            ?: parseOgg(bytes)
            ?: parseAdts(bytes)
            ?: parseMpegAudio(bytes)
            ?: parseMp4(bytes)
            ?: DlnaProbedQuality()
    }

    private fun parseWave(bytes: ByteArray): DlnaProbedQuality? {
        if (
            bytes.size < 12 ||
            (!matches(bytes, 0, "RIFF") && !matches(bytes, 0, "RF64")) ||
            !matches(bytes, 8, "WAVE")
        ) {
            return null
        }
        var offset = 12
        while (offset <= bytes.size - 8) {
            val size = u32le(bytes, offset + 4) ?: return null
            val dataOffset = offset + 8
            if (matches(bytes, offset, "fmt ") && size >= 16) {
                val channels = u16le(bytes, dataOffset + 2)
                val sampleRate = u32le(bytes, dataOffset + 4)
                val byteRate = u32le(bytes, dataOffset + 8)
                val bits = u16le(bytes, dataOffset + 14)
                return DlnaProbedQuality(
                    contentType = "audio/wav",
                    bitrateBps = byteRate
                        ?.takeIf { it > 0L }
                        ?.let(::saturatingBytesToBits),
                    sampleRate = sampleRate
                        ?.takeIf { it in 1..Int.MAX_VALUE.toLong() }
                        ?.toInt(),
                    bitsPerSample = bits?.takeIf { it > 0 },
                    channels = channels?.takeIf { it > 0 },
                )
            }
            val paddedSize = size + (size and 1L)
            if (
                dataOffset > bytes.size ||
                paddedSize > (bytes.size - dataOffset).toLong()
            ) {
                break
            }
            offset = dataOffset + paddedSize.toInt()
        }
        return DlnaProbedQuality(contentType = "audio/wav")
    }

    private fun parseFlac(bytes: ByteArray): DlnaProbedQuality? {
        if (!matches(bytes, 0, "fLaC")) {
            return null
        }
        var offset = 4
        while (offset <= bytes.size - 4) {
            val type = bytes[offset].toInt() and 0x7f
            val length =
                ((bytes[offset + 1].toInt() and 0xff) shl 16) or
                    ((bytes[offset + 2].toInt() and 0xff) shl 8) or
                    (bytes[offset + 3].toInt() and 0xff)
            val dataOffset = offset + 4
            if (type == 0 && length >= 34 && dataOffset + 18 <= bytes.size) {
                var packed = 0L
                for (index in dataOffset + 10 until dataOffset + 18) {
                    packed = (packed shl 8) or
                        (bytes[index].toLong() and 0xff)
                }
                val sampleRate = ((packed ushr 44) and 0xfffff).toInt()
                val channels = (((packed ushr 41) and 0x7) + 1).toInt()
                val bits = (((packed ushr 36) and 0x1f) + 1).toInt()
                return DlnaProbedQuality(
                    contentType = "audio/flac",
                    sampleRate = sampleRate.takeIf { it > 0 },
                    bitsPerSample = bits.takeIf { it > 0 },
                    channels = channels.takeIf { it > 0 },
                )
            }
            if (
                dataOffset > bytes.size ||
                length > bytes.size - dataOffset
            ) {
                break
            }
            offset = dataOffset + length
        }
        return DlnaProbedQuality(contentType = "audio/flac")
    }

    private fun parseOgg(bytes: ByteArray): DlnaProbedQuality? {
        if (!matches(bytes, 0, "OggS")) {
            return null
        }
        val opus = indexOf(bytes, "OpusHead".encodeToByteArray())
        if (opus >= 0 && opus + 10 <= bytes.size) {
            return DlnaProbedQuality(
                contentType = "audio/opus",
                sampleRate = 48_000,
                channels = (bytes[opus + 9].toInt() and 0xff)
                    .takeIf { it > 0 },
            )
        }
        val vorbis = indexOf(
            bytes,
            byteArrayOf(1) + "vorbis".encodeToByteArray(),
        )
        if (vorbis >= 0 && vorbis + 28 <= bytes.size) {
            val nominalBitrate = u32le(bytes, vorbis + 20)
            return DlnaProbedQuality(
                contentType = "audio/ogg",
                bitrateBps = nominalBitrate?.takeIf { it > 0L },
                sampleRate = u32le(bytes, vorbis + 12)
                    ?.takeIf { it in 1..Int.MAX_VALUE.toLong() }
                    ?.toInt(),
                channels = (bytes[vorbis + 11].toInt() and 0xff)
                    .takeIf { it > 0 },
            )
        }
        return DlnaProbedQuality(contentType = "audio/ogg")
    }

    private fun parseAdts(bytes: ByteArray): DlnaProbedQuality? {
        val sampleRates = intArrayOf(
            96_000,
            88_200,
            64_000,
            48_000,
            44_100,
            32_000,
            24_000,
            22_050,
            16_000,
            12_000,
            11_025,
            8_000,
            7_350,
        )
        for (offset in 0 until minOf(bytes.size - 6, 64 * 1024)) {
            if (
                (bytes[offset].toInt() and 0xff) != 0xff ||
                (bytes[offset + 1].toInt() and 0xf6) != 0xf0
            ) {
                continue
            }
            val sampleIndex = (bytes[offset + 2].toInt() ushr 2) and 0x0f
            val channels =
                ((bytes[offset + 2].toInt() and 1) shl 2) or
                    ((bytes[offset + 3].toInt() ushr 6) and 3)
            return DlnaProbedQuality(
                contentType = "audio/aac",
                sampleRate = sampleRates.getOrNull(sampleIndex),
                channels = channels.takeIf { it > 0 },
            )
        }
        return null
    }

    private fun parseMpegAudio(bytes: ByteArray): DlnaProbedQuality? {
        var offset = if (matches(bytes, 0, "ID3") && bytes.size >= 10) {
            10 + synchsafeInt(bytes, 6)
        } else {
            0
        }
        val limit = minOf(bytes.size - 4, offset + 128 * 1024)
        while (offset <= limit) {
            val header = u32be(bytes, offset) ?: break
            if ((header and 0xffe00000L) != 0xffe00000L) {
                offset++
                continue
            }
            val versionBits = ((header ushr 19) and 3).toInt()
            val layerBits = ((header ushr 17) and 3).toInt()
            val bitrateIndex = ((header ushr 12) and 0x0f).toInt()
            val sampleIndex = ((header ushr 10) and 3).toInt()
            if (
                versionBits == 1 ||
                layerBits == 0 ||
                bitrateIndex == 0 ||
                bitrateIndex == 15 ||
                sampleIndex == 3
            ) {
                offset++
                continue
            }
            val bitrateKbps = mpegBitrateKbps(
                versionBits,
                layerBits,
                bitrateIndex,
            )
            if (bitrateKbps == null) {
                offset++
                continue
            }
            val baseRate = intArrayOf(44_100, 48_000, 32_000)[sampleIndex]
            val sampleRate = when (versionBits) {
                3 -> baseRate
                2 -> baseRate / 2
                else -> baseRate / 4
            }
            val channelMode = ((header ushr 6) and 3).toInt()
            return DlnaProbedQuality(
                contentType = "audio/mpeg",
                bitrateBps = bitrateKbps * 1_000L,
                sampleRate = sampleRate,
                channels = if (channelMode == 3) 1 else 2,
            )
        }
        return null
    }

    private fun parseMp4(bytes: ByteArray): DlnaProbedQuality? {
        if (bytes.size < 12 || !matches(bytes, 4, "ftyp")) {
            return null
        }
        // `ftyp` alone does not distinguish an audio-only M4A from an MP4
        // containing video. Keep the container type neutral rather than
        // claiming an audio codec that was not observed.
        return DlnaProbedQuality(contentType = "application/mp4")
    }

    private fun mpegBitrateKbps(
        versionBits: Int,
        layerBits: Int,
        index: Int,
    ): Long? {
        val table = when {
            versionBits == 3 && layerBits == 3 ->
                intArrayOf(0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448)
            versionBits == 3 && layerBits == 2 ->
                intArrayOf(0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384)
            versionBits == 3 ->
                intArrayOf(0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320)
            layerBits == 3 ->
                intArrayOf(0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256)
            else ->
                intArrayOf(0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160)
        }
        return table.getOrNull(index)?.toLong()?.takeIf { it > 0L }
    }

    private fun parseBitrateHeader(
        headers: Map<String, List<String>>,
    ): Long? {
        val icyKbps = header(headers, "icy-br")
            ?.trim()
            ?.toDoubleOrNull()
            ?.takeIf { it > 0.0 }
            ?.let { safePositiveLong(it * 1_000.0) }
        if (icyKbps != null) {
            return icyKbps
        }
        return parsePositiveLongHeader(
            headers,
            "x-audio-bitrate",
            "x-bitrate",
            "audio-bitrate",
        )
    }

    private fun parseFrequencyHeader(
        headers: Map<String, List<String>>,
    ): Int? {
        val value = firstHeader(
            headers,
            "x-audio-sample-rate",
            "x-sample-rate",
            "sample-rate",
        ) ?: return null
        val normalized = value.trim().lowercase(Locale.ROOT)
        val number = decimalPrefix(normalized) ?: return null
        val hertz = if ("khz" in normalized) number * 1_000.0 else number
        return safePositiveLong(hertz)
            ?.takeIf { it <= Int.MAX_VALUE }
            ?.toInt()
    }

    private fun parsePositiveIntHeader(
        headers: Map<String, List<String>>,
        vararg names: String,
    ): Int? = firstHeader(headers, *names)
        ?.let(::decimalPrefix)
        ?.let(::safePositiveLong)
        ?.takeIf { it <= Int.MAX_VALUE }
        ?.toInt()

    private fun parsePositiveLongHeader(
        headers: Map<String, List<String>>,
        vararg names: String,
    ): Long? = firstHeader(headers, *names)
        ?.let(::decimalPrefix)
        ?.let(::safePositiveLong)

    private fun parseContentTypePositiveInt(
        contentType: String?,
        parameterName: String,
    ): Int? = contentTypeParameter(contentType, parameterName)
        ?.let(::decimalPrefix)
        ?.let(::safePositiveLong)
        ?.takeIf { it <= Int.MAX_VALUE }
        ?.toInt()

    private fun parseContentTypeBitDepth(
        declaredContentType: String?,
        rawContentType: String?,
    ): Int? {
        val subtypeDepth = Regex(
            pattern = """(?i)^audio/l(\d{1,2})$""",
        ).matchEntire(declaredContentType.orEmpty())
            ?.groupValues
            ?.getOrNull(1)
            ?.toIntOrNull()
            ?.takeIf { it > 0 }
        if (subtypeDepth != null) {
            return subtypeDepth
        }
        val format = contentTypeParameter(rawContentType, "format")
            ?: return null
        return Regex(
            pattern = """(?i)^[suf](\d{1,2})(?:le|be)?$""",
        ).matchEntire(format)
            ?.groupValues
            ?.getOrNull(1)
            ?.toIntOrNull()
            ?.takeIf { it > 0 }
    }

    private fun contentTypeParameter(
        contentType: String?,
        parameterName: String,
    ): String? = contentType
        ?.split(';')
        ?.asSequence()
        ?.drop(1)
        ?.mapNotNull { field ->
            val separator = field.indexOf('=')
            if (separator <= 0) {
                null
            } else {
                val name = field.substring(0, separator).trim()
                val value = field.substring(separator + 1)
                    .trim()
                    .trim('"')
                value.takeIf {
                    name.equals(parameterName, ignoreCase = true) &&
                        it.isNotEmpty()
                }
            }
        }
        ?.firstOrNull()

    private fun responseTotalLength(
        headers: Map<String, List<String>>,
        responseCode: Int,
    ): Long? {
        val rangeTotal = header(headers, "content-range")
            ?.substringAfterLast('/', "")
            ?.trim()
            ?.takeUnless { it == "*" }
            ?.toLongOrNull()
            ?.takeIf { it > 0L }
        if (rangeTotal != null) {
            return rangeTotal
        }
        if (responseCode != HttpURLConnection.HTTP_OK) {
            return null
        }
        return header(headers, "content-length")
            ?.trim()
            ?.toLongOrNull()
            ?.takeIf { it > 0L }
    }

    private fun safeAverageBitrate(
        totalBytes: Long,
        durationMs: Long,
    ): Long? {
        if (totalBytes <= 0L || durationMs <= 0L) {
            return null
        }
        val value = totalBytes.toDouble() * 8_000.0 / durationMs.toDouble()
        return safePositiveLong(value)
    }

    private fun saturatingBytesToBits(value: Long): Long =
        if (value > Long.MAX_VALUE / 8L) Long.MAX_VALUE else value * 8L

    private fun safePositiveLong(value: Double): Long? {
        if (!value.isFinite() || value <= 0.0) {
            return null
        }
        return if (value >= Long.MAX_VALUE.toDouble()) {
            Long.MAX_VALUE
        } else {
            value.roundToLong()
        }
    }

    private fun decimalPrefix(value: String): Double? {
        val number = value
            .trim()
            .replace(",", "")
            .takeWhile { it.isDigit() || it == '.' }
        return number.toDoubleOrNull()?.takeIf { it.isFinite() && it > 0.0 }
    }

    private fun isPlaylist(contentType: String?): Boolean {
        val value = contentType?.lowercase(Locale.ROOT).orEmpty()
        return "mpegurl" in value ||
            "m3u" in value ||
            "dash+xml" in value
    }

    private fun isGenericContentType(contentType: String?): Boolean =
        contentType.isNullOrBlank() ||
            contentType.equals(
                "application/octet-stream",
                ignoreCase = true,
            ) ||
            contentType.equals(
                "application/octetstream",
                ignoreCase = true,
            )

    private fun header(
        headers: Map<String, List<String>>,
        name: String,
    ): String? = headers[name.lowercase(Locale.ROOT)]
        ?.firstOrNull()
        ?.takeIf(String::isNotBlank)

    private fun firstHeader(
        headers: Map<String, List<String>>,
        vararg names: String,
    ): String? = names.firstNotNullOfOrNull { header(headers, it) }

    private fun matches(bytes: ByteArray, offset: Int, text: String): Boolean {
        if (
            offset < 0 ||
            text.length > bytes.size ||
            offset > bytes.size - text.length
        ) {
            return false
        }
        return text.indices.all { index ->
            bytes[offset + index].toInt() ==
                text[index].code
        }
    }

    private fun indexOf(bytes: ByteArray, needle: ByteArray): Int {
        if (needle.isEmpty() || bytes.size < needle.size) {
            return -1
        }
        for (offset in 0..bytes.size - needle.size) {
            if (needle.indices.all { bytes[offset + it] == needle[it] }) {
                return offset
            }
        }
        return -1
    }

    private fun u16le(bytes: ByteArray, offset: Int): Int? {
        if (offset < 0 || bytes.size < 2 || offset > bytes.size - 2) {
            return null
        }
        return (bytes[offset].toInt() and 0xff) or
            ((bytes[offset + 1].toInt() and 0xff) shl 8)
    }

    private fun u32le(bytes: ByteArray, offset: Int): Long? {
        if (offset < 0 || bytes.size < 4 || offset > bytes.size - 4) {
            return null
        }
        return (bytes[offset].toLong() and 0xff) or
            ((bytes[offset + 1].toLong() and 0xff) shl 8) or
            ((bytes[offset + 2].toLong() and 0xff) shl 16) or
            ((bytes[offset + 3].toLong() and 0xff) shl 24)
    }

    private fun u32be(bytes: ByteArray, offset: Int): Long? {
        if (offset < 0 || bytes.size < 4 || offset > bytes.size - 4) {
            return null
        }
        return ((bytes[offset].toLong() and 0xff) shl 24) or
            ((bytes[offset + 1].toLong() and 0xff) shl 16) or
            ((bytes[offset + 2].toLong() and 0xff) shl 8) or
            (bytes[offset + 3].toLong() and 0xff)
    }

    private fun synchsafeInt(bytes: ByteArray, offset: Int): Int {
        if (offset < 0 || bytes.size < 4 || offset > bytes.size - 4) {
            return 0
        }
        return ((bytes[offset].toInt() and 0x7f) shl 21) or
            ((bytes[offset + 1].toInt() and 0x7f) shl 14) or
            ((bytes[offset + 2].toInt() and 0x7f) shl 7) or
            (bytes[offset + 3].toInt() and 0x7f)
    }
}
