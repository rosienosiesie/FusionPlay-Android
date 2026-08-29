package com.airplayreceiver.desktop.backend

import android.os.Build
import android.provider.Settings
import java.net.InetAddress

/**
 * Resolves the device identity shared by AirPlay, DLNA and MiPlay.
 *
 * On Android this is the system device name. The account/user name and
 * product name are deliberately never used.
 */
internal object WindowsComputerName {
    fun current(): String = resolve(
        physicalNetbiosName = null,
        environmentComputerName = null,
        physicalDnsHostname = androidDeviceName(),
        localHostName = runCatching { InetAddress.getLocalHost().hostName }.getOrNull(),
    )

    internal fun resolve(
        physicalNetbiosName: String?,
        environmentComputerName: String?,
        physicalDnsHostname: String? = null,
        localHostName: String? = null,
    ): String {
        val resolved = sequenceOf(
            physicalDnsHostname,
            physicalNetbiosName,
            environmentComputerName,
            localHostName,
        )
            .mapNotNull { it?.trim()?.trimEnd('.') }
            .firstOrNull { it.isNotEmpty() }
            ?: throw IllegalStateException(
                "Device name is unavailable; receiver discovery cannot start.",
            )
        return resolved.take(AppSettings.MAX_RECEIVER_NAME_LENGTH)
    }

    private fun androidDeviceName(): String? {
        val resolver = runCatching { AndroidPaths.context().contentResolver }.getOrNull()
        val settingsName = resolver?.let {
            sequenceOf(
                runCatching { Settings.Global.getString(it, "device_name") }.getOrNull(),
                runCatching { Settings.Secure.getString(it, "bluetooth_name") }.getOrNull(),
            ).firstOrNull { name -> !name.isNullOrBlank() }
        }
        if (!settingsName.isNullOrBlank()) {
            return settingsName
        }
        val model = Build.MODEL?.trim().orEmpty()
        val manufacturer = Build.MANUFACTURER?.trim().orEmpty()
        return when {
            model.isNotEmpty() &&
                manufacturer.isNotEmpty() &&
                !model.startsWith(manufacturer, ignoreCase = true) ->
                "$manufacturer $model"
            model.isNotEmpty() -> model
            else -> Build.DEVICE?.trim()?.takeIf { it.isNotEmpty() }
        }
    }
}
