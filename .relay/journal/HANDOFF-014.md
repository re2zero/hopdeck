# HANDOFF-014（M2b 增量十+十一 → 会话 #N+1）
会话：session_9f5fc17b8ccedb5ae0382dbe81e4ed8a（续 #9）  日期：2026-10-24  基线：feat/usbip-emulation@76d70ac

## 一句话状态
公子四项拍板中**三项已落地并冒烟**（增量十 `6850683` 移出=停用+编辑表单字段；增量十一 `76d70ac` 后端真切换，USB/IP 热切实证导出 3240 虚拟 HID）；**第四项（访问密码）拍板齐备但未开工**——下一会话第一件事。

## 已完成（含证据）
- **增量十（拍板②③，`6850683`）**：
  - 布局画布（layout.rs）：槽内右上角「移出/放回」按钮 → `Activate(handle,false/true)`（D26②：移除即停用，边缘不再触发——daemon `set_client_active`→`capture.destroy/create` 该侧实例）；停用盒暗显 + 「已停用」徽标 + edge_bar 变灰，槽位仍占用（容量守卫不变）；拖已停用设备到别的槽 = 放置并自动启用（place_device 里 `was_active` 检查）。
  - 编辑表单（detail.rs `render_send_option_rows`）：「输入映射」区 = Command→Ctrl Switch（仅 macOS 发送端 hint）+ 跨越需按修饰键 Switch + 修饰键 chips（Ctrl/Alt/Shift/Super，仅开启时显示）；草稿态、保存走 live `SetClientCommandAsCtrl/SetClientRequireCrossingModifier/SetClientCrossingModifier`（daemon handler 即时 capture 更新+落盘，已存在 service.rs:509-534）。
  - 冒烟（/tmp/mh-inc9/v*.png + daemon.log）：「放回」→ `activated client 0 (right)` + UI 徽标消失/edge 变绿；「移出」→ `deactivated client 0`；保存后 config.toml `command_as_ctrl = true / require_crossing_modifier = true / crossing_modifier = "alt"`。
- **增量十一（拍板④，`76d70ac`）**：
  - ipc：`FrontendRequest::SetEmulationBackend(Option<String>)`（config 拼写，None=自动）+ `FrontendEvent::EmulationBackend(Option<String>)` 回显（sync 时也推初值，service.rs sync_frontend）。
  - daemon：`apply_emulation_backend`（service.rs）——serde 解析拼写、**CLI `--emulation-backend` 覆盖在效时拒绝并回显现值**、`Emulation::switch_backend`（fire-and-forget）、`config.set_emulation_backend`（config.rs 新 setter，写 config_toml 层）、save、回显。
  - emulation.rs 热切链：`EmulationRequest::SwitchBackend` → ListenTask → `EmulationProxy::switch_backend`（10s oneshot 上限）→ `ProxyRequest::SwitchBackend`；EmulationTask 三处接入——session loop 停靠 `pending_switch` 后 `break Ok(())`（走正常会话收尾=释放按键）、`wait_for_termination`（覆盖创建/创建期窗口）塞 pending 后 return、run 外层等待 loop 直接应用；**run() 在 do_emulation 返回后优先应用 pending**（不等 Reenable），随即走既有重建循环：新建 InputEmulation + 刷显示缓存 + 重放 per-addr post_processing + create_clients；新后端起不来走既有 `EmulationDisabled`（console 状态栏「注入 关」）。连接簿记（handles/post_processing/clipboard）全程保留。
  - console：后端区 live chips 自动/X11/USB-IP（settings.rs `render_emulation_backend_row`），点选发请求，回显事件更新选中 + journal「仿真后端 → …」；USB/IP 指引卡第②步措辞同步更新。
  - **冒烟金证**（daemon2.log，构建 `--no-default-features --features "x11_capture,x11_emulation,usbip_emulation"`）：自动→X11→USB/IP→自动四次热切全通；`switching emulation backend → usbip` 后 **`usbip: virtual HID device exported on 127.0.0.1:3240 (busid 1-1)`**——M1 的 loopback server 热态起活；config.toml 逐步 `emulation_backend = "x11"` → 移除（auto）→ `"usbip"` → 移除。**M2b 验收项 5 机制面全通，仅剩真机 vhci attach 打字验证。**
  - 负例：未知拼写/缺 feature 时拒绝路径工作（`rejected emulation backend switch: unknown variant`，回显旧值）。
