package com.airplayreceiver.desktop.backend

import java.nio.file.Files
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AdvancedEffectsSettingsTest {
    @Test
    fun defaultsOffAndPersistsExplicitChoice() = runBlocking {
        val directory = Files.createTempDirectory("fusionplay-effects-test-")
        val settingsPath = directory.resolve("settings.json")
        try {
            val store = SettingsStore(settingsPath)
            assertFalse(store.load().advancedEffectsEnabled)

            store.save(AppSettings(advancedEffectsEnabled = true))
            assertTrue(store.load().advancedEffectsEnabled)
        } finally {
            Files.deleteIfExists(settingsPath)
            Files.deleteIfExists(directory)
        }
    }
}
