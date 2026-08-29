# FusionPlay Android

FusionPlay 是一个面向全平台的局域网音乐投放接收器，支持 MiPlay AirPlay DLNA

FusionPlay Android 最低支持 Android 5.0（API 21） 并对 Android TV 做了遥控器适配

当前开源版本为 **1.2.3**，应用 ID 为 `com.fusionplay.android`。

源码仓库：<https://github.com/18223392722/FusionPlay>

## 功能

- AirPlay 音频接收、媒体信息与播放状态同步。
- DLNA/UPnP AVTransport 接收与本地播放。
- 进程内实现的小米妙播发现、控制、鉴权兼容和音频接收。
- `armeabi-v7a`、`arm64-v8a`、`x86_64` 单一通用 APK。
- Android 5.0（API 21）最低版本兼容。
- 遥控器、无障碍保活、自动唤起、媒体通知与日志导出。

## 目录结构

```text
flutter/
  lib/                  Flutter 界面与状态模型
  android/app/          Android 宿主、Kotlin 后端和 Rust 构建任务
  test/                 Flutter 与界面交互测试
src/
  AirPlayReceiver.Core/ AirPlay、DLNA、音频和 JNI 核心
  FusionPlay.MiPlaySdk/ 小米妙播协议与媒体接收
  FusionPlay.AndroidIfAddrs/ Android 5 网络接口适配
vendor/shairplay/       带本地修复的上游 AirPlay 实现
tools/                  图标、抓包和诊断辅助工具
```

项目只有 `flutter/android/app` 一个 Android 应用模块；Rust 动态库由该模块的 `buildRustNative` 任务生成到 Gradle 构建目录，不会写入源码树。

## 构建环境

- Windows 10/11 与 PowerShell 7。
- Flutter 3.32.8、Dart 3.8.1。
- Android SDK 36、Android NDK 28.2.13676358。
- JDK 21。
- Rust 工具链以 `src/AirPlayReceiver.Core/rust-toolchain.toml` 为准。
- `cargo-ndk` 可执行文件位于 `CARGO_HOME/bin` 或 `PATH`。

标准环境变量为 `FLUTTER_ROOT`、`ANDROID_HOME`（或 `ANDROID_SDK_ROOT`）、`JAVA_HOME` 与 `CARGO_HOME`。如使用集中工具目录，也可以设置 `FUSIONPLAY_TOOLS_ROOT`；构建脚本会将缓存放入该目录并自动处理包含非 ASCII 字符的工程路径。

## 检查与测试

```powershell
Set-Location .\flutter
flutter pub get --enforce-lockfile
flutter analyze --no-pub
flutter test --no-pub
Set-Location ..
```

Kotlin 单元测试可通过 Flutter Android 工程的 Gradle Wrapper 执行：

```powershell
.\flutter\android\gradlew.bat `
  -p .\flutter\android `
  :app:testReleaseUnitTest `
  --no-daemon
```

Rust 模块可分别运行 `cargo test` 和 `cargo clippy`。Android 原生库需要通过完整 APK 构建验证对应 NDK 目标。

## 构建通用 APK

```powershell
.\flutter\tool\build_universal.ps1
```

脚本读取根目录 `VERSION`，计算 Android `versionCode`，构建包含 32 位 ARM、64 位 ARM 与 x86_64 的单一 APK，并将 APK 与 Dart 混淆符号保存到 `artifacts/releases/`。该目录不进入版本控制。

当前 Release 配置在没有私有签名配置时使用 Android 调试签名，公开发布商店版本前应替换为自己的安全签名流程，且不得提交密钥文件。

## 协议与商标说明

本项目仅为网络互操作实现，不包含或调用小米、Apple 或其他厂商的私有运行库、账号服务或设备证书。AirPlay、Apple、小米、妙播、DLNA 等名称及商标归各自权利人所有，本项目与这些厂商没有隶属或官方认证关系。

## 许可证

FusionPlay 原创源码使用 [GNU Affero General Public License v3.0](LICENSE)，SPDX 标识为 `AGPL-3.0-only`。项目内第三方组件继续适用各自许可证，详情见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 与对应依赖源码或元数据。
