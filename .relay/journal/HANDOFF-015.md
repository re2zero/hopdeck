# HANDOFF-015（M2b 增量十二 → 会话 #N+1）
会话：session #11（续 #10，见 HANDOFF-014）  日期：2026-10-24  基线：feat/usbip-emulation@afce68d

## 一句话状态
公子四项拍板**全部落地**：增量十二（`6f775e7`+`afce68d`）访问密码全链打通并冒烟五项全过（弹框/错拒/限速/上线/不落指纹）；M2b 剩收尾项（一控多冒烟、技术报告、gpui-kit 上报）。

## 已完成（含证据）
- **增量十二（拍板①，`6f775e7`）**：
  - **proto**（mousehop-proto/src/lib.rs）：`CAP_ACCESS_PASSWORD = 1<<2` 入 PROTOCOL_CAPABILITIES；事件 tag 26 `AuthRequired{salt,nonce}`（33B）/ 27 `AuthPassword{proof}`（33B）/ 28 `AuthResult{ok}`（2B）；`MAX_EVENT_SIZE` 26→33（`MAX_AUTH_EVENT_SIZE`=1+32 取 max；纯内存缓冲上限，wire 兼容，老 Hello 17B 补零解码语义不变）；编解码测试 4 个（tag 稳定/round-trip/截断拒收）。
  - **crypto.rs**：`access_password_proof(salt,nonce,password) = SHA-256(salt‖nonce‖password)`。
  - **config.rs**：`access_password: Option<String>`（ConfigToml + getter；v1 明文，README 注记）。**ipc**：`FrontendEvent::PasswordRequired{addr:IpAddr}` + `FrontendRequest::SubmitAccessPassword{addr,password}`。
  - **接收端**（listen.rs）：`AuthGate{password,authorized,failures}`；verify 闭包密码模式放行未授权对端（指纹仅 INFO 日志）；`register_accepted_conn` 对需挑战会话**不发 Accept**；read_loop 挑战状态机——Hello 到达时查 capability 位/限速/授权，发 AuthRequired（`self_reply` 直写 conn）；AuthPassword 验证→成功发 AuthResult{true}+发布 Accept（先于 peer 下个 Hello，FIFO 保序）+清失败计数，失败记数+AuthResult{false}+断开；限速 3 次/30s/IP 滑动窗口，挑战开启与验证前都查；watchdog 改共享 deadline Cell（每连接，挑战中推 +120s，成功回 +12s），recv idle 挑战中 150s。**emulation.rs 零改动**。
  - **发起端**（connect.rs）：receive_loop 新 AuthRequired 臂——auth_pending 置位 + 事件透传 + 250ms 轮询 `pending_passwords`（Rc<std::sync::Mutex<HashMap<IpAddr,String>>>，MousehopConnection 新字段 + `submit_access_password`）+ proof 作答；AuthResult{false}→清密码+断开；`handshake_allows_event` 放行 Auth\* 三事件（测试更新）；hello_handshake 在 auth_pending 时停预算（总顶 150s）、ping_pong 无响应判定在 auth_pending 时 continue。
  - **capture.rs/service.rs**：ICaptureEvent::PasswordRequired(ip)（capture 泵 AuthRequired 臂）→ service `notify_frontend(PasswordRequired)`；SubmitAccessPassword handler 直写 conn；`access_password` Arc 热重载随 config reload 刷新。
  - **console**（mod.rs）：`auth_password` InputState 实体 + `prompt_access_password`（对 `has_active_dialog` 守卫；on_ok 发 SubmitAccessPassword，空密码不发送）。
- **冒烟金证**（/tmp/mh-inc12/，双实例 10.8.15.60:4301(A)/4302(B)，`--capture-backend dummy`，Xvfb :97(A console)/:98(B console)）：
  - B 点「连接」→ A `admitted unauthorized peer … protocol challenge follows` + `withholding session` + `challenging`；B daemon `<==<== AuthRequired(challenge)` → capture `auth challenge from client 0` → service `requires the access password; prompting frontend` → **B 弹「对方需要访问密码」框**（s10 截图：说明文+输入框+取消/发送密码）。
  - 错密码 `wrongpass`：B `answering…` → A `failed the access-password challenge — rejecting`（共 6 次）→ B `rejected the access password — closing connection`，行回离线、暂存密码已清。
  - **限速**：3 次/30s 窗口满后第 6/7 次 dial 被 A 直接拒（两次 `too many failed access-password attempts — cooling down`，不再挑战；手动慢节奏跨窗口时不触发=滑动窗口语义正确）。
  - 冷却 31s 后对密码 `test1234`：A `access password accepted …; session admitted` → B `accepted the access password; completing handshake` → **B console 设备「在线」+0.6ms 延迟**（s13 截图）。
  - **不落指纹**：A config.toml 仍只有 `port`+`access_password`；A console 设备 0 台/信任区空（s14）。
