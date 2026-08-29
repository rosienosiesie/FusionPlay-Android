package com.airplayreceiver.desktop.backend

enum class MiPlayDeviceIdentity(
    val persistedValue: String,
    val protocolValue: Int,
    val displayName: String,
) {
    VEHICLE("vehicle", 5, "车机"),
    TELEVISION("television", 2, "电视"),
    TABLET("tablet", 18, "平板"),
    SPEAKER("speaker", 4, "音响"),
    ;

    companion object {
        fun fromPersistedValue(value: String?): MiPlayDeviceIdentity {
            val normalizedValue = value?.trim()
            if (normalizedValue.equals("display_speaker", ignoreCase = true)) {
                return SPEAKER
            }
            return entries.firstOrNull {
                it.persistedValue.equals(normalizedValue, ignoreCase = true)
            } ?: TELEVISION
        }
    }
}

/**
 * User-controlled settings persisted below %LOCALAPPDATA%\AirPlayReceiver.
 *
 * Startup is intentionally enabled by default. [WindowsStartupManager] writes
 * the default to disk before registering the Run entry, so a later explicit
 * opt-out remains durable across launches.
 */
data class AppSettings(
    val schemaVersion: Int = CURRENT_SCHEMA_VERSION,
    val receiverName: String? = null,
    val outputDeviceId: String? = null,
    val xiaomiNetworkAdapterId: String? = null,
    val startupEnabled: Boolean = true,
    val autoWakeEnabled: Boolean = true,
    val advancedEffectsEnabled: Boolean = false,
    val miPlayEnabled: Boolean = true,
    val miPlayDeviceIdentity: MiPlayDeviceIdentity =
        MiPlayDeviceIdentity.TELEVISION,
    val airPlayEnabled: Boolean = true,
    val dlnaEnabled: Boolean = true,
) {
    fun validated(): AppSettings {
        require(schemaVersion in OLDEST_SUPPORTED_SCHEMA_VERSION..CURRENT_SCHEMA_VERSION) {
            "Unsupported settings schema version: $schemaVersion."
        }
        val normalizedReceiverName = receiverName
            ?.trim()
            ?.takeIf(String::isNotEmpty)
        require(
            normalizedReceiverName == null ||
                normalizedReceiverName.length <= MAX_RECEIVER_NAME_LENGTH,
        ) {
            "Receiver name must not exceed $MAX_RECEIVER_NAME_LENGTH characters."
        }
        return copy(
            schemaVersion = CURRENT_SCHEMA_VERSION,
            receiverName = normalizedReceiverName,
            outputDeviceId = outputDeviceId?.trim()?.takeIf(String::isNotEmpty),
            xiaomiNetworkAdapterId =
                xiaomiNetworkAdapterId?.trim()?.takeIf(String::isNotEmpty),
        )
    }

    companion object {
        const val OLDEST_SUPPORTED_SCHEMA_VERSION = 1
        const val CURRENT_SCHEMA_VERSION = 8
        const val MAX_RECEIVER_NAME_LENGTH = 63
    }
}

data class SettingsLoadResult(
    val settings: AppSettings,
    val existed: Boolean,
)
