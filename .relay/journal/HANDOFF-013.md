# HANDOFF-013（M2b 增量九 → 会话 #N+1）
会话：session_9f5fc17b8ccedb5ae0382dbe81e4ed8a  日期：2026-10-24  基线：feat/usbip-emulation@c0ca4df + 本次提交

## 一句话状态
M2b **增量九（设置浮层「后端/USB-IP」区 + UI 集成测试三枚）完成并冒烟**；**M2b 验收 7 项盘点完毕**（6 项有实证，差「一控多双被控会话级冒烟」与真机项）；gpui-kit 上游问题 5 项成文待公子上报。下一步 = 公子拍板四件事（访问密码三问 / 移出布局语义 / 发送侧字段 / 后端 live 切换），或先补一控多冒烟 + M2b 技术报告（素材已齐）。

## 已完成（含证据）
- **M2b 验收 7 项盘点**（对照 PLAN 第 13 行验收原文）：
  | # | 验收项 | 结论 | 证据 |
  |---|---|---|---|
  | 1 | 设备添加与发现三路径（自动/手动/反向外呼） | ✅ | mDNS 发现（daemon 既有）；搜索框 IP 回车 → `ConnectTo`（mod.rs:529-567）；详情页连接方式 chips → `UpdateConnectMode`（detail.rs:444+，:594-598） |
  | 2 | 拖拽布局 | ✅ | 增量六（layout.rs 六槽+容量守卫+ghost），冒烟 s8/s9（/tmp/mh-inc7/） |
  | 3 | per-device 连接方式 | ✅ | `ConnectMode{Direct,Reverse}`（转发端口=direct+转发地址，README）；列表行反向外呼徽标 + 详情 chips |
  | 4 | 发起一控多会话 | ⚠️ 差会话级冒烟 | 机制齐：布局六槽/激活 per-side/daemon 每侧一台；输入级 1↔1 已由 M2a-ii E2E 证明（M2a-REPORT）；**缺**一台控制端+两台被控端同时在线激活的三实例冒烟 |
  | 5 | 接收端 libei↔USB-IP 切换后共享正常 | ⚠️ 挂真机欠账 | 机制 = config `emulation_backend="usbip"` + 重启（service.rs:237 仅启动构建，无热切）；USB-IP 注入验证与 M1 真机欠账同源（等 vhci 硬件） |
  | 6 | GPUI 前端崩溃不影响 daemon | ✅ 本轮实证 | kill -9 console → daemon 存活（pid 不变）、UDP 4497 保持监听（ss）、日志无异常、新 console 即连「服务已连接」（/tmp/mh-inc9/u1–u3 + b2-reconnect.png） |
  | 7 | USB-IP「需要接收端配置」显式提示与可操作指引 | ✅ 本轮补齐 | 设置浮层「后端」区（settings.rs render_backend_section/render_usbip_card）：警告 tag + 三步指引 + 两条 mono 命令；冒烟 u3-settings.png 渲染正确 |
- **增量九·设置浮层「后端」区**（settings.rs）：本机/共享选项之后插入——「输入仿真」说明行（emulation_backend 本机全局、缺省自动探测 libei→layer-shell→X11、重启生效）+「USB/IP 注入（Linux）」卡片（`需要接收端配置` warning chip + 内核级注入说明 + ①接收端 `sudo scripts/usbip-receiver-setup.sh attach` ②`emulation_backend = "usbip"` 重启 ③Windows usbip-win 实验性）。刻意**不做 select**：daemon 无运行时热切、无 SetBackend IPC（D25①）；素材取 README:621 + M1-REPORT。
- **增量九·UI 集成测试三枚**（ui_tests.rs 新文件，mod.rs `#[cfg(all(test, feature="ui-test-support"))]`）：
  - `settings_show_backend_section_with_usbip_guidance`（开设置 → 断言 settings-backend/backend-usbip-card 存在且可见）
  - `debug_toggle_reveals_log_bar`（log-body 初始缺席 → set_debug_open 后可见）
  - `clicking_device_row_selects_it`（喂 Created 事件 → **真实鼠标事件序列**点击 device-row → 断言 selected==7 + device-detail 渲染）
  - 运行：`cargo test -p mousehop-console --features ui-test-support`（16/16 过；默认 13 过、模块被 cfg 排除）。
  - 被测元素加 `.test_support()` 标记（mod.rs 设备行、detail.rs device-detail、logbar.rs log-body、settings.rs 新区两容器）——无 feature 时恒等返回零开销。
- **验证**：console 16 测试全过、clippy 双配置（默认/带 feature --all-targets）零告警、fmt 干净；daemon `--no-default-features` 构建正常。
- **gpui-kit 上游问题 5 项成文**：`.relay/journal/GPUIKIT-ISSUES.md`——①gpui-kit/test-support 解析破坏（今日新发现，含三组对照实验：声明即炸/dev-dep 即炸/三件套直连不炸）；②gpui-component 0.8.0 在 gpui-base/test-support 下编译失败（Observed 缺 ScrollableElement 接线，popup_menu.rs:1537 vs scrollable.rs:313 的 cfg 错层）；③D21④ AlertDialog on_ok 锁死；④D21⑤ ghost 按钮死区；⑤D23③ 监听器内 window.update 再入被吞。优先级 1>2>3>5>4，**等公子上报**。
- 提交：见 git log（feat(console) M2b 增量九 + chore(relay) 账本）。

