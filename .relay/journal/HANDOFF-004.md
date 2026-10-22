# HANDOFF-004（会话 #4 → 会话 #5）
会话：session_44de4d0（接力 #4）  日期：2026-10-23  基线：feat/usbip-emulation@44de4d0

## 一句话状态
**M2a 全部完成**（M2a-ii：控制输入流已桥接到反向隧道，双实例 E2E 注入验证通过）；下一里程碑 M2b（GPUI 控制端 UI）**挂起——等公子查看确认设计稿后才可开工**。自动排程已按计划停止。

## 已完成（含证据）
- commit `44de4d0` feat(ipc): bridge control input onto the reverse-dialed session (M2a-ii)（7 文件，+491/-62）
  - `listen.rs`：accept 注册路径抽为 `register_accepted_conn`；新增 `MousehopListener::adopt`（反向拨号连接按入站会话注册）与 `take_conn`（收养时取走 conn）；**修复孤儿 hello_watchdog 12s 必杀收养连接的 bug**（关闭前校验 `accepted_connection_is_current`）。
  - `connect.rs`：`connect_to_handle` 的 Reverse 分支（拨号成功 → 簿记 + `AdoptReverse` 交 listener，不 spawn connect 侧三任务）；`adopt_inbound`（装 slot + spawn hello/ping_pong/receive_loop）；`MousehopConnection::new` 增 `reverse_adopt` 通道参数。
  - `emulation.rs`：`EmulationRequest::{AdoptReverse, TakeConn}` + ListenTask 处理（TakeConn 附带簿记清理）+ `Emulation::{adopt_channel, take_conn}`。
  - `service.rs`：构造顺序调整（Emulation 先于 Connection 以接通道）；`maybe_adopt_reverse_session`（get_client + Reverse + 非拨号回声 + 无活跃传输四道闸）。
  - `input-capture/src/dummy.rs`：`MOUSEHOP_DUMMY_CAPTURE_DELAY_MS`（默认 0，行为不变）——E2E 确定性触发合成越边。
  - README：Connection Modes 增补反向隧道输入方向说明与单向限制。
- E2E 验证（单机双实例 + Xvfb :93/:94）：B 反向拨号 → 双侧收养（日志：`adopting reverse-dialed ... as inbound session` / `client (0) adopting inbound transport`）→ 延迟 12s 的合成 Begin 在隧道上完成 Handover → **B 的 Xvfb 指针连续跟踪注入轨迹**（xdotool 采样 640→839→568→562→690），60s+ 零真实掉线。A 侧 client 端口故意配 4399（不可达）以排除 direct 干扰。
- 测试：mousehop 83 / input-capture 70 / input-emulation 23 全绿；fmt + clippy 干净（仅预存 `start_service` 警告）。
- 技术报告：`.relay/journal/M2a-REPORT.md`（含验收对照表、架构图、已知限制）。

## 关键决策与死路
- **D11–D13 已写入 PLAN**（拨号回声闭环、watchdog 修复、单向输入限制 + E2E 方法），新会话勿重查。
- 死路：① dummy 的 Begin 每后端实例仅一次，靠启动相位与 B 的 5s 拨号竞速不可靠（试了 3 种相位差，最差差 1s）→ 改为 dummy 延迟环境变量；② `cli enable-capture` 在会话活跃时只 release 不重建后端，无法重新触发 Begin。
- 本机 X11 capture 是未实现 stub（`X11 input capture is not yet implemented :(`），capture 只能走 dummy——E2E 只能验证到「capture 事件之后」的管线。

## 下一步精确动作
**无自动下一步**：M2b 挂起等用户确认设计稿（`/home/zero/.zcode/v2/visualizations/97edd8bc06635e4af0dc6710e96b262848aa423f5b02ef52c79aa28b460977e9/mousehop-console-design.html`）。用户确认后：① 跑通 `Ely-GPUI-Components/compat/kit` 示例（M2b 开工前置）；② 按 PLAN M2b 验收清单开工。若用户想先做真机验证：M2a-REPORT「已知限制」第 1 条 + M1 的 vhci 清单。

## 风险与阻塞
- 反向模式要求双侧均为 `44de4d0` 之后版本；老对端退化为增量一行为（会话通、输入不走隧道）——UI 文案应提示版本。
- 真实 NAT 两机场景未验证（等用户环境）；双方互标 reverse 且双向可达时 active_addr 短暂抖动（~2s 自愈，见 M2a-REPORT 已知限制 3）。

## 上下文指针
- 计划：`.relay/PLAN.md`（D7–D13 + 测试环境事实必读）；上篇：HANDOFF-003；本里程碑报告：journal/M2a-REPORT.md
- 测试脚手架：/tmp/mh2a/（a.toml 的 B 条目端口=4399 是故意的；b13.log/a12.log 为最终验证轮日志；Xvfb 用 `-screen 0 WxHxD` 语法）
- M2b 设计稿（等确认）：/home/zero/.zcode/v2/visualizations/97edd8bc06635e4af0dc6710e96b262848aa423f5b02ef52c79aa28b460977e9/mousehop-console-design.html
