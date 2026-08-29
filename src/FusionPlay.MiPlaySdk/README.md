# FusionPlay MiPlay SDK

独立的纯 Rust 小米妙播音频兼容接收 SDK。运行时不启动 MiPCAudio、MAFSvr、
小米电脑管家或小米互联服务；发现、控制、媒体接收、解密、AAC 解码和音频
输出均在当前进程内完成。

## 官方能力边界

小米公开的 HyperOS 架构把互联流程分为跨设备总线和分布式安全两层。前者负责
广播、发现、连接和传输，后者还包含服务权限与设备可信验证。公开的妙播 SDK 是
手机应用侧入口，并没有公开通用接收端的设备证书、硬件信任根或准入协议。

因此，本 SDK 会明确区分以下阶段：

1. `transport_connected`：发现探测建立 TCP 连接；
2. `secure_channel_established`：兼容 SafetyAuth 会话通过；
3. `identity_exchanged`：本机可证明的设备身份已经返回；
4. `capabilities_exchanged`：双方能力信息已经交换；
5. `media_session_established`：来源端发送 OPEN，媒体路由正式成立。

前四个阶段都不会被当成正在播放的活动音源，也不会启用播放控制。SDK 不再通过
哈希伪造小米账号 ID 或厂商序列号；日志中的 `vendor_attestation_verified=false`
表示当前仅完成兼容会话鉴权，不能据此声称通过了小米官方设备认证。普通电脑是否
出现在系统妙播选择器中仍受手机系统版本、设备准入策略和官方支持范围影响。

支持目标：

- Windows：`x86_64-pc-windows-msvc`
- macOS：`aarch64-apple-darwin`、`x86_64-apple-darwin`
- Android：`armv7-linux-androideabi`、`aarch64-linux-android`、`x86_64-linux-android`

宿主应用必须允许局域网访问，并允许 UDP 5353、TCP 8899 与动态 RTSP/RTP
端口。Android 宿主还需要声明网络权限并在接收期间持有 Wi-Fi MulticastLock；
这属于系统网络权限，不是外部服务依赖。

小米公开的电脑端使用条件写的是同一 Wi-Fi 和受支持的小米笔记本。当前实现会绑定
用户指定的局域网接口，因此同一子网内的有线网络在传输层可以工作，但这不代表小米
官方对任意有线 Windows 设备提供选择器准入保证。

Windows 使用系统 DNS-SD API 与其他 mDNS 应用共享 UDP 5353。服务实例名仍
采用小米 Lyra/Mi Connect 格式，但 SRV 目标必须使用 `GetComputerNameW`
取得的 Windows 系统主机名；Windows 不会为自定义 SRV 主机名自动发布可解析
的 A/AAAA 记录。启动日志中的 `address_resolution_host`、
`address_resolution_strategy` 和 `interface_index` 可用于确认实际广播的主机名
与有线网卡。

## 身份与诊断

接收器把 IDM 实例 UUID、Lyra 的 8 位服务实例 ID 和媒体层的 8 位设备 ID 分开
生成并持久化。Windows 默认文件为
`%LOCALAPPDATA%\FusionPlay\Identity\miplay-identity-v1.json`。网卡 MAC 只在
无法写入身份目录时作为域分离哈希的回退种子，不会原样写入磁盘，也不会让三个协议
身份复用同一个值。旧版 `miplay-idm-instance-id.txt` 会一次性迁移到新格式。

SDK 在解析前记录收到的 UDP 数据包来源、长度和受限长度的十六进制内容，并分别
记录查询匹配、响应目标、控制端口连接、鉴权阶段及媒体会话阶段。Windows 安装器会
为侧车可执行文件创建仅限本地子网的 UDP 5353 和 TCP 8899 入站规则，覆盖域、
专用和公用网络配置文件。

仓库中的 `tools/probe_miplay_mdns.py` 与 `tools/analyze_miplay_mdns.py` 可用于采集和
分析经过脱敏的 mDNS 广播，判断数据包究竟停在广播、手机查询、SDK 回复、
TCP 8899 连接还是媒体建立阶段。真实设备抓包在公开前必须删除身份和网络信息。

Rust 宿主通过 `ReceiverConfig` 和 `MiPlayReceiver::start` 启动，通过
`ReceiverController` 执行播放、暂停、上一曲、下一曲和进度跳转。
