package com.airplayreceiver.desktop.backend

import android.content.Context
import java.nio.file.Path
import java.nio.file.Paths
import java.util.concurrent.atomic.AtomicReference

object AndroidPaths {
    private val filesDir = AtomicReference<Path?>()
    private val appContext = AtomicReference<android.content.Context?>()

    fun initialize(context: android.content.Context) {
        val app = context.applicationContext
        appContext.set(app)
        filesDir.set(Paths.get(app.filesDir.absolutePath))
    }

    fun context(): android.content.Context =
        appContext.get()
            ?: error("AndroidPaths.initialize() must run before FusionPlay stores files.")

    fun filesDirectory(): Path =
        filesDir.get()
            ?: error("AndroidPaths.initialize() must run before FusionPlay stores files.")

    fun identityDirectory(): Path = filesDirectory().resolve("Identity")

    fun stateDirectory(): Path = filesDirectory().resolve("AirPlayReceiver")

    fun coreStatePath(): Path = stateDirectory().resolve("airplay-state.json")
}
