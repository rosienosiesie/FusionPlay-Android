package com.fusionplay.android.media

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.app.PendingIntent
import android.os.Build
import android.os.SystemClock
import android.util.Base64
import android.util.Log
import android.support.v4.media.MediaMetadataCompat
import android.support.v4.media.session.MediaSessionCompat
import android.support.v4.media.session.PlaybackStateCompat
import androidx.core.graphics.scale
import androidx.core.net.toUri
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.URI
import java.net.URL
import java.nio.file.Path
import java.util.concurrent.CopyOnWriteArraySet
import kotlin.math.abs
import kotlin.math.roundToInt
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.withContext

enum class FusionPlayMediaCommand {
    PLAY,
    PAUSE,
    PREVIOUS,
    NEXT,
    SEEK,
}

data class FusionPlayMediaCommandEvent(
    val command: FusionPlayMediaCommand,
    val positionMs: Long? = null,
)

data class FusionPlayMediaSnapshot(
    val title: String? = null,
    val artist: String? = null,
    val album: String? = null,
    val mediaIdentity: String? = null,
    val playing: Boolean = false,
    val positionMs: Long = 0L,
    val durationMs: Long = 0L,
    val canPlayPause: Boolean = false,
    val canPrevious: Boolean = false,
    val canNext: Boolean = false,
    val canSeek: Boolean = false,
) {
    val hasMedia: Boolean
        get() = mediaIdentity != null || title != null || artist != null || album != null
}

/**
 * The single Android media endpoint owned by FusionPlay.
 *
 * Protocol receivers never register their own Android media sessions. They
 * publish into FusionPlay's existing state reducer, and the app projects that
 * final state into this one MediaSession/audio-focus owner.
 */
