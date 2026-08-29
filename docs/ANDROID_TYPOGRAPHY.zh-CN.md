# FusionPlay Android 字体与字重方案喵

## 1. 文档范围喵

本文记录 FusionPlay Android 当前实际生效的字体、字号、行高和字重方案喵

本文依据当前 Flutter 主题与页面代码整理，不代表尚未实现的设计提案喵

## 2. 字体来源喵

应用统一使用 Flutter 的 `sans-serif` 系统字体族，没有内置或下载自定义字体文件喵

英文、数字与中文的最终字体由 Android 系统和设备厂商的无衬线字体回退链决定喵

因此在小米设备上通常会呈现系统提供的中文无衬线字体，而不是由 FusionPlay 单独打包字体喵

## 3. 全局 Typography 方案喵

| Material 3 层级 | 字号 | 行高 | 字重 | 主要用途 |
| --- | ---: | ---: | ---: | --- |
| `displaySmall` | 36 sp | 44 sp | SemiBold 600 | 大型展示标题喵 |
| `headlineMedium` | 28 sp | 36 sp | SemiBold 600 | 页面标题与歌曲名称喵 |
| `titleLarge` | 22 sp | 28 sp | SemiBold 600 | 大号标题与歌词喵 |
| `titleMedium` | 16 sp | 24 sp | SemiBold 600 | 设置项标题与次级标题喵 |
| `bodyLarge` | 16 sp | 24 sp | Regular 400 | 正文与版本信息喵 |
| `bodyMedium` | 14 sp | 20 sp | Regular 400 | 次级正文和辅助信息喵 |
| `labelLarge` | 14 sp | 20 sp | Medium 500 | 按钮和主要标签喵 |
| `labelSmall` | 11 sp | 16 sp | Medium 500 | 小型标签喵 |

其余 Material 3 文字层级也在 `FusionTypography` 中显式定义，避免不同 Flutter 版本产生字重差异喵

## 4. 页面特殊字重覆盖喵

### 4.1 播放页喵

| 元素 | 字重 | 说明 |
| --- | ---: | --- |
| 歌曲名称 | SemiBold 600 | 在 `headlineMedium` 基础上明确指定 SemiBold 喵 |
| 歌手等普通信息 | Regular 400 | 使用正文样式，不额外加粗喵 |
| 进度时间 | Medium 500 | 使用 12 sp 的页面专用样式喵 |

### 4.2 设置页喵

| 元素 | 字重 | 说明 |
| --- | ---: | --- |
| 页面主标题“设置” | Bold 700 | 当前页面最高强调层级喵 |
| 设置板块标题 | SemiBold 600 | 使用 `titleMedium` 并明确保持 SemiBold 喵 |
| 设置项标题 | SemiBold 600 | 与板块标题保持一致喵 |
| 按钮文字 | Medium 500 | 主要沿用 `labelLarge` 喵 |
| 说明文字与版本号 | Regular 400 | 使用正文层级喵 |

## 5. 字重层级总结喵

FusionPlay 当前使用四级字重体系喵

| 字重 | Flutter 名称 | 使用原则 |
| ---: | --- | --- |
| 400 | `FontWeight.Normal` 或未显式设置 | 正文、说明、版本号和普通辅助信息喵 |
| 500 | `FontWeight.w500` | 按钮和标签喵 |
| 600 | `FontWeight.SemiBold` | 歌曲名称、设置项标题和常规标题喵 |
| 700 | `FontWeight.Bold` | 设置页主标题和当前歌词喵 |

整体原则可概括为正文使用 400、交互标签使用 500、常规标题使用 600、最高强调内容使用 700 喵

## 6. 对应实现文件喵

全局 Typography 定义位于 `flutter/lib/theme/fusion_theme.dart` 喵

播放页的特殊覆盖位于 `flutter/lib/ui/player_view.dart` 喵

设置页的特殊覆盖位于 `flutter/lib/ui/settings_view.dart` 喵
