package com.airplayreceiver.desktop.backend

import org.junit.Assert.assertEquals
import org.junit.Test

class MiPlayDeviceIdentityTest {
    @Test
    fun optionsMatchXiaomiAudioDeviceTableAndRequestedOrder() {
        assertEquals(
            listOf(
                Triple("车机", "vehicle", 5),
                Triple("电视", "television", 2),
                Triple("平板", "tablet", 18),
                Triple("音响", "speaker", 4),
            ),
            MiPlayDeviceIdentity.entries.map {
                Triple(it.displayName, it.persistedValue, it.protocolValue)
            },
        )
    }

    @Test
    fun missingOrUnknownStoredIdentityFallsBackToTelevision() {
        assertEquals(
            MiPlayDeviceIdentity.TELEVISION,
            MiPlayDeviceIdentity.fromPersistedValue(null),
        )
        assertEquals(
            MiPlayDeviceIdentity.TELEVISION,
            MiPlayDeviceIdentity.fromPersistedValue("unknown"),
        )
    }

    @Test
    fun legacyDisplaySpeakerIdentityMigratesToSpeaker() {
        assertEquals(
            MiPlayDeviceIdentity.SPEAKER,
            MiPlayDeviceIdentity.fromPersistedValue("display_speaker"),
        )
    }
}
