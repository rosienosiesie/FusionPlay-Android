package com.airplayreceiver.desktop.backend

import java.nio.file.Files
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AutoWakeSettingsTest {
    @Test
    fun defaultsOnAndPersistsExplicitChoice() = runBlocking {
        val directory = Files.createTempDirectory("fusionplay-auto-wake-test-")
        val settingsPath = directory.resolve("settings.json")
        try {
            val store = SettingsStore(settingsPath)
            assertTrue(store.load().autoWakeEnabled)

            store.save(AppSettings(autoWakeEnabled = false))
            assertFalse(store.load().autoWakeEnabled)
        } finally {
            Files.deleteIfExists(settingsPath)
            Files.deleteIfExists(directory)
        }
    }
}
