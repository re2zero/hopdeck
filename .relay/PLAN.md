# 项目计划：DeskGo 跨设备协同（Mousehop 三期能力）

<!-- 本文件是接力的「计划层」真相源：里程碑完成就勾选，决策与死路只追加不删改。
     全部完成或终止时：在文件顶部另起一行写入「退出标记」——HTML 注释内写 RELAY:EXIT。 -->

## 目标与范围
在 Mousehop v0.17.1（lan-mouse 系 Rust 工作区）上新增三项能力：① 一控多键鼠共享（复用现有捕获/DTLS/事件模型）；② 可选 USB/IP HID 注入后端（Linux 接收端兜底，不依赖合成器 libei 支持）；③ 可选实时画面（GPUI 控制端 + gpui_interop 零拷贝渲染，Linux+Windows 首发）。
Out of scope：per-client 后端选择（保持本机全局语义）、Windows USB/IP 客户端驱动打包（UI 标记实验性）、macOS 画面、mousehop-proto 输入协议语义改动。

## 里程碑
- [ ] M1 USB/IP 注入后端 PoC（分支 `feat/usbip-emulation`）—— 验收：QEMU 接收端注入键盘+鼠标成功；真机 Wayland（GNOME 45+/KDE 6.1+）与 X11 交叉验证；关闭 libei portal 后注入仍生效；`scripts/usbip-receiver-setup.sh`（vhci-hcd 加载 + loopback attach）交付；`cargo test -p input-emulation` 与 `cargo clippy --workspace` 通过。
- [ ] M2a 连接层功能（2026-10-23 拆分；用户指示功能优先）—— 验收：ClientConfig 支持连接方式（直连 / 反向外呼 / 转发端口地址），daemon 拨号方向按配置执行；反向外呼场景两机可经单侧发起建立会话；`mousehop-cli` 可配置与验证；mousehop-proto 无变更。
- [ ] M2b GPUI 控制端 UI（**挂起：等用户查看确认设计稿后再开始**）—— 验收：GPUI 应用（gpui-pre 0.3.9 + gpui-kit/ely 组件）完成设备添加与发现（自动扫描 / 手动 / 反向外呼三路径）+ 拖拽布局 + per-device 连接方式 + 发起一控多会话；接收端在 libei ↔ USB-IP 间切换后键鼠共享正常；GPUI 前端崩溃不影响 daemon；USB-IP 选项带「需要接收端配置」显式提示与可操作指引。开工前置：跑通 `Ely-GPUI-Components/compat/kit` 示例。
- [ ] M3a 画面链路（可选，feature `video`，ely `RenderSurface` 显示）—— 验收：采集（PipeWire portal / DXGI）→ 硬编 → 独立通道 → 硬解 → RenderSurface 全链路可用，LAN 1080p 端到端 <150 ms；画面开关关闭后键鼠共享延迟与 CPU 占用对比 M2 基线无显著退化。
- [ ] M3b 零拷贝渲染进程（可选进阶）—— 验收：独立 `mousehop-view` 进程（bite-gpui ^1.25 + gpui_interop 0.2.0）以 DMA-BUF/共享纹理导入显示，渲染路径无 CPU 像素拷贝；渲染进程崩溃不阻断键鼠与控制台。
- 每个里程碑收尾：技术报告（实际 crate 版本、已知限制、验收差距、下一步建议）+ 交接三步（PLAN 勾选 → HANDOFF → 登记）。

## 当前焦点
- 当前里程碑：M1 代码完成（commit 1ff7c99，分支 feat/usbip-emulation）。23/23 测试通过；DEVLIST/IMPORT 协议级集成测试 + 真实二进制端到端冒烟通过；**真机内核级注入验证待用户硬件**（本机无 vhci_hcd/sudo，详见 journal/M1-REPORT.md）。自动接力范围 = M1 收尾 → M2a（纯功能）；**M2b（UI）与 M3 必须停在等待用户确认的状态，接力会话不得自行开工 UI**。D8 内置中继默认不做。
- 下一步精确动作（M2a 连接层功能，新会话从这里开始）：
  1. 读 `mousehop/src/connect.rs` 与 `listen.rs`，确认现状「谁拨谁」：激活客户端时由哪侧发起 DTLS、会话如何被两端共享（`ClientState.active_addr`、`ConnectionAttempt` 事件），以及反向会话是否已天然支持（对端拨入后本机能否把它当作可注入客户端）。
  2. 设计 per-device「连接方式」字段：`TomlClient`（config.rs:95-128）与 `ClientConfig`（mousehop-ipc lib.rs:345-398）新增 `connect_mode: direct|reverse`（forward 场景已被「自定义 host:port 地址」覆盖，仅补文档；若调查发现无需新字段即可反拨，则字段只服务 UI 语义与校验）。**不动 mousehop-proto**。
  3. 实现反向外呼路径：受限侧 daemon 主动拨出对端并维持会话；确保未拨入成功时 UI/日志给出可操作提示。
  4. `mousehop-cli` 增加对应设置/查询；两实例本机回环互测（两个 config、两个端口）。
  5. `RUSTUP_TOOLCHAIN=1.96.1 cargo fmt/clippy/test`（1.95.0 钉子未安装且离线，见 D9）。
  6. 完成后执行交接三步并继续排自动接力；M2a 完成后停止，等用户确认 M2b。