## 关键决策与事实（D25 已录 PLAN）
- **后端区=说明+指引而非 select**：service.rs:237-238 仅启动构建 Emulation；后端 live 切换（新 IPC SetBackend + daemon 重建实例 + 按键状态迁移）需拍板后再做。
- **gpui-kit/test-support 解析雷**（D25②，最重要）：下游**声明即入 lock 解析**（不启用也炸，最小实验两次复现）→ gpui-fast 0.1.10 精确钉（cocoa =0.26.0 / unicode-properties =0.1.3 / cfg_aliases 旧版）与 gpui-pre 系浮动版本无解；dev-dep 同炸。**解法 = 三件套直连**：`ui-test-support = ["gpui/test-support", "gpui-base/test-support", "gpui-component/test-support"]`（gpui-pre 的 lib 名就是 `gpui`，Cargo.toml 以 `package = "gpui-pre"` 改名引入）；gpui-component 必须同开（否则 Observed 缺 ScrollableElement 编译失败）。代价：Cargo.lock 多 ~300 行 gpui-fast/proptest 可选依赖节点（**不编译**，无版本冲突）。
- **无头测试助手要点**（ui_tests.rs 免翻上游源码）：`frame = window.refresh(); window.draw(cx).clear(cx);`；`find = gpui_kit::base::test_support::find(window, &[], &id)`；click = MouseMove→MouseDown→MouseUp 各派发后一帧；MouseDownEvent 有 `first_mouse: bool` 字段；`to_platform_input()` 需 `gpui::InputEvent` trait 作用域；`cx.update_window` 是 **AppContext trait** 方法（`use AppContext as _`）、闭包三参 `(AnyView, &mut Window, &mut App)`；ElementId 非 Copy。
- **冒烟环境**：Xvfb :98 需 ≥1300×870（1180×780 窗口落在 (120,80)，1280×800 截断）；齿轮 = 自机栏行右端 (390,806)；scrot 截图可用。
- 沿用：daemon `--no-default-features` 与 console 分开构建；`RUSTUP_TOOLCHAIN=1.96.1`；Xvfb SVG 图标不渲染（盲点操作）；`pkill -f "Xvfb :9[8]"` 括号法。

## 下一步精确动作
1. **等公子拍板四件**：① `.relay/DESIGN-access-password.md` 三问 → 浮层「安全」区；② D23① 移出布局语义（`Position::None` 全链路 vs 映射停用）→ 槽内「× 移出」；③ 发送侧字段进编辑表单否（IPC 变体已在 mousehop-ipc/src/lib.rs:806-812）；④ 后端 live 切换（SetBackend IPC + daemon 热切）要不要进 M2b（本会话按「说明+指引」交付，见 D25①）。
2. 一控多会话级冒烟（三实例：A 控制端 dummy capture + console，B 右 C 左 X11 emulation；复用 M2a-REPORT 的 `MOUSEHOP_DUMMY_CAPTURE_DELAY_MS` 方法与 /tmp/mh-inc9/ config 模板）——补齐验收项 4。
3. M2b 技术报告（.relay/journal/M2b-REPORT.md）：素材 = 上表 7 项盘点 + 各增量提交 + 已知限制（真机欠账/Xvfb 图标/离线 0.1ms 延迟显示）。
4. gpui-kit issues 上报（材料在 GPUIKIT-ISSUES.md，可直接翻译提交）。

## 风险与阻塞
- 真机欠账（等用户硬件）：M1 vhci 注入、M2a 真实两机反向、M2b 验收项 5 的实际切换、Xvfb 图标与日志栏观感。
- 本机环境：GTK4 dev 缺失 → workspace 全量 clippy 含 mousehop-gtk 不可跑（D24⑤）。
- Cargo.lock 新增 ~300 行 gpui-fast/proptest 可选依赖节点（三件套 feature 声明的解析税，不编译、无冲突）；若公子嫌脏可评估 CLI-only feature 方案（`cargo test --features "gpui/test-support,..."`，但 cfg 门控的模块名需同步改）。
- 冒烟清理已做：daemon/console/Xvfb :98 全停；/tmp/mh-inc9/ 留证（u1/u2/u3/b2 + config + 日志 + Cargo.toml.bak 可删）。

## 上下文指针
- 计划：`.relay/PLAN.md`（D14–D25）；上篇：HANDOFF-012（增量七+八）
- 上游问题：`.relay/journal/GPUIKIT-ISSUES.md`
- 冒烟：/tmp/mh-inc9/（u3-settings.png = 后端区渲染证据；b2-reconnect.png = 崩溃隔离证据）
- 测试：mousehop-console/src/console/ui_tests.rs（`--features ui-test-support`）
- 设计稿：/home/zero/.zcode/v2/visualizations/3d252e46…/mousehop-console-v2.html（常规页「后端」区）
