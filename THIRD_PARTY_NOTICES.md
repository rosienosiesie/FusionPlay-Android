# 第三方组件与许可证说明

FusionPlay 原创源码以 GNU Affero General Public License v3.0（`AGPL-3.0-only`）发布。第三方组件不因被本项目引用或打包而改变其原有许可证。

依赖的精确版本以以下锁定文件和构建文件为准：

- `flutter/pubspec.lock`
- `src/AirPlayReceiver.Core/Cargo.lock`
- `src/FusionPlay.MiPlaySdk/Cargo.lock`
- `flutter/android/app/build.gradle.kts`

## Flutter 与 Dart

Flutter SDK 及其框架组件主要采用 BSD 3-Clause License。`flutter_svg` 及其传递依赖分别采用其包内声明的 MIT、BSD 或其他兼容开源许可证。完整许可证文本可在 Flutter SDK和 Dart/Flutter 包缓存中的对应包目录查看。

## Android 与 Kotlin

AndroidX Core、Media、Media3、Kotlin Coroutines、Kotlin Serialization 和 Android desugar JDK libraries 主要采用 Apache License 2.0。JUnit 4 测试依赖采用 Eclipse Public License 1.0。

## Rust 依赖

Rust 依赖主要采用 MIT、Apache-2.0、BSD、MPL-2.0、Zlib 或这些许可证的双重/多重许可组合。Cargo 元数据中没有发现许可证字段为空的依赖。完整清单以 Cargo 锁定文件及各 crate 的 `license` 元数据和许可证文件为准。

## shairplay 0.7.0

`vendor/shairplay` 是项目内维护的上游 `shairplay` 0.7.0 源码副本，并包含 FusionPlay 所需的本地修复。crate 元数据声明 `LGPL-3.0-or-later`，个别源码文件另带 `GPL-3.0-only` 标识；这些文件继续受各自声明约束。本地修改见 `vendor/shairplay/LOCAL_PATCHES.md`，上游许可证全文见 `vendor/shairplay/LICENSE`。

`vendor/shairplay/airport.key` 来自该上游 AirPlay 兼容实现并随其源码公开分发，用于协议互操作；它不是 FusionPlay 用户、开发者或设备的登录凭据。

上游项目：<https://github.com/fabianlindfors/shairplay>

## 协议名称

小米妙播接收逻辑由 `src/FusionPlay.MiPlaySdk` 中的 Rust 源码实现，不打包或启动 MiPCAudio、MAFSvr、MiConnect、MiCont、小米电脑管家或厂商账号服务。源码中的协议名称、服务标识和兼容性测试向量仅用于网络互操作。

本文件用于记录工程分发边界，不构成法律意见。
