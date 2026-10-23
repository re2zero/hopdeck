# 项目计划：DeskGo 跨设备协同（Mousehop 三期能力）

<!-- 本文件是接力的「计划层」真相源：里程碑完成就勾选，决策与死路只追加不删改。
     全部完成或终止时：在文件顶部另起一行写入「退出标记」——HTML 注释内写 RELAY:EXIT。 -->

## 目标与范围
在 Mousehop v0.17.1（lan-mouse 系 Rust 工作区）上新增三项能力：① 一控多键鼠共享（复用现有捕获/DTLS/事件模型）；② 可选 USB/IP HID 注入后端（Linux 接收端兜底，不依赖合成器 libei 支持）；③ 可选实时画面（GPUI 控制端 + gpui_interop 零拷贝渲染，Linux+Windows 首发）。
Out of scope：per-client 后端选择（保持本机全局语义）、Windows USB/IP 客户端驱动打包（UI 标记实验性）、macOS 画面、mousehop-proto 输入协议语义改动。

## 里程碑
- [ ] M1 USB/IP 注入后端 PoC（分支 `feat/usbip-emulation`）—— 验收：QEMU 接收端注入键盘+鼠标成功；真机 Wayland（GNOME 45+/KDE 6.1+）与 X11 交叉验证；关闭 libei portal 后注入仍生效；`scripts/usbip-receiver-setup.sh`（vhci-hcd 加载 + loopback attach）交付；`cargo test -p input-emulation` 与 `cargo clippy --workspace` 通过。
- [x] M2a 连接层功能（2026-10-23 拆分；用户指示功能优先）—— 验收：ClientConfig 支持连接方式（直连 / 反向外呼 / 转发端口地址），daemon 拨号方向按配置执行；反向外呼场景两机可经单侧发起建立会话；`mousehop-cli` 可配置与验证；mousehop-proto 无变更。（增量一 e6d973b：ConnectMode 全链路 + 反向拨号/保活 + CLI；增量二 = M2a-ii：控制输入流走反向隧道，双实例 E2E 注入验证通过，见 journal/M2a-REPORT.md）
- [ ] M2b GPUI 控制端 UI（**挂起：等用户查看确认设计稿后再开始**）—— 验收：GPUI 应用（gpui-pre 0.3.9 + gpui-kit/ely 组件）完成设备添加与发现（自动扫描 / 手动 / 反向外呼三路径）+ 拖拽布局 + per-device 连接方式 + 发起一控多会话；接收端在 libei ↔ USB-IP 间切换后键鼠共享正常；GPUI 前端崩溃不影响 daemon；USB-IP 选项带「需要接收端配置」显式提示与可操作指引。开工前置：跑通 `Ely-GPUI-Components/compat/kit` 示例。
- [ ] M3a 画面链路（可选，feature `video`，ely `RenderSurface` 显示）—— 验收：采集（PipeWire portal / DXGI）→ 硬编 → 独立通道 → 硬解 → RenderSurface 全链路可用，LAN 1080p 端到端 <150 ms；画面开关关闭后键鼠共享延迟与 CPU 占用对比 M2 基线无显著退化。
- [ ] M3b 零拷贝渲染进程（可选进阶）—— 验收：独立 `mousehop-view` 进程（bite-gpui ^1.25 + gpui_interop 0.2.0）以 DMA-BUF/共享纹理导入显示，渲染路径无 CPU 像素拷贝；渲染进程崩溃不阻断键鼠与控制台。
- 每个里程碑收尾：技术报告（实际 crate 版本、已知限制、验收差距、下一步建议）+ 交接三步（PLAN 勾选 → HANDOFF → 登记）。

