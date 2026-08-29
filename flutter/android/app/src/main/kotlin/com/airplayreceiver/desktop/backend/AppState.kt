package com.airplayreceiver.desktop.backend

import java.time.Instant

enum class AppLogLevel {
    INFO,
    WARNING,
    ERROR,
}

data class AppLogLine(
    val timestamp: Instant = Instant.now(),
    val level: AppLogLevel,
    val message: String,
)

data class OutputDeviceState(
    val id: String,
    val name: String,
    val isDefault: Boolean,
    val sampleRate: Int? = null,
    val channels: Int? = null,
    val sampleFormat: String? = null,
    val bitsPerSample: Int? = null,
)

data class XiaomiNetworkAdapterState(
    val id: String,
    val name: String,
    val description: String,
    val interfaceType: String,
    val interfaceIndex: Int,
    val ipv4Address: String?,
    val macAddress: String?,
    val isUp: Boolean,
    val classification: String,
    val autoEligible: Boolean,
    val manualEligible: Boolean,
    val isDefaultRoute: Boolean,
    val warning: String? = null,
) {
    val supportsMiPlayTransport: Boolean
        get() = classification.trim().lowercase() in setOf(
            "physical_ethernet",
            "physicalethernet",
            "physical_wifi",
            "physicalwifi",
        )

    val miPlayEligible: Boolean
        get() = manualEligible && supportsMiPlayTransport

    val isPhysicalEthernet: Boolean
        get() = classification.trim().lowercase() in setOf(
            "physical_ethernet",
            "physicalethernet",
        )
}

data class PlaybackSnapshot(
    val title: String? = null,
    val artist: String? = null,
    val album: String? = null,
    val genre: String? = null,
    val coverArt: String? = null,
    val mediaUrl: String? = null,
    val mediaKind: String? = null,
    val protocol: String? = null,
    val qualityText: String? = null,
    val durationMs: Long? = null,
    val positionMs: Long = 0,
    val isPlaying: Boolean = false,
    val streamActive: Boolean = false,
    val sourceEpoch: Long? = null,
    val trackIdentity: String? = null,
    val lyrics: LyricsDocument? = null,
    val lyricsLoading: Boolean = false,
)

data class RemoteControlState(
    val available: Boolean = false,
    val commands: Set<String> = emptySet(),
    val transport: String? = null,
    val experimental: Boolean = false,
)

data class SourcePlaybackState(
    val playback: PlaybackSnapshot = PlaybackSnapshot(),
    val remoteControl: RemoteControlState = RemoteControlState(),
)

data class AppState(
    val initialized: Boolean = false,
    val busy: Boolean = false,
    val settings: AppSettings = AppSettings(),
    val startupRegistered: Boolean = false,
    val coreRunning: Boolean = false,
    val receiverReady: Boolean = false,
    val receiverPort: Int? = null,
    val receiverDeviceId: String? = null,
    val connectedClient: String? = null,
    val outputDevices: List<OutputDeviceState> = emptyList(),
    val xiaomiNetworkAdapters: List<XiaomiNetworkAdapterState> = emptyList(),
    val xiaomiNetworkAdaptersLoading: Boolean = false,
    val xiaomiAutoSelectedAdapterId: String? = null,
    val playback: PlaybackSnapshot = PlaybackSnapshot(),
    val remoteControl: RemoteControlState = RemoteControlState(),
    val activeMediaSource: MediaSource? = null,
    val selectedCoreMediaSource: MediaSource? = null,
    val sourcePlaybackStates: Map<MediaSource, SourcePlaybackState> = emptyMap(),
    val lastEvent: AppEvent? = null,
    val lastError: String? = null,
    val logs: List<AppLogLine> = emptyList(),
) {
    companion object {
        const val MAX_LOG_LINES = 300
    }
}
