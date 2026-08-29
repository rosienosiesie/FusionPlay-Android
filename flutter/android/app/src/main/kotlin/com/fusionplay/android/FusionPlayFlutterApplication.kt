package com.fusionplay.android

import android.app.Application
import com.airplayreceiver.desktop.backend.AndroidPaths
import com.airplayreceiver.desktop.backend.FusionPlayDiagnosticLogger
import com.airplayreceiver.desktop.nativebridge.FusionPlayNative
import com.fusionplay.android.media.FusionPlayMediaChannel
import kotlin.system.exitProcess

class FusionPlayFlutterApplication : Application() {
    lateinit var diagnosticLogger: FusionPlayDiagnosticLogger
        private set
    lateinit var runtime: FusionPlayRuntime
        private set

    private var previousCrashHandler: Thread.UncaughtExceptionHandler? = null

    override fun onCreate() {
        super.onCreate()
        AndroidPaths.initialize(this)
        diagnosticLogger = FusionPlayDiagnosticLogger()
        installCrashHandler()
        diagnosticLogger.write(
            component = "android_application",
            event = "lifecycle",
            outcome = "created",
            details = mapOf("android_sdk" to android.os.Build.VERSION.SDK_INT),
        )
        FusionPlayNative.initialize(this)
        FusionPlayMediaChannel.initialize(this)
        runtime = FusionPlayRuntime(this, diagnosticLogger)
    }

    override fun onTerminate() {
        diagnosticLogger.write(
            component = "android_application",
            event = "lifecycle",
            outcome = "terminated",
        )
        runtime.close()
        Thread.setDefaultUncaughtExceptionHandler(previousCrashHandler)
        super.onTerminate()
    }

    private fun installCrashHandler() {
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        previousCrashHandler = previous
        Thread.setDefaultUncaughtExceptionHandler { thread, error ->
            diagnosticLogger.write(
                component = "android_runtime",
                event = "uncaught_exception",
                outcome = "failure",
                details = mapOf(
                    "exception_type" to error.javaClass.name,
                    "exception_message" to error.message,
                    "stack_trace" to error.stackTraceToString(),
                    "crashing_thread" to thread.name,
                ),
            )
            if (previous != null) {
                previous.uncaughtException(thread, error)
            } else {
                android.os.Process.killProcess(android.os.Process.myPid())
                exitProcess(10)
            }
        }
    }
}
