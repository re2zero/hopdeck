# HANDOFF-021（M3a 增量② 全收口 → 增量③ portal 采集）
会话：#14（session_27e048faec3abf9b1a8c518289cdc8dd）  日期：2026-10-25  基线：feat/usbip-emulation@cf9069f

## 一句话状态
M3a 增量② **全部收口**：②a proto（18d46b3）+ **②b daemon video UDP+DTLS 通道接线**（cf9069f，T2 done）；双实例 E2E 冒烟五项全过（780 帧 counter=779 lost=0 零错序零丢失 + 拆除路径）；**下一步 = T3 PipeWire portal 采集后端**（换掉 B 侧 DummyVideoSource）。

## 已完成（含证据）
- **②b 接线**（cf9069f，9 文件 +915 行）：
  - `mousehop/Cargo.toml`：`mousehop-video` 可选依赖 + `video` feature（不进 default）。
  - `config.rs`：`[video]` 段（enabled/port/width/height/max_fps，默认 disabled + 0 端口 + 1280x720@30）+ `ConfigToml::video()` 解析 + `Config::video()` 委托 + 3 个往返测试。
  - `video.rs`（新，~660 行）：**B 侧 VideoHost**（双栈 bind `[::]:port` v4 回退 / DTLS 服务端 / 严格指纹 verify / accept loop / send_loop = DummyVideoSource→OpenH264Encoder→Packetizer→DTLSConn / read_loop 观察对端关闭）；**A 侧 VideoPeer**（VideoOffer 拨号+接收解码+日志计数 / VideoStop+peer_gone 拆除 / 最新 offer 胜单槽）。恒编译 + `mod imp` 按 feature 切换同签名 no-op stub，集成点零 cfg。
  - `emulation.rs`：`EmulationRequest::AnnounceVideo` 变体 + run loop 臂（session_is_current 校验后 listener.reply）+ `Emulation::announce_video`。
  - `service.rs`：VideoHost 构造（bind 失败仅告警不 fatal）；Connected 臂 announce（connect_mode==Reverse 跳过，D34）；Disconnected 臂 `video.halt()`；Capture::new 传 VideoPeer。
  - `capture.rs`：主循环处理 VideoOffer/VideoStop（加入 active_client 过滤白名单——offer 早于激活）+ Disconnected 分支 `peer_gone`。
  - `README.md`：Screen Sharing (experimental) 段。
- **测试**：mousehop 113 绿（含 3 个新 config 测试）× 两 feature 组合；mousehop-proto 22 绿；mousehop-video 20 绿；clippy 两组合净（仅预存 `start_service` dead_code）。
- **E2E 冒烟**（/tmp/mh-video1，配方 = findings #14 末条）：announce→dial→DTLS→帧流 780 帧 counter=779 lost=0；kill A → 输入会话 idle 超时 ~15s → `sender stopped after 1243 frames`（拆除链验证）。B listener `[::]:57787` 双栈临时端口实证。

## 关键决策与事实（D37 全文在 PLAN.md，防重蹈）
1. **鉴权收紧**：video 通道只认 authorized_fingerprints，access-password 旁路**不适用**（旁路对端在 video 上无协议挑战可守门=裸流泄漏）；密码模式对端无画面 = v1 已知限制（README 已注明）。
2. **VideoOffer 路径**：listener 在 ListenTask 手里，service 必须经 `EmulationRequest::AnnounceVideo` 走 emulation 侧 reply（HANDOFF-020 风险项命中）。
3. **A 侧挂点**在 daemon capture.rs 主循环（HANDOFF-020 写的 service.rs 是笔误）；VideoOffer/VideoStop 须豁免 active_client 过滤。
4. **单通配 bind**（非 per-IP）：per-IP 临时端口各不相同而 VideoOffer 只有一个 port；多宿主源地址对称性为已接受 v1 限制。
5. reverse 会话双端都不 announce（connect_mode==Reverse 查 client_manager）。
6. kill 无 close_notify → B 侧靠输入会话 idle 超时（~15s）兜底停发；优雅路径走 VideoStop/Disconnected。
7. IDR 间隔 = 2×max_fps；`MOUSEHOP_DUMMY_CAPTURE_DELAY_MS` 本冒烟未需（无输入流需求），`--capture-backend dummy` 双侧必带（D28b）。

## 下一步精确动作（T3：PipeWire portal 采集后端，task claim T3 开始）
1. **调研先行**（半增量纪律）：ashpd 0.13.9 已在依赖树（wl-clipboard 拉入，见冒烟构建日志）；查其 ScreenCast/RestoreToken API 形态 + pipewire-rs 版本与 BGRx shm 帧拉取方式；**注意 D18 ashpd async-io/tokio 特性互斥**——daemon 构建组合（--no-default-features + x11_* + video）必须单独验证，mousehop-video 需要自己的 feature 控制 ashpd 后端（照 `capture` 模块门控惯例）。
2. **source 抽象接线**：`[video]` 加 `source = "dummy" | "portal"`（默认 dummy，portal 仅 Linux cfg）；`mousehop-video/src/capture/` 新增 portal 模块实现 `VideoSource` trait（BGRx→BGRA，帧几何以 portal 实际协商为准——**届时 VideoOffer 的 width/height 应改为监听实际帧尺寸**，现 config 驱动只是 dummy 时代的临时语义）。
3. **daemon 接线**：`video.rs` send_loop 的 `DummyVideoSource::new(...)` 换成 source 工厂（cfg 门控）；portal 授权弹框/RestoreToken 持久化日志声明（AGENTS 惯例：log backend selection）。
4. **验收**：cargo test -p mousehop-video（portal 单测用 mock/spy source）；真机 portal 弹框+RestoreToken 复用留手动清单（本机 X11 capture 是 stub、Xvfb 无 portal——D·事实）；daemon `--no-default-features` + video + portal 特性组合 clippy。
5. 完成即 `task done T3` + findings + 刷 PLAN；随后 T4 console RenderSurface（HANDOFF-020 锚点 + D7/D17 组件结论仍有效）。

## 风险与阻塞
- portal 采集无头环境不可自动化——本机只能验证编译+单测，真机验证排进「真机 Wayland 批次」（与 M2c 复验同批，等公子）。
- ashpd 特性互斥是历史已知雷（D18/D24）：mousehop-video 引入 ashpd 后，daemon 与 console 同 cargo 调用构建的禁令继续有效。
- VideoOffer 几何语义切换（config 驱动 → 实际帧尺寸）跨 proto 兼容面小（offer 只是宣告），但 T3 内要做掉，别拖到 T4。
- 勿做：gpui-kit 上报（D29）；真机 Wayland / M2c 真机复验（等公子批次）。

## 上下文指针
- 设计：`.relay/DESIGN-m3a-video.md`（§4 采集/§9 落点）；本次实现：`mousehop/src/video.rs`（cf9069f）、`mousehop/src/config.rs`（grep `TomlVideo`）、`emulation.rs`（grep `AnnounceVideo`）
- 会话锚点：video.rs send_loop（source 工厂落点）、capture.rs 主循环 VideoOffer 臂、PLAN.md D37
- 计划：`.relay/PLAN.md`（焦点已刷新）；上篇：HANDOFF-020；冒烟配方：findings「M3a②b E2E 冒烟全过」条目