object FusionPlayMediaChannel {
    private val stateLock = Any()
    private val stateListeners = CopyOnWriteArraySet<() -> Unit>()
    private val commandQueue = Channel<FusionPlayMediaCommandEvent>(
        capacity = MAX_PENDING_MEDIA_COMMANDS,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val commands: Flow<FusionPlayMediaCommandEvent> = commandQueue.receiveAsFlow()

    @Volatile
    private var initialized = false
    private lateinit var appContext: Context
    private lateinit var audioManager: AudioManager
    private lateinit var mediaSession: MediaSessionCompat
    private var audioFocusRequest: AudioFocusRequest? = null
    private val artworkRequests = LatestRequestGate()
    private var snapshot = FusionPlayMediaSnapshot()
    private var artwork: Bitmap? = null
    private var lastPublishedPlaybackSnapshot: FusionPlayMediaSnapshot? = null
    private var lastPlaybackStatePublishTimeMs: Long = 0L

    private val audioFocusListener = AudioManager.OnAudioFocusChangeListener { change ->
        if (
            change == AudioManager.AUDIOFOCUS_LOSS ||
            change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT
        ) {
            val shouldPause = synchronized(stateLock) { snapshot.playing }
            if (shouldPause) {
                dispatch(FusionPlayMediaCommand.PAUSE)
            }
        }
    }

    fun initialize(context: Context) {
        if (initialized) return
        synchronized(stateLock) {
            if (initialized) return
            appContext = context.applicationContext
            audioManager = appContext.getSystemService(Context.AUDIO_SERVICE) as AudioManager
            mediaSession = MediaSessionCompat(appContext, MEDIA_SESSION_TAG).apply {
                setFlags(
                    MediaSessionCompat.FLAG_HANDLES_MEDIA_BUTTONS or
                        MediaSessionCompat.FLAG_HANDLES_TRANSPORT_CONTROLS,
                )
                setPlaybackToLocal(AudioManager.STREAM_MUSIC)
                appContext.packageManager
                    .getLaunchIntentForPackage(appContext.packageName)
                    ?.let { launchIntent ->
                        val flags = PendingIntent.FLAG_UPDATE_CURRENT or
                            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                                PendingIntent.FLAG_IMMUTABLE
                            } else {
                                0
                            }
                        setSessionActivity(
                            PendingIntent.getActivity(
                                appContext,
                                0,
                                launchIntent,
                                flags,
                            ),
                        )
                    }
                setCallback(object : MediaSessionCompat.Callback() {
                    override fun onPlay() = dispatch(FusionPlayMediaCommand.PLAY)

                    override fun onPause() = dispatch(FusionPlayMediaCommand.PAUSE)

                    override fun onStop() = dispatch(FusionPlayMediaCommand.PAUSE)

                    override fun onSkipToPrevious() =
                        dispatch(FusionPlayMediaCommand.PREVIOUS)

                    override fun onSkipToNext() =
                        dispatch(FusionPlayMediaCommand.NEXT)

                    override fun onSeekTo(pos: Long) =
                        dispatch(FusionPlayMediaCommand.SEEK, pos.coerceAtLeast(0L))
                })
                isActive = false
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                audioFocusRequest = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                    .setAudioAttributes(
                        AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_MEDIA)
                            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                            .build(),
                    )
                    .setOnAudioFocusChangeListener(audioFocusListener)
                    .setWillPauseWhenDucked(false)
                    .build()
            }
            initialized = true
            publishLocked()
        }
    }

    fun setMetadata(
        title: String?,
        artist: String?,
        album: String?,
        mediaIdentity: String?,
    ) {
        ensureInitialized()
        synchronized(stateLock) {
            snapshot = snapshot.copy(
                title = title.nonBlankOrNull(),
                artist = artist.nonBlankOrNull(),
                album = album.nonBlankOrNull(),
                mediaIdentity = mediaIdentity.nonBlankOrNull(),
            )
            publishLocked()
        }
        notifyStateChanged()
    }

    suspend fun setArtwork(path: Path) {
        val request = artworkRequests.begin()
        val decoded = decodeArtwork {
            decodeSampledFile(path.toAbsolutePath().normalize().toString())
        }
        commitArtwork(request, decoded)
    }

    suspend fun setArtwork(uri: URI) {
        val request = artworkRequests.begin()
        val decoded = decodeArtwork {
            when (uri.scheme?.lowercase()) {
                "http", "https" -> {
                    val connection = URL(uri.toString()).openConnection().apply {
                        connectTimeout = ARTWORK_TIMEOUT_MS
                        readTimeout = ARTWORK_TIMEOUT_MS
                        useCaches = false
                    }
                    // getContentLengthLong was added after Android 5; the
                    // Int-sized value is sufficient for this 8 MiB guard.
                    val declaredLength = connection.contentLength
                    require(declaredLength <= MAX_ARTWORK_BYTES || declaredLength < 0) {
                        "Artwork exceeds the ${MAX_ARTWORK_BYTES / (1024 * 1024)} MiB limit."
                    }
                    connection.getInputStream().use { input ->
                        decodeSampledBytes(input.readBytesLimited(MAX_ARTWORK_BYTES))
                    }
                }

                "content" -> appContext.contentResolver
                    .openInputStream(uri.toString().toUri())
                    ?.use { input ->
                        decodeSampledBytes(input.readBytesLimited(MAX_ARTWORK_BYTES))
                    }

                "file", null -> decodeSampledFile(
                    if (uri.scheme == null) uri.toString() else Path.of(uri).toString(),
                )

                else -> null
            }
        }
        commitArtwork(request, decoded)
    }

    suspend fun setArtworkDataUri(dataUri: String) {
        val request = artworkRequests.begin()
        val decoded = decodeArtwork {
            val encoded = dataUri.substringAfter(',', missingDelimiterValue = "")
            if (encoded.isEmpty() || encoded.length > MAX_ARTWORK_BASE64_CHARS) {
                null
            } else {
                val bytes = Base64.decode(encoded, Base64.DEFAULT)
                if (bytes.size > MAX_ARTWORK_BYTES) null else decodeSampledBytes(bytes)
            }
        }
        commitArtwork(request, decoded)
    }

    fun clearArtwork() {
        artworkRequests.invalidate()
        setArtworkBitmap(null)
    }

    fun setPlayback(playing: Boolean) {
        ensureInitialized()
        if (playing) requestAudioFocus() else abandonAudioFocus()
        synchronized(stateLock) {
            snapshot = snapshot.copy(playing = playing)
            publishLocked()
        }
        notifyStateChanged()
    }

    fun setTimeline(positionMs: Long, durationMs: Long) {
        ensureInitialized()
        synchronized(stateLock) {
            val normalizedDuration = durationMs.coerceAtLeast(0L)
            val durationChanged = snapshot.durationMs != normalizedDuration
            snapshot = snapshot.copy(
                positionMs = positionMs.coerceAtLeast(0L),
                durationMs = normalizedDuration,
            )
            if (durationChanged) {
                publishMetadataLocked()
            }
            publishPlaybackStateLocked(force = durationChanged)
        }
    }

    fun setCapabilities(
        canPlayPause: Boolean,
        canPrevious: Boolean,
        canNext: Boolean,
        canSeek: Boolean,
    ) {
        ensureInitialized()
        synchronized(stateLock) {
            snapshot = snapshot.copy(
                canPlayPause = canPlayPause,
                canPrevious = canPrevious,
                canNext = canNext,
                canSeek = canSeek,
            )
            publishLocked()
        }
        notifyStateChanged()
    }

    fun clear() {
        if (!initialized) return
        abandonAudioFocus()
        synchronized(stateLock) {
            artwork = null
            snapshot = FusionPlayMediaSnapshot()
            publishLocked()
        }
        notifyStateChanged()
    }

    fun currentSnapshot(): FusionPlayMediaSnapshot =
        synchronized(stateLock) { snapshot }

    fun sessionToken(): MediaSessionCompat.Token? =
        if (initialized) mediaSession.sessionToken else null

    fun currentSystemMediaVolumePercent(): Int {
        ensureInitialized()
        val range = mediaVolumeRange()
        if (range.last <= range.first) return 100
        val current = audioManager
            .getStreamVolume(AudioManager.STREAM_MUSIC)
            .coerceIn(range)
        return (((current - range.first) * 100f) /
            (range.last - range.first))
            .roundToInt()
            .coerceIn(0, 100)
    }

    /**
     * Applies a sender-provided remote volume to Android's real media stream.
     * No UI flags are used, so repeated MiPlay volume frames do not keep the
     * system volume panel visible on the receiver.
     */
    fun setSystemMediaVolumePercent(percent: Int): Boolean {
        ensureInitialized()
        if (audioManager.isVolumeFixed) return false
        val range = mediaVolumeRange()
        val index = volumeIndexForPercent(percent, range.first, range.last)
        return runCatching {
            if (audioManager.getStreamVolume(AudioManager.STREAM_MUSIC) != index) {
                audioManager.setStreamVolume(
                    AudioManager.STREAM_MUSIC,
                    index,
                    0,
                )
            }
            audioManager.getStreamVolume(AudioManager.STREAM_MUSIC) == index
        }.getOrDefault(false)
    }

    fun addStateListener(listener: () -> Unit) {
        stateListeners += listener
    }

    fun removeStateListener(listener: () -> Unit) {
        stateListeners -= listener
    }

    fun dispatch(command: FusionPlayMediaCommand, positionMs: Long? = null) {
        val queued = commandQueue.trySend(
            FusionPlayMediaCommandEvent(command, positionMs),
        )
        if (queued.isFailure) {
            Log.e(
                MEDIA_SESSION_TAG,
                "Unable to enqueue Android media command $command.",
                queued.exceptionOrNull(),
            )
        }
    }

    private fun setArtworkBitmap(source: Bitmap?) {
        ensureInitialized()
        val normalized = source?.fitInside(MAX_ARTWORK_SIZE)
        synchronized(stateLock) {
            artwork = normalized
            publishMetadataLocked()
        }
        notifyStateChanged()
    }

    private fun commitArtwork(request: Long, decoded: Bitmap?) {
        val committed = artworkRequests.commitIfCurrent(request) {
            setArtworkBitmap(decoded)
        }
        if (!committed) decoded?.recycle()
    }

    private suspend fun decodeArtwork(block: () -> Bitmap?): Bitmap? {
        var decoded: Bitmap? = null
        try {
            return withContext(Dispatchers.IO) {
                block().also { decoded = it }
            }
        } catch (cancelled: CancellationException) {
            decoded?.recycle()
            throw cancelled
        }
    }

    private fun publishLocked() {
        publishMetadataLocked()
        publishPlaybackStateLocked(force = true)
    }

    private fun publishMetadataLocked() {
        val state = snapshot
        val metadata = MediaMetadataCompat.Builder().apply {
            state.title?.let {
                putString(MediaMetadataCompat.METADATA_KEY_TITLE, it)
                putString(MediaMetadataCompat.METADATA_KEY_DISPLAY_TITLE, it)
            }
            state.artist?.let {
                putString(MediaMetadataCompat.METADATA_KEY_ARTIST, it)
                putString(MediaMetadataCompat.METADATA_KEY_DISPLAY_SUBTITLE, it)
            }
            state.album?.let { putString(MediaMetadataCompat.METADATA_KEY_ALBUM, it) }
            state.mediaIdentity?.let {
                putString(MediaMetadataCompat.METADATA_KEY_MEDIA_ID, it)
            }
            if (state.durationMs > 0L) {
                putLong(MediaMetadataCompat.METADATA_KEY_DURATION, state.durationMs)
            }
            artwork?.let {
                putBitmap(MediaMetadataCompat.METADATA_KEY_ALBUM_ART, it)
                putBitmap(MediaMetadataCompat.METADATA_KEY_DISPLAY_ICON, it)
            }
        }.build()
        mediaSession.setMetadata(if (state.hasMedia) metadata else null)
    }

    private fun publishPlaybackStateLocked(force: Boolean) {
        val state = snapshot
        val now = SystemClock.elapsedRealtime()
        val lastState = lastPublishedPlaybackSnapshot
        if (!force && lastState != null) {
            val elapsedSincePublish =
                (now - lastPlaybackStatePublishTimeMs).coerceAtLeast(0L)
            val expectedPosition = if (lastState.playing) {
                lastState.positionMs + elapsedSincePublish
            } else {
                lastState.positionMs
            }
            val timelineJumped =
                abs(state.positionMs - expectedPosition) >= TIMELINE_JUMP_THRESHOLD_MS
            val publicationDue =
                elapsedSincePublish >= TIMELINE_PUBLISH_INTERVAL_MS
            if (
                !timelineJumped &&
                !publicationDue &&
                state.durationMs == lastState.durationMs &&
                state.playing == lastState.playing &&
                state.hasMedia == lastState.hasMedia
            ) {
                return
            }
        }

        var actions = 0L
        if (state.canPlayPause) {
            actions = actions or PlaybackStateCompat.ACTION_PLAY or
                PlaybackStateCompat.ACTION_PAUSE or
                PlaybackStateCompat.ACTION_PLAY_PAUSE
        }
        if (state.canPrevious) actions = actions or PlaybackStateCompat.ACTION_SKIP_TO_PREVIOUS
        if (state.canNext) actions = actions or PlaybackStateCompat.ACTION_SKIP_TO_NEXT
        if (state.canSeek) actions = actions or PlaybackStateCompat.ACTION_SEEK_TO
        val playbackState = when {
            !state.hasMedia -> PlaybackStateCompat.STATE_NONE
            state.playing -> PlaybackStateCompat.STATE_PLAYING
            else -> PlaybackStateCompat.STATE_PAUSED
        }
        mediaSession.setPlaybackState(
            PlaybackStateCompat.Builder()
                .setActions(actions)
                .setState(
                    playbackState,
                    state.positionMs,
                    if (state.playing) 1f else 0f,
                    now,
                )
                .build(),
        )
        mediaSession.isActive = state.hasMedia
        lastPublishedPlaybackSnapshot = state
        lastPlaybackStatePublishTimeMs = now
    }

    private fun requestAudioFocus() {
        if (!initialized) return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            audioFocusRequest?.let(audioManager::requestAudioFocus)
        } else {
            @Suppress("DEPRECATION")
            audioManager.requestAudioFocus(
                audioFocusListener,
                AudioManager.STREAM_MUSIC,
                AudioManager.AUDIOFOCUS_GAIN,
            )
        }
    }

    private fun abandonAudioFocus() {
        if (!initialized) return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            audioFocusRequest?.let(audioManager::abandonAudioFocusRequest)
        } else {
            @Suppress("DEPRECATION")
            audioManager.abandonAudioFocus(audioFocusListener)
        }
    }

    private fun notifyStateChanged() {
        stateListeners.forEach { listener -> runCatching(listener) }
    }

    private fun ensureInitialized() {
        check(initialized) { "FusionPlayMediaChannel.initialize must be called first." }
    }

    private fun mediaVolumeRange(): IntRange {
        val maximum = audioManager
            .getStreamMaxVolume(AudioManager.STREAM_MUSIC)
            .coerceAtLeast(0)
        val minimum = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            audioManager
                .getStreamMinVolume(AudioManager.STREAM_MUSIC)
                .coerceIn(0, maximum)
        } else {
            0
        }
        return minimum..maximum
    }

    private fun String?.nonBlankOrNull(): String? =
        this?.trim()?.takeIf(String::isNotEmpty)

    private fun Bitmap.fitInside(maxSize: Int): Bitmap {
        val largest = maxOf(width, height)
        if (largest <= maxSize) return this
        val scale = maxSize / largest.toFloat()
        val scaled = this.scale(
            (width * scale).toInt().coerceAtLeast(1),
            (height * scale).toInt().coerceAtLeast(1),
        )
        if (scaled !== this) recycle()
        return scaled
    }

    private fun decodeSampledFile(path: String): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeFile(path, bounds)
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
        return BitmapFactory.decodeFile(
            path,
            BitmapFactory.Options().apply {
                inSampleSize = sampledDecodeFactor(bounds.outWidth, bounds.outHeight)
            },
        )
    }

    private fun decodeSampledBytes(bytes: ByteArray): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
        return BitmapFactory.decodeByteArray(
            bytes,
            0,
            bytes.size,
            BitmapFactory.Options().apply {
                inSampleSize = sampledDecodeFactor(bounds.outWidth, bounds.outHeight)
            },
        )
    }

    private fun sampledDecodeFactor(width: Int, height: Int): Int {
        val largest = maxOf(width, height)
        var sample = 1
        while (largest / (sample * 2) >= MAX_ARTWORK_SIZE) sample *= 2
        return sample
    }

    private fun InputStream.readBytesLimited(limit: Int): ByteArray {
        val output = ByteArrayOutputStream(minOf(limit, 64 * 1024))
        val buffer = ByteArray(16 * 1024)
        var total = 0
        while (true) {
            val read = read(buffer)
            if (read < 0) break
            total += read
            require(total <= limit) {
                "Artwork exceeds the ${limit / (1024 * 1024)} MiB limit."
            }
            output.write(buffer, 0, read)
        }
        return output.toByteArray()
    }

    private const val MEDIA_SESSION_TAG = "FusionPlay"
    private const val MAX_ARTWORK_SIZE = 384
    private const val MAX_ARTWORK_BYTES = 8 * 1024 * 1024
    private const val MAX_ARTWORK_BASE64_CHARS = (MAX_ARTWORK_BYTES * 4 / 3) + 4
    private const val MAX_PENDING_MEDIA_COMMANDS = 32
    private const val ARTWORK_TIMEOUT_MS = 5_000
    private const val TIMELINE_PUBLISH_INTERVAL_MS = 1_000L
    private const val TIMELINE_JUMP_THRESHOLD_MS = 1_500L
}

internal class LatestRequestGate {
    private val lock = Any()
    private var generation = 0L

    fun begin(): Long = synchronized(lock) { ++generation }

    fun invalidate() {
        synchronized(lock) { ++generation }
    }

    fun commitIfCurrent(request: Long, action: () -> Unit): Boolean = synchronized(lock) {
        if (generation != request) return@synchronized false
        action()
        true
    }
}

internal fun volumeIndexForPercent(
    percent: Int,
    minimum: Int,
    maximum: Int,
): Int {
    require(minimum <= maximum) { "minimum volume must not exceed maximum" }
    if (minimum == maximum) return minimum
    return (minimum +
        (maximum - minimum) * percent.coerceIn(0, 100) / 100f)
        .roundToInt()
        .coerceIn(minimum, maximum)
}
