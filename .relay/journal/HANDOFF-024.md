# HANDOFF-024（M3a 增量⑤ T5 + ⑥剩余池 T6 收口 → 真机批次）
会话：#17（session_d72ca869e039b3801aec）  日期：2026-10-25  基线：feat/usbip-emulation@2ab9826（工作区仅余 .relay 记账改动）

## 一句话状态
T5（PLI 关键帧重请求 + 可选 XOR parity，163bfa8）与 T6（halt 后 offer() 返 None，2ab9826）**全部落地并验收**，任务账本 6/6 清空；**M3a 代码侧完毕，仅欠真机批次**（等公子）。自动接力已按本会话决定不续排（账本无任务可领）。

## 已完成（含证据，写前已验证）
- **T5 = 163bfa8**（22 文件 +3052/−131，含 T3+T4 一并入库，见「关键决策」1）：
  - transport v2（`mousehop-video/src/transport/mod.rs` 全重写）：`FLAG_PLI=1<<4` 反向关键帧请求包（`Packet::keyframe_request()`，零 payload）；`Packetizer::with_parity(n)` XOR parity（sideband 不占 seq，payload 带成员 len/flags 表）；Reassembler v2 = pending BTreeMap（cap 128）+ recent VecDeque（64，供 straddling 组恢复）+ gap_armed 重同步闸 + `keyframe_requests()` 计数；push 返 `Vec<EncodedFrame>`。
  - 接线（`mousehop/src/video.rs`）：A receive_loop 300ms 限速回发 PLI（reassembler 缺口计数 + 解码 `Ok(None)` 非关键帧两路触发）；B read_loop 解析 PLI → mpsc → send_loop `force_key` 下一帧 IDR；parity 开关 → `Packetizer::with_parity(PARITY_GROUP_SIZE=8)`。
  - config：`[video] parity = false` 默认（round-trip 测试锁契约）；README Screen Sharing 补 parity/PLI 说明。
  - 测试：transport 32 单测（丢包矩阵：丢 1 恢复/丢 2 放弃→PLI/丢 parity→后续组 parity 证明后 resync/跨帧组/环绕组/畸形 payload/flood 有界）+ loopback 4 集成（parity on 丢 1 包 30 帧像素级精确零 PLI；parity off 丢包→下一 IDR 恢复）；daemon 115 + ipc 18 + console 13 绿；clippy 四包净。
  - **E2E 双 pass**（/tmp/mh-video1）：pass1 parity off = 420 帧@30fps counter=419 lost=0 + 开关双向；pass2 parity on = B 日志 'XOR parity every 8 packets'、A 端 120 帧 lost=0。
- **T6 = 2ab9826**（`mousehop/src/video.rs` +30）：`Host.halt()` 置 `halted` 闩，`offer()` 检查后返 None；单测 `offer_stops_after_halt` + E2E（杀 A → 'accept loop stopped' → A 重连新 DTLS session → A 侧零新 offer）。daemon 115 绿。
- 提交：163bfa8（feat, T3+T4+T5 合并）、2ab9826（fix, T6）；relay 记账随本 chore 提交。

## 关键决策与死路（防重蹈）
1. **T3+T4+T5 合并一笔 feat 提交（偏离 HANDOFF-023「分两笔」建议）**：三个增量的 hunk 在 `mousehop/src/video.rs`（portal glue / feed / PLI 混在同一 send_loop/receive_loop）、`config.rs`（Portal 变体 + parity 字段）、README 交错；按文件拆不出、按 hunk 拆会产出编不过的中间提交。已在 commit message 注明。
2. **D40 全文在 PLAN.md 必读**。最疼三条：
   - **parity=sideband 是硬设计**：parity 包不占 seq（丢 parity ≠ 假数据缺口）；成员 len/flags 必须进 parity 头（keyframe 每分片都带 FLAG_KEY、末片短——XOR 恢复要精确重建长度与标志位）。
   - **恢复 engage 窗口**：首个 parity 包到达后生效（首组裸奔、recent 同刻起保留）——测试必须先喂 warm-up 组，否则 recovery 因 recent 为空而 bail（三个测试首版全栽在这）。
   - **重同步闸 gap_armed**：无 parity 流缺口立即 arm（保 v1 快 resync）；parity 流靠「后续组的 parity 到达」证明缺口组 parity 已失。UDP 重排下 parity 先到会误 arm（已记录接受的限制，下一 IDR 自愈）。
3. **冒烟配方（D30②/D38⑧ 修订，D40⑦ 全文）**：`--capture-backend dummy` 双侧必带（A 无后端 = 连接/VideoPeer 全瘫，今日首踩浪费半轮）；A/B/console 三进程各自 `XDG_RUNTIME_DIR=/tmp/mh-video1/xdg-{a,b}` 隔离（console 用 xdg-a）；console 断 IPC 不自愈必须重启；**pkill 自杀坑第五/六变体**——bracket 只防 pattern 文本自身，同命令行任何其他位置含明文（启动行！）照样自匹配，pkill 必须独占一条 Bash。
4. **cargo test 不刷新 target/debug 可执行二进制**：改完代码做 E2E 前必须显式 `cargo build`，否则验旧码（T6 验证时首踩，A 复连仍收 offer 假阴性一轮）。
5. 冒烟进程已全部清理（仅存 Xvfb :99，上个会话遗留）。

## 下一步精确动作
- **等公子真机批次**（无自主可做任务，勿再开自动接力）：① Wayland portal 弹框 + RestoreToken 复用复验（真机桌面，D38⑥ 清单）；② M2c 真机复验（D31④ 缺陷 1 修复后的确认）；③ M3a 验收 = LAN 1080p 端到端 <150ms + 软编 CPU 占用（HD 6450，D34 拍板项①）；④ 顺带 gpui-kit 上报（D29：全部完成后报，材料在 `.relay/journal/GPUIKIT-ISSUES.md`）。
- 若真机批次发现问题需代码修复：新会话 `task add` 建任务后按账本走。
- 冒烟环境复用配方：/tmp/mh-video1（A/B config + xdg-{a,b} + 截图；B config 已含 `parity = true` 可作 parity pass 配方）。

## 风险与阻塞
- 全部剩余项依赖公子：真机测试机安排（uzero@10.8.15.62 或新机）、gpui-kit 上报、console 自动重连产品决策（D27③）。
- parity 已知限制（D40⑤，记录不做）：UDP 重排误 arm；`[video] parity` 需双端同版（README 已注明）。
- 勿做：M3b（零拷贝渲染/DMA-BUF/硬解导入）未拍板不动；UI 画面面板滚动可见性欠账待公子拍板（HANDOFF-023 遗留）。

## 上下文指针
- 本次实现：`mousehop-video/src/transport/mod.rs`（v2 全貌）、`mousehop/src/video.rs`（PLI/parity 接线 + halted 闩）、`mousehop/src/config.rs`（parity 字段）、`mousehop-video/tests/loopback.rs`（丢包矩阵集成）
- 设计：`.relay/DESIGN-m3a-video.md` §6/§10；D40 = PLAN.md 增量⑤全文 + D39/D38 前文；findings「T5④ E2E」「T6 增量⑥」两条
- 冒烟：/tmp/mh-video1（t5-*.png 截图序列）；E2E 配方见上「关键决策」3
- 计划：`.relay/PLAN.md`（焦点已刷：账本 6/6，仅欠真机批次）；上篇：HANDOFF-023
