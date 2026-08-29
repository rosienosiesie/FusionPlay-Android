package com.fusionplay.android

import android.accessibilityservice.AccessibilityServiceInfo
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.SystemClock
import android.provider.Settings
import android.view.accessibility.AccessibilityManager
import android.widget.Toast
import java.io.File

internal object AccessibilityKeepAliveController {
    fun isServiceEnabled(context: Context): Boolean {
        val manager =
            context.getSystemService(Context.ACCESSIBILITY_SERVICE) as AccessibilityManager
        val expected = ComponentName(
            context,
            FusionPlayKeepAliveAccessibilityService::class.java,
        )
        return manager
            .getEnabledAccessibilityServiceList(AccessibilityServiceInfo.FEEDBACK_ALL_MASK)
            .any { enabledService ->
                val serviceInfo = enabledService.resolveInfo.serviceInfo
                ComponentName(serviceInfo.packageName, serviceInfo.name) == expected
            }
    }

    fun requestAuthorization(context: Context) {
        if (isServiceEnabled(context)) return
        Toast.makeText(
            context.applicationContext,
            R.string.accessibility_permission_prompt,
            Toast.LENGTH_LONG,
        ).show()
        val flags = Intent.FLAG_ACTIVITY_NEW_TASK
        runCatching {
            context.startActivity(
                Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS).addFlags(flags),
            )
        }.recoverCatching {
            context.startActivity(Intent(Settings.ACTION_SETTINGS).addFlags(flags))
        }
    }

    fun markAppLaunchedThisBoot(context: Context) {
        preferences(context)
            .edit()
            .putString(LAUNCHED_BOOT_ID_KEY, currentBootId(context))
            .apply()
    }

    fun wasAppLaunchedThisBoot(context: Context): Boolean =
        preferences(context).getString(LAUNCHED_BOOT_ID_KEY, null) == currentBootId(context)

    private fun currentBootId(context: Context): String {
        val bootCount = runCatching {
            Settings.Global.getInt(
                context.contentResolver,
                BOOT_COUNT_SETTING,
                -1,
            )
        }.getOrDefault(-1)
        if (bootCount >= 0) return "count:$bootCount"

        val kernelBootId = runCatching {
            File(KERNEL_BOOT_ID_PATH).readText().trim()
        }.getOrNull()
        if (!kernelBootId.isNullOrEmpty()) return "kernel:$kernelBootId"

        val bootEpochMillis = System.currentTimeMillis() - SystemClock.elapsedRealtime()
        return "epoch:${bootEpochMillis / FALLBACK_BOOT_EPOCH_BUCKET_MS}"
    }

    private fun preferences(context: Context) = context.getSharedPreferences(
        STARTUP_PREFERENCES,
        Context.MODE_PRIVATE,
    )

    private const val STARTUP_PREFERENCES = "accessibility_startup_state"
    private const val LAUNCHED_BOOT_ID_KEY = "launched_boot_id"
    private const val BOOT_COUNT_SETTING = "boot_count"
    private const val KERNEL_BOOT_ID_PATH = "/proc/sys/kernel/random/boot_id"
    private const val FALLBACK_BOOT_EPOCH_BUCKET_MS = 60_000L
}
