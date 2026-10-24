# HANDOFF-020（M3a 增量① 收口 + 增量② proto 半落地 → ②b daemon 通道接线）
会话：#13（session_3b48e11ec10f1af0e2300b3080996824）  日期：2026-10-25  基线：feat/usbip-emulation@18d46b3

## 一句话状态
M3a 按拍板设计增量推进：**增量① mousehop-video crate 全量落地**（2ecbf6d，20 测试绿）+ **增量② proto 半落地**（18d46b3，CAP_VIDEO/VideoOffer/VideoStop，22 测试绿）；**下一步 = ②b daemon video UDP+DTLS 通道接线**（调研已做完，锚点齐全，直接写代码）。

## 已完成（含证据）
- **M3a① mousehop-video crate**（2ecbf6d，T1 `cargo test -p mousehop-video` 验收过）：capture（`VideoSource` 流 trait + `DummyVideoSource` 彩条源内嵌 16 位帧计数，`read_pattern_index` 从解码像素读回）+ yuv（BT.601 BGRA↔I420，奇数尺寸安全 stride 感知）+ codec（`VideoEncoder`/`VideoDecoder` trait + OpenH264 软编软解，解码器「毒化自愈」=任一失败跳到下一 IDR）+ transport（`[seq u16][flags u8][ts u32][len u16][payload≤1200B]` 大端分帧；`Reassembler` 顺序严格、缺口=丢帧+FIRST 重同步、`in_frame` 防迟到 LAST 拼垃圾帧）。E2E 三项：无损环回像素精确、彩条 BT.601 往返、**丢关键帧包后下一 IDR 像素精确恢复**。
- **M3a②a proto**（18d46b3）：`CAP_VIDEO=1<<3` 进 `PROTOCOL_CAPABILITIES`（追加式，旧对端把新 tag 当未知数据报跳过）+ `VIDEO_CODEC_OPENH264=0` + `ProtoEvent::VideoOffer{port,codec,max_fps,width,height}`（定长 9B）/ `VideoStop`（1B）编解码 + Display/event_type 臂 + 往返测试；`encode_impl!(u16)`/`decode_impl!(u16)` 已实例化。
- 任务账本 T1–T5 建立（T1 done，T2=②b 通道接线待做）；PLAN 焦点刷新；D36 落盘；findings ×3。
- 提交链：2ecbf6d（crate）→ e721457（relay 记账）→ 18d46b3（proto）。

## 关键决策与事实（D36 + 本次新知，防重蹈）
1. **测试策略**：dummy 彩条内嵌帧计数，解码像素读回断言端到端零错序/零丢失（不信任协议元数据）；丢包恢复测试用 **720p**（320x180 关键帧 <1200B 单包，注入丢包撕裂不了帧——禁止按编码字节尺寸写断言）；彩条断言容差 ±2（编解码器舍入，黑条 G=1 正常）。
2. **openh264 0.9.9**：sys2 把 C 源码 vendored 进 crate（默认 source feature，cc 本地编译零网络）；Encoder 尺寸取自 `YUVSource` 自动 reinit；`Error::native_code()`=DECODING_STATE 位集（dsRefLost=2 等），可恢复态已映射为 `Ok(None)`。
3. **D9 已失效**：1.95.0 已在本机就位（rustup channel 校验超时≠缺失，等一会儿就过）；无需 `RUSTUP_TOOLCHAIN=1.96.1`。
4. 环境惯例重确认（D24⑤）：本机 GTK4 dev 缺失 → workspace 全量 clippy 跑不了；daemon 用 `--no-default-features` 检查（仅预存 `start_service` dead_code）。
5. proto 兼容面：daemon 测试无一断言精确 capabilities 位图（全按位与检查），加位无回归；daemon `--no-default-features` clippy 已复核。