- console 16 测试过、clippy 双配置零告警、daemon `--no-default-features` 编译净（start_service dead-code 为该组合预存）。提交：`6850683`、`76d70ac`。

## 关键决策与事实（D26/D27 已录 PLAN）
- **D26 公子四拍板**：①密码不落指纹+nonce 挑战+3 次/30s 冷却；②移除布局=停用（非 Position::None 全链路）；③发送侧字段进编辑表单；④后端真切换。
- **D27 要点**：CLI override 在效时 UI 切换拒绝（否则 persisted 值 silently 不生效）；daemon 裸 `--no-default-features` 时 config::EmulationBackend 只剩 dummy（"x11" 被拒）——运行冒烟必须带 `--features "x11_capture,x11_emulation,usbip_emulation"`；**console 不自动重连 daemon 重启**（本轮冒烟实测：停在「服务已断开」，需重启 console；是否做自动重连待定）；usbip 热起日志自带 attach 指引。
- 沿用：`RUSTUP_TOOLCHAIN=1.96.1`；Xvfb :98 需 ≥1300×870；齿轮 (390,806)；设置浮层 chips X11(583,406)/自动(535,406)/USB-IP(640,406)；`pkill -f "Xvfb :9[8]"` 括号法；console 编辑表单滚动 = 在空白处 (700,520) click 5（滚轮向下，Input 上滚无效）。

## 下一步精确动作
1. **访问密码实施**（拍板齐：会话准入不落指纹 + nonce 挑战 + 3 次/30s 冷却；蓝本 = `.relay/DESIGN-access-password.md`，该稿协议设计一节可直接照做）：
   - proto：`CAP_ACCESS_PASSWORD = 1<<2`（Hello capabilities 位，wire shape 不变）+ 3 事件 `AuthRequired{salt:[u8;16],nonce:[u8;16]}` / `AuthPassword{proof:[u8;32]}` / `AuthResult{ok:bool}` + 编解码测试（mousehop-proto/src/lib.rs，事件 tag 追加先例见 ProtoEvent 现有 variants）。
   - daemon 接收端：config.toml 手填密码（v1，`access_password` 键，文档注明明文简化）；`verify_peer_certificate`（listen.rs:165-186）密码模式放行并记录指纹；握手前挂挑战（复用握手 watchdog 超时）；通过后会话级 allowlist（不动 authorized 表）；失败计数限速 3 次/30s。
   - daemon 发起端：connect.rs Hello 循环处理 `AuthRequired` → `FrontendEvent::PasswordRequired`；新 `FrontendRequest::SubmitAccessPassword`（内存暂存）。
   - console：发起端密码对话框（连接中收到 PasswordRequired 弹框）；接收端设置项 v1 不做（config.toml 手填）。
   - 冒烟：双实例（A config 设密码，B 搜索直连 → B 弹密码框 → 错密码拒绝 + 限速 → 对密码上线；A 控制台信任区不显示该指纹）。
2. 一控多双被控会话级冒烟（HANDOFF-013 §下一步 2 的三实例方案照用）。
3. M2b 技术报告（.relay/journal/M2b-REPORT.md）：素材 = HANDOFF-013 七项盘点表 + 增量十/十一 + 已知限制（真机欠账、console 不重连、Xvfb 图标、离线 0.1ms 显示）。
4. gpui-kit issues 上报（GPUIKIT-ISSUES.md 就绪）。

## 风险与阻塞
- 真机欠账不变：vhci 注入（现可全程热切到 USB/IP 后 `usbip attach` 验证）、真实两机反向、Xvfb 图标/日志观感。
- console 不自动重连 daemon 重启（D27③）——产品语义待公子定。
- proto 加事件 = wire 追加 tag（capabilities 协商后收发，老端忽略未知位）——设计稿论证过无版本 bump；实施时照稿核对。
- 冒烟清理已做；/tmp/mh-inc9/ 留证（v/w 系截图 + daemon2.log 热切日志）。

## 上下文指针
- 计划：`.relay/PLAN.md`（D14–D27）；上篇：HANDOFF-013（增量九+验收盘点）
- 密码蓝本：`.relay/DESIGN-access-password.md`（协议设计一节 = 实施规格）
- 上游问题：`.relay/journal/GPUIKIT-ISSUES.md`
- 热切实现：mousehop/src/emulation.rs（SwitchBackend 全链）+ service.rs `apply_emulation_backend` + config.rs `set_emulation_backend`
- 冒烟：/tmp/mh-inc9/（daemon2.log = 热切金证；v3-v7 = 增量十 UI 证据）
