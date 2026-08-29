package com.airplayreceiver.desktop.backend

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

data class StartupInitialization(
    val settings: AppSettings,
    val firstRun: Boolean,
    val registrationEnabled: Boolean,
)

/**
 * Persists the startup preference. On Android the enabled accessibility
 * service performs the actual once-per-boot foreground launch.
 */
class WindowsStartupManager(
    private val settingsStore: SettingsStore,
) {
    private val mutex = Mutex()

    suspend fun initialize(): StartupInitialization = mutex.withLock {
        val loaded = settingsStore.loadWithStatus()
        if (!loaded.existed) {
            settingsStore.save(loaded.settings)
        }
        StartupInitialization(
            settings = loaded.settings,
            firstRun = !loaded.existed,
            registrationEnabled = loaded.settings.startupEnabled,
        )
    }

    suspend fun setEnabled(enabled: Boolean): AppSettings = mutex.withLock {
        settingsStore.update { it.copy(startupEnabled = enabled) }
    }

    suspend fun isRegistrationEnabled(): Boolean = mutex.withLock {
        withContext(Dispatchers.IO) {
            settingsStore.load().startupEnabled
        }
    }
}