## 下一步精确动作（②b daemon 通道接线——调研已毕，按此直接写）
1. **feature 接线**：`mousehop/Cargo.toml` 加 `mousehop-video = { workspace = true, optional = true }` + `[features] video = ["dep:mousehop-video"]`（**不进 default**；gtk feature 先例在 101 行）。config.rs 加顶层 `[video]` 段：`enabled=false`、`port=0`（0=临时端口；tunnel 段是顶层新段先例，grep `tunnel` 于 config.rs 照抄结构 + TOML 往返测试）。
2. **B 侧（被控端）**：新建 `mousehop/src/video.rs`——video listener：`UdpSocket::bind(config port)` + webrtc-dtls 服务端 Config（**照抄 listen.rs:248-291**：同 cert + `verify_peer_certificate` 查 `authorized_keys` 指纹表 + access-password 旁路语义）；会话建立即 announce：`EmulationEvent::Connected`（service.rs:963，有 addr/session/fingerprint）时若 video enabled → 经 emulation 侧 reply 路径发 `ProtoEvent::VideoOffer{port,codec:VIDEO_CODEC_OPENH264,max_fps,width,height}`（emulation.rs:517 有 `.reply(addr, session, event)` 先例）。发送任务 = `VideoSource` 流 → `OpenH264Encoder::encode` → `Packetizer::packetize` → DTLSConn send。画面未开时只 announce 不发帧（零 CPU）。
3. **A 侧（控制端）**：service.rs 主循环处理 `MousehopConnectionEvent::Received`（枚举定义 connect.rs:102，发送点 connect.rs:1322；主循环 match 在 service.rs run loop，grep `MousehopConnectionEvent::Received`）→ `VideoOffer` 存最新目标（addr:port，重复 offer 以新代旧）+ 按需拨号（connect.rs:233-249 是 DTLS 客户端先例：`UdpSocket::bind` → `DTLSConn::new(conn, config, true)`）；接收任务 = `Packet::from_bytes` → `Reassembler` → `OpenH264Decoder`（毒化自愈免费）→ 帧交 consumer channel（T4 才接 console，本增量 log/计数即可）。`VideoStop`/会话断开 → 拆通道。
4. **E2E 冒烟**（双实例 loopback，注意本机网段已迁 192.168.101.16/24，D35⑥）：A/B 各自 `--config/--cert-path/--port 43xx` + 隔离 `XDG_RUNTIME_DIR`；B `[video] enabled=true`（源先用 DummyVideoSource 占位，T3 换 portal）；断言 A 日志出现连续帧计数递增（黑盒走 RUST_LOG=mousehop=debug grep video）。daemon/console 分开 cargo 调用（ashpd 互斥，D24）。
5. 完成即 `task done T2` + findings note + 刷新 PLAN。

## 风险与阻塞
- B 侧 VideoOffer 发送要经 **emulation 侧** reply 路径（入站会话在 listener 表），不是 connect 侧 `self.conn.send`——别接错侧（D35② 教训同款）。
- video listener 与主 listener 端口语义：4253 是 M2c TCP 隧道端口，video 是新 UDP 端口（临时端口时 VideoOffer 动态带 port，无需防火墙固定项；固化端口再补 firewall xml）。
- reverse 单向拓扑 v1 无画面（D34 拍板项④）：A 拨 B 的 video 端口在 reverse 场景不可达——v1 直接不 announce/不拨（gating 写清楚）。
- 勿做：gpui-kit 上报（D29）；真机 Wayland / M2c 真机复验（等公子批次）。

## 上下文指针
- 设计：`.relay/DESIGN-m3a-video.md`（§6 传输/§7 通道/§9 落点）；代码：`mousehop-video/src/`（2ecbf6d）、`mousehop-proto/src/lib.rs`（18d46b3，grep `CAP_VIDEO`/`VideoOffer`）
- 会话锚点：service.rs:963（Connected）、emulation.rs:517（reply）、listen.rs:248-291（DTLS 服务端）、connect.rs:233-249（DTLS 客户端）、connect.rs:102/1322（Received 事件）
- 计划：`.relay/PLAN.md`（D36）；上篇：HANDOFF-019（M2c 收口）；冒烟配方：D35 + HANDOFF-018/019
