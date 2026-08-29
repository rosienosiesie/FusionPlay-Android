# MiPlay 歌词发送端探针

该调试应用只用于验证歌词是否由 HyperOS 通过 MiPlay `SetMediaInfo.mLrc` 传输到 FusionPlay 喵。

它把固定的带时间戳 LRC 文本写入 Android `MediaSession` 的 `android.media.metadata.DISPLAY_DESCRIPTION`，不读取本地歌词文件、不请求网络，也不按歌曲信息匹配歌词喵。

该模块独立于 FusionPlay 正式 Android 应用，不会进入发布 APK 喵。

运行后选择声明为电视或带屏音响诊断类型的 FusionPlay 接收端，HyperOS 才会把该字段序列化为 `mLrc` 喵。