## 当前焦点
- 当前里程碑：**M2b 设计稿 v2**。2026-10-23 公子查看 v1 设计稿后否决（「不成熟、拿不出手」），给出全新产品形态需求（见 D14）。本轮交付 v2 设计稿（HTML 交互稿）再过目；**确认前仍严禁写 GPUI 实现代码**。M2b 实现开工前置不变：跑通 `Ely-GPUI-Components/compat/kit` 示例。**M3 严禁在 M2b 确认前开工。**
- 真机验证欠账（等用户硬件/环境）：M1 的 vhci 真机注入、M2a 的真实两机反向场景（本机为单机双实例 + Xvfb + dummy capture 模拟，方法见 journal/M2a-REPORT.md）。

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
- 2026-10-23 [D10·M2a 拆分] 调查结论：Mousehop 会话角色与拨号方向耦合（拨号=发送角色、接听=注入角色），因此「输入流走反向隧道」需要会话角色桥接（收养入站连接进出站表 + 拨号侧收包注入仿真），改动面大，拆为 M2a-ii 单独做。M2a 增量一（e6d973b）交付 ConnectMode 配置全链路 + 被控端主动拨号/保活 + CLI；双实例验证反向拨入握手全通。「端口转发」场景由 direct + 指向转发地址（UDP 需转发）覆盖，README 已写明。
- 2026-10-23 [事实·测试环境] 本机 listener 不绑 loopback（按真实网卡枚举），双实例回环测试须用真实 IP（10.8.15.60）+ 隔离 `XDG_RUNTIME_DIR`（IPC socket 路径）+ 各自 `--config/--cert-path/--port`；被控端本机 emulation 自动落到 X11 后端（无 Wayland/portal），xdotool 可验证注入。本机还跑着用户真实 mousehop（占 4242），测试端口用 43xx。
- 2026-10-23 [D11·M2a-ii 实现] 反向隧道桥接（收养式，commit 见 HANDOFF-004）：a) 被控端 `connect_to_handle` 拨号成功后若 `ConnectMode::Reverse`：装 capture 侧簿记（slot + `set_alive(true)`——本侧无 receive_loop 处理 Pong，存活由 listener 心跳 + 控制端 ping 维持）后经 `EmulationRequest::AdoptReverse` 把 conn 交给 `MousehopListener::adopt`（accept 注册路径抽出为 `register_accepted_conn` 共用），不 spawn connect 侧三任务——listener read_loop 是唯一读者；b) 控制端 `EmulationEvent::Connected` → `maybe_adopt_reverse_session`：`get_client`（IP+active）+ Reverse + **非拨号回声**（addr == 该客户端配置拨号目标 ⇒ 是自己 dial 的回声，已由对端收养）+ **无活跃传输**（收养只填空，不抢现有会话）→ `EmulationRequest::TakeConn` 从 listener 取走 conn → `MousehopConnection::adopt_inbound`（alloc 新 session 使在途拨号被现有代际守卫自动废弃 + spawn hello/ping_pong/receive_loop），capture 发送路径零改动；c) 拨号回声两跳闭环：B 拨号→收养→listener Accept→Connected→B 侧判定 dial-echo 跳过二次收养。
- 2026-10-23 [D12·bug 修复] accept 时 spawn 的 `hello_watchdog` 持 conn 引用，take_conn 收养后 12s 必杀已收养连接（T+12 掉线）——修复为关闭前校验 `accepted_connection_is_current`，已离开 listener 连接表的会话其孤儿 watchdog 退避。同时确认：僵尸 read_loop 收养后至多吃一个数据报即自退且不发布 Disconnected（现有 stale 守卫），Hello 重传预算（8×750ms）覆盖该一次性丢包。
- 2026-10-23 [D13·已知限制] 反向隧道对输入是单向的：被控端可经隧道接收注入，但其控制端反向（B 控制 A）需反向拨号的会话，M2b/M3 前不做（README Connection Modes 已注明）。forward_log：capture 侧 conn2 类冗余连接（双方互标 reverse 且双向可达时）由 ping 无应答在 ~2s 内自清理，无需专门处理。
- 2026-10-23 [事实·E2E 方法] 本机 X11 capture 是未实现 stub，`--capture-backend dummy` 是唯一 capture 路径；dummy 的 Begin 每后端实例仅一次，竞速 B 的 5s 拨号不可靠——在 `input-capture/src/dummy.rs` 加 `MOUSEHOP_DUMMY_CAPTURE_DELAY_MS`（默认 0，行为不变）使 Begin 可在隧道建立后确定性触发。E2E = Xvfb :93(A，dummy capture) / :94(B，X11 emulation 注入)，`xdotool getmouselocation` 持续变化即注入成功。
- 2026-10-23 [D14·M2b 设计 v1 否决] 公子否决 v1 设计稿（不成熟、拿不出手），新产品形态需求：① 单一主界面 + 设置按钮（配置界面从设置进入）；② 设备列表上方搜索框可输入任意 IP，回车时目标不在列表则直接向该 IP 发起连接请求——覆盖跨网段不可发现与单向网络（reverse）场景；③ 设备列表与设备详情并排两列一行（左列表右详情）；④ 屏幕布局编辑收为下方一行、可折叠；⑤ 点击设备项展示详情与操作（修改配置、重置指纹、远程查看等）；⑥ 左下角为本机设备 + 设置按钮；⑦ 底部状态栏，开启调试日志后底部追加日志栏；⑧ 为云端远程控制预留（头脑风暴方向：云中继接入、设备码直连、远程查看/反控、LAN/云设备分组徽标）。v1 稿存档：/home/zero/.zcode/v2/visualizations/97edd8bc06635e4af0dc6710e96b262848aa423f5b02ef52c79aa28b460977e9/mousehop-console-design.html
- 2026-10-23 [D15·v2 设计迭代] 公子对 v2 反馈两条：① 认证流重做——「复制指纹」无意义（不应要求手动填指纹到其它机器），改为远程控制式认证：能连通即发认证请求，支持**对方确认**与**访问密码**两种方式，指纹仅在首次确认时核对一次、此后自动信任；「重置指纹」移入安全区并强制二次确认（防误触）。② 布局区从底部通栏移到**右栏设备详情下方**（详情内容少时填补空旷），并支持**从列表拖拽设备到布局画布**放置（取消「未布置」区）。→ v2.1 稿。实现备注：认证请求/密码对应 proto 已有能力（Hello/授权表），「访问密码」为新需求（被控端设置 + 连接握手附加认证，实现期待设计）；「撤销信任」= 移除 authorized_fingerprints 条目。
- 2026-10-23 [D16·v2.1 设计迭代] 公子认可 v2.1（「不错」）并提细节两条：① **窗口高度不随内容变化**（编辑模式切换等场景不再「秃噜」）——定高 780px，详情区内部滚动；共享选项改**卡片式布局**（剪贴板/自然滚动/灵敏度三卡，紧凑）。② 布局画布支持**「下」屏并扩展多屏**：左/右/下各 1 槽 + 上排最多 3 槽（合计 6），离线设备可预布局（半透明+离线标注），容量满后拖入槽自动消失、占用槽拒绝重复放置并 toast 提示。→ v2.2 稿（同路径覆盖）。

## 接力索引
- 会话账本：`.relay/state.json`
- 交接文档：`.relay/journal/HANDOFF-*.md`（以最新为准）
- 约定：`.relay/` 纳入版本库（团队接力资产）。