## 决策与死路（追加式）
- 2026-10-22 初始化计划。
- 2026-10-22 [D1] USB/IP 采用「接收端 loopback」架构：接收端 daemon 经 `usbip` crate 在 127.0.0.1:3240 导出虚拟复合 HID 设备（键盘用内置 boot-keyboard handler，鼠标自写 `UsbInterfaceHandler`），vhci-hcd attach 本机；事件路径 = 捕获 → DTLS → Emulation trait → HID 报告 → loopback USB/IP → 内核。理由：零改动发送端与协议层、完整复用现有 Emulation 插入点、保留 DTLS 加密；替代方案（发送端导出虚拟设备、绕过 Mousehop 传输）被否决——破坏加密与架构边界。
- 2026-10-22 [D2] GPUI 控制端建在 bite-gpui ^1.25（1.25.202，2026-10-22 发布，维护活跃）—— gpui_interop 0.2.0 硬依赖 bite-gp-engine ^1.25；降级路径 = zed gpui 0.2.2 + CPU 拷贝 Image 渲染。注：原方案所引「guise VideoView」不存在（guise 仅为 GPUI 组件库），降级路径据此修正。
- 2026-10-22 [D3] 后端选择保持「本机全局」语义（config.toml `emulation_backend` + notify 热重载已支持，config.rs:575-586）；per-client 后端列为后续扩展，不进本期。
- 2026-10-22 [D4] USB/IP 接收端首发仅 Linux（vhci-hcd 内树支持）；Windows 依赖 usbip-win 系第三方驱动（cezanne 上游停滞、fork 需处理签名/HVCI），UI 标记实验性、不可默认启用。
- 2026-10-22 [D5] 画面传输用独立 UDP 通道（同证书 DTLS 认证），不与输入 DTLS socket 复用，避免码流竞争输入；分帧/FEC 细节 M3 内设计。
- 2026-10-22 [D6] evdev→HID 映射表置于 `input-emulation/src/usbip/` 模块内，不动 input-event 公共 API；出现第二个使用方再上移。
- 2026-10-22 [事实] mousehop 端到端为纯 UDP+DTLS（无 TCP 连接请求阶段，connect.rs:207-250）；Hello{magic,commit,capabilities} 握手（proto lib.rs:52-76），画面等新能力应走 capabilities 位 + 追加 tag（forward-compat 已内建）。
- 2026-10-23 [D7 修订 D2] GPUI 主线改选 **gpui-pre 0.3.9**：证据——本地 Ely-GPUI-Components 0.1.1（活跃开发，lib.rs 实装 44 模块：forms 19k 行、shell 5.3k、navigation 4.9k、settings 4.7k、media 7.7k、onboarding 2.2k）绑定 `gpui-pre =0.3.9`；仓库内 `compat/kit`（ely-kit-compat，"Ely and the newest GPUI Kit in one app and one window"）证明 ely 与 gpui-kit ≥0.8 同窗共存；用户另一项目 herdr-gpui 亦用 gpui-pre。控制端 UI = gpui-kit 基础组件 + ely 组件；**拖拽原语 ely 未实装**（src/interaction 仅 resizable/rotatable/roving/selection），布局拖拽需自研轻量实现。ely `RenderSurface`（src/rendering/surface.rs）为 CPU 帧回调→RenderImage→atlas 路径，非零拷贝——故 M3 拆为 M3a（RenderSurface 先行）/ M3b（bite-gpui+gpui_interop 独立渲染进程，兑现零拷贝）；原「控制端建在 bite-gpui」方案作废。
- 2026-10-23 [D8] 连接层新增 per-device「连接方式」：① 直连（现状，双向可拨）；② 反向外呼/单向隧道（网络受限侧主动外呼建立 DTLS 会话，拨号方向与 KVM 控制方向解耦，会话建立后双向可用）；③ 经转发端口（自定义 host:port，覆盖路由 DNAT 与 ssh -L/socat 场景；注意 ssh -L 为 TCP，UDP 需 UDP 隧道，文档给指引）。涉及 connect.rs 拨号方向选择与 ClientConfig 新字段，mousehop-proto 无需变更。**待确认**：②是否需要内置中继/TCP 隧道，还是仅 UI+拨号方向支持即可。
- 2026-10-23 [D9] 工具链偏离：仓库钉 1.95.0 但未安装且 static.rust-lang.org 下载超时，本机一律以 `RUSTUP_TOOLCHAIN=1.96.1` 执行 fmt/clippy/build/test；CI（有网）仍用 1.95.0，网络恢复后应本地 `rustup toolchain install 1.95.0` 回归钉子。
- 2026-10-23 [事实·M1] usbip crate 0.9 硬依赖 rusb+nusb（无 feature 可关）；ep0 控制传输按 `wIndex` 路由到目标接口 handler、中断传输按 endpoint 归属接口——复合设备（一设备双接口、一次 attach）成立；OP 码为 crate 自定义帧（REQ_DEVLIST 0x8005 / REP 0x0005 / REQ_IMPORT 0x8003 / REP 0x0003，版本 0x0111），集成测试据此手写帧。

## 接力索引
- 会话账本：`.relay/state.json`
- 交接文档：`.relay/journal/HANDOFF-*.md`（以最新为准）
- 约定：`.relay/` 纳入版本库（团队接力资产）。
