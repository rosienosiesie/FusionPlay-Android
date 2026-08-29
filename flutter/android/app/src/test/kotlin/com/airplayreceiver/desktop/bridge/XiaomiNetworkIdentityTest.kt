package com.airplayreceiver.desktop.bridge

import kotlinx.serialization.json.Json
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Test

class XiaomiNetworkIdentityTest {
    @Test
    fun adapterMacAddressSurvivesNativeBridgeParsing() {
        val result = Json.parseToJsonElement(
            """
            {
              "auto_selected_adapter_id": "wlan0-12",
              "adapters": [
                {
                  "id": "wlan0-12",
                  "name": "wlan0",
                  "description": "wlan0",
                  "interface_type": "physical_wifi",
                  "interface_index": 12,
                  "ipv4_address": "192.168.31.207",
                  "mac_address": "A036BC250543",
                  "is_up": true,
                  "classification": "physical_wifi",
                  "auto_eligible": true,
                  "manual_eligible": true,
                  "is_default_route": false,
                  "warning": null
                }
              ]
            }
            """.trimIndent(),
        )

        val parsed = WindowsBridgeClient.parseXiaomiNetworkAdapterListResult(result)
        val adapter = parsed.adapters.single()
        assertEquals("wlan0-12", parsed.autoSelectedAdapterId)
        assertEquals("A036BC250543", adapter.macAddress)
        assertNotNull(adapter.ipv4Address)
    }
}