- daemon 84 测试过、console 16 过（`--features ui-test-support`）、proto 21 过；clippy 双配置零告警（`start_service` dead-code 为 `--no-default-features` 组合预存）。提交：`6f775e7`（实现）、`afce68d`（文档 README Access Password 节 + DOC.md 协议走查 + 探针清理）。

## 关键决策与事实（D28 已录 PLAN）
- 门禁上移协议层：Accept 挂起到挑战通过，emulation.rs 零改动；失败会话对上层是 Disconnected-noop。
- capability 位当版本探测器：A 拒 "does not support access-password auth" = 对端二进制是旧的。
- **冒烟三坑（新）**：(a) daemon 日志开关是 `MOUSEHOP_LOG_LEVEL`（main.rs:58 `filter_or`，**RUST_LOG 无效**）；(b) **无 capture 后端时 capture 泵退出**（capture.next()→None→select 循环 return），connect 侧 Received 无人排空、PasswordRequired 永不到前端——双实例冒烟 A/B 都必须 `--capture-backend dummy`；(c) `--cert-path` 必须文件非目录。
- 沿用：`RUSTUP_TOOLCHAIN=1.96.1`；Xvfb :98 ≥1300×870；console 无 WM 下窗口 @ (120,80) 1180×780，搜索框 (258,134)，详情「连接」(456,521)，AlertDialog 输入框 (700,265)、Enter=on_ok；daemon 重启后 console 必须重启（D27③）；pkill 清理与启动分两条命令防自戕（模式变量间接引用）。

## 下一步精确动作
1. **一控多双被控会话级冒烟**（M2b 待办①）：三实例方案照 HANDOFF-013 §下一步 2——A 控制，B/C 双被控；A dial B、A dial C，验证双会话并存 + A console 布局两槽同亮。注意本次新坑：A/B/C 都 `--capture-backend dummy` + `MOUSEHOP_LOG_LEVEL`。
2. **M2b 技术报告**（.relay/journal/M2b-REPORT.md）：素材 = HANDOFF-013 七项盘点表 + 增量十/十一/十二（`6850683`/`76d70ac`/`6f775e7`）+ 已知限制（真机欠账、console 不重连、Xvfb 图标、访问密码 v1 明文）。
3. gpui-kit issues 上报（.relay/journal/GPUIKIT-ISSUES.md 就绪，等公子）。
4. 冒烟环境清理：`/tmp/mh-inc12/` 留证（s9–s14 截图 + a/b-daemon.log）；`pkill` 括号法清 Xvfb :97/:98 与 mousehop 进程。

## 风险与阻塞
- 真机欠账不变：vhci 注入打字验证、真实两机反向、Xvfb 图标/日志观感。
- 访问密码 v1 限制（设计内）：明文落 config（README 注记）；接收端设置项 UI 未做（config.toml 手填）；错密码后 console 无专用失败提示（行回离线，重连再弹框）。
- console 不自动重连 daemon 重启（D27③）——产品语义待公子定。
- proto 加事件 = wire 追加 tag（已实测老二进制被 capability 门禁快速拒绝，无版本 bump）。

## 上下文指针
- 计划：`.relay/PLAN.md`（D14–D28）；上篇：HANDOFF-014（增量十+十一）
- 密码蓝本：`.relay/DESIGN-access-password.md`（已全量兑现）
- 实现：mousehop-proto/src/lib.rs（事件）；mousehop/src/listen.rs（AuthGate+挑战）；mousehop/src/connect.rs（auth_pending+pending_passwords）；mousehop/src/capture.rs+service.rs（事件链）；mousehop-console/src/console/mod.rs（对话框）
- 冒烟：/tmp/mh-inc12/（s10=弹框、s13=在线、s14=A 信任区；a/b-daemon.log=全链日志）
- 上游问题：`.relay/journal/GPUIKIT-ISSUES.md`
