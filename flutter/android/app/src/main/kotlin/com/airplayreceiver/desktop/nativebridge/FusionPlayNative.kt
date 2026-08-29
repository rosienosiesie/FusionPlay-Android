package com.airplayreceiver.desktop.nativebridge

import android.content.Context
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean

interface NativeCallback {
    fun onCoreEvent(json: String)
    fun onXiaomiEvent(json: String)
    fun onNativeLog(message: String, isError: Boolean)
}

object FusionPlayNative : NativeCallback {
    private val initialized = AtomicBoolean(false)
    private val listeners = CopyOnWriteArrayList<NativeCallback>()

    init {
        System.loadLibrary("fusionplay_core")
    }

    fun initialize(context: Context) {
        if (initialized.compareAndSet(false, true)) {
            nativeInit(context.applicationContext, this)
        }
    }

    fun addListener(listener: NativeCallback) {
        listeners += listener
    }

    fun removeListener(listener: NativeCallback) {
        listeners -= listener
    }

    override fun onCoreEvent(json: String) {
        listeners.forEach { it.onCoreEvent(json) }
    }

    override fun onXiaomiEvent(json: String) {
        listeners.forEach { it.onXiaomiEvent(json) }
    }

    override fun onNativeLog(message: String, isError: Boolean) {
        listeners.forEach { it.onNativeLog(message, isError) }
    }

    @JvmStatic
    private external fun nativeInit(context: Context, callback: NativeCallback)

    @JvmStatic
    external fun nativeStartCoreProtocol(
        protocol: String,
        name: String,
        statePath: String,
        outputDeviceId: String?,
    ): String?

    @JvmStatic
    external fun nativeStopCoreProtocol(protocol: String)

    @JvmStatic
    external fun nativeSendCoreCommand(target: String, json: String): Boolean

    @JvmStatic
    external fun nativeListNetworkAdapters(): String

    @JvmStatic
    external fun nativeStartMiPlay(
        receiverName: String,
        ipv4: String,
        interfaceName: String,
        hardwareAddress: String?,
        identityDir: String,
        outputDeviceId: String?,
        initialVolumePercent: Int,
        deviceType: Int,
    ): String?

    @JvmStatic
    external fun nativeStopMiPlay()

    @JvmStatic
    external fun nativeSuspendMiPlayOutput()

    @JvmStatic
    external fun nativeResumeMiPlayOutput()

    @JvmStatic
    external fun nativeSetMiPlayVolume(percent: Int): Boolean

    @JvmStatic
    external fun nativeControlMiPlay(action: String, positionMs: Long): String
}
