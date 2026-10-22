# HANDOFF-001（会话 #1 → 会话 #N+1）
会话：session_749de8ba88987545b88bb35b0b825e8e  日期：2026-10-22  基线：main@6b9a273（工作区干净，仅新增未跟踪 .relay/）

## 一句话状态
调查与设计阶段完成：产品交互设计稿（HTML）、三阶段实施计划（.relay/PLAN.md）已产出，**等待用户确认后才开工 M1**（用户明确要求「确认后再开始实施」）。

## 已完成（含证据）
- 代码库探索（file:line 均经子代理逐一核实）：
  - `Emulation` trait：input-emulation/src/lib.rs:431-495（私有 trait，`consume/create/destroy/terminate/display_bounds/display_layout/supports_edge_warp/warp_cursor*`）。
  - 新后端插入点五件套：`Backend` 枚举 lib.rs:69-84；`with_backend` match :146-161；自动 try-order :187-201（Wlroots→Libei→Xdp→X11→Windows→MacOs→Dummy）；`Display` :86-104；feature 于 input-emulation/Cargo.toml:74-86；另有 mousehop/src/config.rs:257-298（`EmulationBackend` serde 枚举 + `From`）与 mousehop/Cargo.toml:57-75（feature 映射）。
  - 事件模型 input-event/src/lib.rs:38-88：`PointerEvent::{Motion{dx,dy}, Button, Axis, AxisDiscrete120}`、`KeyboardEvent::{Key{key:u32}, Modifiers}`、`Event::Clipboard`；键值为 evdev 码（scancode.rs `Linux` 枚举）；按钮码 BTN_LEFT=0x110 等。
  - 传输：**纯 UDP + DTLS**（mousehop/src/connect.rs:207-250，webrtc-dtls 0.12.0；listen.rs 24-26）——AGENTS.md 所述「TCP 连接请求」与代码不符，以代码为准。握手 `Hello{magic=b"MOUSEHOP", commit, capabilities}`（proto lib.rs:52-76），新能力应加 capabilities 位 + 追加 tag（追加式 forward-compat，proto lib.rs:444-464）。
  - 前端 IPC：mousehop-ipc，UDS（Unix）/127.0.0.1:5252（Windows），JSON-lines（connect_async.rs:47-53）；`FrontendEvent/FrontendRequest` 已覆盖枚举/激活/建删/mDNS 发现/延迟（lib.rs:594-780）。
  - 配置：TOML + notify 热重载（config.rs:462-549）；backend 为**本机全局**选择（:575-586），无 per-client 字段。
  - 依赖：tokio 1.51.1、gtk4 0.9.7、reis 0.7.1、ashpd 0.13.9、arboard 3.6.1；**无 GPUI/wgpu/rdev**。
  - 单二进制双进程：GUI+daemon（main.rs:170-313，daemon 崩溃有 supervisor 重启）。
- 外部栈核查（docs.rs / crates.io / GitHub，2026-10-22）：
  - `usbip` 0.9.0（2026-10-01，MIT，tokio ^1.22）：`UsbIpServer::new_simulated(vec![UsbDevice...])` + `usbip::server(addr, server)`（默认 3240）；内置仅 `UsbHidKeyboardHandler`（boot 6KRO）；**鼠标 handler 需自写** `UsbInterfaceHandler`（GET_DESCRIPTOR/interrupt-IN）；一个 server 可挂多设备（demo 示例）。
  - `gpui_interop` 0.2.0（2026-10-22，自述 provisional）：必需 `bite-gp-engine ^1.25`，可选 `bite-gpui ^1.25`、`wgpu ^30`；Linux=SharedBuffer+Fence(DMA-BUF)；Windows=DX12 共享纹理+fence、DX11 导入；**macOS 未实现**。
  - `bite-gpui` 1.25.202（2026-10-22 发布，维护活跃）；zed `gpui` 0.2.2（2025-10-22，上游放缓）。
  - Windows USB/IP 客户端：cezanne/usbip-win 上游停滞；社区 fork（usbip-win2 类）需自处驱动签名/HVCI。

## 关键决策与死路
- D1–D6 全文见 `.relay/PLAN.md`「决策与死路」。核心：D1 USB/IP 采用「接收端 loopback」架构（零改动发送端、保留 DTLS 加密与 Emulation 插入点）；D2 GPUI 线选 bite-gpui ^1.25（降级 = zed gpui + CPU 拷贝渲染）；D2-注 原方案所引「guise VideoView」不存在（guise 仅是组件库），降级路径据此修正。

## 下一步精确动作
待用户确认方案后执行 PLAN.md「当前焦点」1–4：建 `feat/usbip-emulation` 分支 → input-emulation 加 `usbip` feature 与依赖 → `input-emulation/src/usbip.rs` 骨架 + 四处插入点 → 鼠标 HID 报告描述符与 evdev→HID 映射的单元测试先行。

## 风险与阻塞
- usbip crate 文档率 40.95%，URB/描述符细节需读源码；SET_PROTOCOL 仅返回空响应（boot protocol 足够 PoC）。
- 位移语义：proto 有 `MotionAbsolute`，HID 相对鼠标会漂移；PoC 先相对位移，必要时加绝对指针（digitizer 型描述符）接口。
- QEMU 验证需含 usbhid 的 guest 内核（jiegec/usbip CI 教训：Azure 内核缺 CONFIG_USB_HID）。
- bite-gpui 为 fork，有 API 漂移风险；gpui_interop 自述 API 不稳定 → 阶段三以 feature flag 隔离。

## 上下文指针
- 设计稿：`/home/zero/.zcode/v2/visualizations/97edd8bc06635e4af0dc6710e96b262848aa423f5b02ef52c79aa28b460977e9/mousehop-console-design.html`
- 计划真相源：`.relay/PLAN.md`
- 上游 crate：github.com/jiegec/usbip · github.com/bite-gpui/gpui_interop · bite-gpui.github.io
