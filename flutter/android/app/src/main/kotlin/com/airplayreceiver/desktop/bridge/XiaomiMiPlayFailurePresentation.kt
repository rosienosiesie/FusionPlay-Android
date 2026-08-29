package com.airplayreceiver.desktop.bridge

import java.io.IOException

internal data class XiaomiMiPlayFailurePresentation(
    val title: String,
    val reason: String,
    val recovery: String,
    val code: String?,
) {
    fun format(context: String): String = buildString {
        append(context.trim().ifEmpty { "小米妙播启动失败" })
        append("：")
        append(title)
        append("\n原因：")
        append(reason)
        append("\n处理：")
        append(recovery)
        code?.takeIf(String::isNotBlank)?.let {
            append("\n错误码：")
            append(it)
        }
    }
}

internal class XiaomiMiPlayClientException(
    val code: String,
    message: String,
) : IOException(message)

internal fun xiaomiMiPlayFailurePresentation(
    code: String?,
    detail: String?,
): XiaomiMiPlayFailurePresentation {
    val normalizedCode = code?.trim()?.lowercase()?.takeIf(String::isNotEmpty)
    return when (normalizedCode) {
        "miplay_receiver_missing" -> XiaomiMiPlayFailurePresentation(
            title = "内置接收器文件缺失",
            reason = detail ?: "安装目录中没有找到小米妙播接收器。",
            recovery = "请运行当前版本安装器并选择修复。",
            code = normalizedCode,
        )

        "xiaomi_miplay_physical_adapter_required" -> XiaomiMiPlayFailurePresentation(
            title = "小米妙播要求使用真实物理网卡",
            reason =
                "当前没有可用的物理有线或 Wi-Fi 网卡；Hyper-V、VMware、VPN " +
                    "和隧道接口不会用于小米妙播。",
            recovery = "请连接物理有线或 Wi-Fi，在设置中刷新网卡后重试。",
            code = normalizedCode,
        )

        "xiaomi_miplay_signed_wifi_required" -> XiaomiMiPlayFailurePresentation(
            title = "检测到旧版 Wi-Fi-only 小米妙播运行时",
            reason =
                "当前桥接组件仍在返回旧版仅支持物理 Wi-Fi 的限制，" +
                    "因此不能启用本版本的有线妙播路径。",
            recovery = "请运行当前版本安装器并选择“修复”，完成后刷新网卡。",
            code = normalizedCode,
        )

        "selected_adapter_unavailable",
        "network_adapter_unavailable",
        -> XiaomiMiPlayFailurePresentation(
            title = "所选小米妙播网卡当前不可用",
            reason = "保存的物理网卡已断开、未获取 IPv4 地址或不再存在。",
            recovery = "请在设置中选择已连接的物理有线或 Wi-Fi 网卡，或改用自动选择。",
            code = normalizedCode,
        )

        "xiaomi_miplay_signature_invalid" -> XiaomiMiPlayFailurePresentation(
            title = "小米妙播运行时签名校验失败",
            reason =
                "本机小米运行时文件与受支持签名不一致；FusionPlay 已停止" +
                    "小米链路，AirPlay 和 DLNA 不受影响。",
            recovery =
                "请重新运行 FusionPlay 安装器并选择“修复”，不要手动替换" +
                    "小米运行时文件。",
            code = normalizedCode,
        )

        "xiaomi_lyra_registration_rejected" -> XiaomiMiPlayFailurePresentation(
            title = "小米 Lyra 注册被拒绝",
            reason = "Lyra 服务已经响应，但没有接受本次接收器注册。",
            recovery =
                "请确认所选物理有线或 Wi-Fi 已连接并刷新网卡；若仍失败，请导出日志。",
            code = normalizedCode,
        )

        "xiaomi_lyra_advertisement_missing" -> XiaomiMiPlayFailurePresentation(
            title = "小米妙播广播没有发布",
            reason = "注册调用完成后仍未检测到 Lyra 接收器广播，手机暂时无法发现电脑。",
            recovery =
                "请等待网络稳定后重试；若仍无广播，请运行安装器修复并导出日志。",
            code = normalizedCode,
        )

        "xiaomi_miplay_rtsp_listener_missing" -> XiaomiMiPlayFailurePresentation(
            title = "小米妙播播放端口未就绪",
            reason =
                "广播流程已经开始，但内置接收器没有建立 RTSP 监听，" +
                    "因此音频不会进入 FusionPlay。",
            recovery =
                "请关闭其他小米互联组件后重试；若仍失败，请运行安装器修复并导出日志。",
            code = normalizedCode,
        )

        "miplay_receiver_start_failed",
        "miplay_receiver_ready_timeout",
        "miplay_receiver_error",
        "miplay_receiver_exited",
        -> XiaomiMiPlayFailurePresentation(
            title = "内置接收器未能就绪",
            reason = detail ?: "小米妙播接收器启动或监听失败。",
            recovery = "请确认防火墙允许 FusionPlay，并导出日志用于排查。",
            code = normalizedCode,
        )

        else -> XiaomiMiPlayFailurePresentation(
            title = "小米妙播操作未完成",
            reason = detail?.trim()?.takeIf(String::isNotEmpty)
                ?: "接收器没有返回可识别的失败原因。",
            recovery = "请刷新网卡后重试；若仍失败，请导出日志。",
            code = normalizedCode,
        )
    }
}

internal fun Throwable.toXiaomiMiPlayUserMessage(context: String): String {
    val structured = generateSequence(this as Throwable?) { it.cause }
        .mapNotNull { failure ->
            when (failure) {
                is WindowsBridgeCommandException ->
                    failure.response.error?.let { it.code to it.message }
                is XiaomiMiPlayClientException ->
                    failure.code to failure.message
                else -> null
            }
        }
        .firstOrNull()
    return xiaomiMiPlayFailurePresentation(
        code = structured?.first,
        detail = structured?.second ?: message,
    ).format(context)
}
