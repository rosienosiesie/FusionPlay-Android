# 本地 shairplay 0.7.0 修改说明

本目录来自 `shairplay` 0.7.0 的 crates.io 发布包，并作为源码依赖参与构建。
项目没有把它伪装成未经修改的上游版本。

## 修改内容

- 为 `RemoteControl` 增加传输类型标识。
- 复用 AirPlay 2 加密 event channel，发送实验性的 MediaRemote 命令。
- 对 event channel 的入站明文进行 RTSP 分帧，支持 TCP/加密块分片与粘包；解析
  `updateMRSupportedCommands` 并使用相同加密通道回复带原始 `CSeq` 的 RTSP 200。
- 捕获 AirPlay 1/2 请求中的 `DACP-ID` 与 `Active-Remote`，优先通过
  DACP 发送媒体命令，并校验 HTTP 2xx 响应；DACP 失败后可回退到 MediaRemote。
- 解析 MediaRemote 的播放 `0`、暂停 `1` 与切换 `2`；发送端只公布
  Play/Pause 时，会结合播放速率选择正确命令。
- 连续播放/暂停时，以接收端最新控制意图对齐 MediaRemote 与 RTSP 两条独立
  通道；短时忽略乱序到达的旧速率回报，避免旧暂停覆盖新播放。
- 支持播放、暂停、播放/暂停切换、上一曲、下一曲，以及按绝对位置跳转；
  跳转通过 DACP `dacp.playingtime` 或 MediaRemote 命令 `24` 发送。
- 正确处理发送端回发的 RTSP `PAUSE`：返回 200、暂停 AirPlay 2
  buffered playout 并保留 RTP/RTSP 会话；后续 `RECORD` 恢复播放状态。
- event channel 断开后立即撤销该会话的控制能力。
- 区分 AirPlay 2 流级与连接级 TEARDOWN：停止单条音频流时保留主 RTSP
  连接和事件通道，只有连接级 TEARDOWN 才断开。
- 未识别的 buffered RTP SSRC 不再伪报为 AAC、48 kHz 或双声道。
- 增加 MediaRemote plist、RTSP 封装、命令能力解析和去重测试。
- 删除发布包中仅供示例使用的 dev-dependencies，避免本地库测试解析无关 GUI 依赖。

## 逆向依据与限制

MediaRemote 不是公开的 AirPlay 2 API。本实现参考了 2026 年 2 月公开的协议抓包：

<https://gist.github.com/MinshuG/20225d1923999c980d8545f7ac46fe6f>

抓包直接确认了：

- event channel 中的 `POST /command RTSP/1.0` 二进制 plist 形状；
- `sendMediaRemoteCommand`；
- 播放 `0`、暂停 `1`、播放/暂停切换 `2`；
- 下一曲 `4`、上一曲 `5`、推进循环模式 `7`。
- 绝对位置跳转 `24`，位置放在
  `kMRMediaRemoteOptionPlaybackPosition`（秒）。

以下仍是实验性推断，必须针对目标 iOS/iPadOS/macOS 版本做真机验证：

- `DestinationDeviceUIDs` 当前使用接收器的 AirPlay pairing ID（`pi`）；
- 下一曲、上一曲、循环命令的兼容 `value` 字符串；
- 自定义 `SenderID` 的接受范围；
- 分组/多房间路由中的目标 UUID 语义。

命令成功结果只表示消息已经写入加密 event channel，不表示 Apple 发送端已经执行。

## 许可证

上游 Cargo 元数据声明 `LGPL-3.0-or-later`，但
`src/raop/config.rs` 标注 `GPL-3.0-only`。请同时阅读本目录的 `LICENSE`
和项目根目录的 `THIRD_PARTY_NOTICES.md`。在上游澄清前，本项目只按内部研究原型交付。
