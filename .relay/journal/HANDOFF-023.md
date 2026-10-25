# HANDOFF-023（M3a 增量④ console 画面挂接 + 画面开关 → 增量⑤ PLI+parity）
会话：#16（session_2ec746066addb7552770）  日期：2026-10-25  基线：feat/usbip-emulation@fbf11b2 + 未提交工作区（T3+T4 全部改动，见下）

## 一句话状态
T4 **全链路落地并冒烟全过**：console 画面面板 + UI 开关 + IPC SetVideoView/VideoState + `video_feed` 旁路二进制 socket + VideoPeer offer 存档/set_view 驱动 dial；**②b pre-Hello 竞态一并修复**（白名单放行 VideoOffer，首连即达 offer）；**下一任务 = T5 PLI 关键帧重请求 + 可选 XOR parity（默认关）**。

## 已完成（含证据，写前已验证）
- **改动面**（git status 20 modified + 3 new T4 相关；与 T3 未提交改动叠加在工作区）：
  - `mousehop-ipc/src/video_feed.rs`（新，~330 行含测试）：帧记录 `[w u16][h u16][index u64][len u32][BGRA]` 大端；`VideoFeedHandle`（bounded(2) channel + try_send 满即丢 latest-wins，写任务 spawn_local）、`connect_video_feed`/`read_video_frame`（console 侧阻塞读）；socket = `$XDG_RUNTIME_DIR/mousehop-video.sock`。ipc 主 tokio 加 `rt` 特性（spawn_local）+ dev-dep。
  - `mousehop-ipc/src/lib.rs`：`FrontendRequest::SetVideoView(ClientHandle, bool)` + `FrontendEvent::VideoState{...}`（全字段 `#[serde(default)]`）；serde 兼容测试。
  - `mousehop/src/connect.rs`：`handshake_allows_event` 白名单加 VideoOffer/VideoStop（**D39③，②b 竞态修复提前落地，增量⑥缩水**）。
  - `mousehop/src/video.rs`：Peer 重构——offer 存档（`StoredOffer` 新代旧）+ `views: HashMap` 偏好持久 + `set_view`/`state()`；dial 时惰性 spawn feed serve；receive_loop 解码后 `feed.push`（帧 index 过线恒 0，console 本地计数）。`VideoStateInfo` 顶层定义，两 imp 变体同签名。
  - `mousehop/src/capture.rs`：`CaptureRequest::SetVideoView`（内外两个请求循环都接）+ `ICaptureEvent::VideoState` + emit 三处（handle_proto/peer_gone/SetVideoView 后）。
  - `mousehop/src/service.rs`：`video_state` 镜像 + `SetVideoView` 分发 + `VideoState` 广播 + sync 推送。
  - `mousehop-console/src/console/video.rs`（新）：VideoView（buffer 覆盖写 + revision + painted RefCell 缓存 RenderImage；读线程 std::thread 250ms 读超时 + stop flag；**连接重试窗 5s** 吸收 socket 惰性绑定竞态）+ `spawn_tick_pump`。
  - `mousehop-console/src/console/{mod,detail}.rs`：Console.video 字段 + VideoState 分发 + on_video_tick + detail「画面」区（Switch 镜像 viewing + 画面面板 + 帧计数 + stats 行）；原 disabled 占位按钮改为 `disabled(!available)`。
  - `mousehop-console/Cargo.toml`：`image = "0.25"`（与 gpui-pre 锁定同版 0.25.10）。
  - `README.md`：Screen Sharing 段补 console 开关与 video feed socket 说明。
- **测试**：ipc 18 绿（hub 环回/帧编解码/serde）；daemon `--no-default-features --features x11_capture,x11_emulation,video` 114 绿；console 13 绿；clippy 三包净（仅预存 `start_service` dead_code）。
- **E2E 冒烟全过**（/tmp/mh-video1，findings「T4 E2E 全通过」）：
  - 首连即达 offer（昨日 0/7 → 今日 1/1）；Sync 镜像 `VideoState{handle:0, available:true, 320x180@30}`；
  - `SetVideoView(0,true)` → dial → 解码 → feed socket 4s 收 **120 帧精确 30fps**，payload 全 =230400，dummy 计数器 112→122 连续 lost=0；
  - off → `receive channel torn down (view toggled off)` + 帧停止；**UI 实点开关**（xdotool）off/on 双向全过；
  - console letterbox 正确（黑边 201px + 彩条 56px/条 = 455px 图像宽 = Contain 语义）；截图 `/tmp/mh-video1/console-video-panel4.png`、`console-toggle-off.png`、`console-toggle-on_000.png`。

## 关键决策与死路（防重蹈）
1. **D39 全文在 PLAN.md，必读**。最疼四条：
   - **gpui-pre Img 布局坑（D39②）**：`Img::request_layout` 无条件注入 aspect_ratio，taffy 会用它覆盖 definite 宽高对——`w_full().h_64()` 实测渲染 857×482 被父裁剪（看似拉伸）。正解 = **只定高**（`img.h_64()`），宽由比例推出，外层 `h_flex().justify_center()` 补黑边。
   - **LocalSet 测试死锁（D39③）**：`spawn_local` 任务与 block_on 主 future 同线程，async 块里阻塞 read_exact = 全局饿死（hub 测试首版挂死 30min）。阻塞 IO 必须独立 `std::thread` + oneshot 回传。另：hub adopt 前 push 落空（无读者即丢），测试须循环 push。
   - **feed socket 惰性绑定**：首次 dial 才 `spawn_local(serve)`（`VideoPeer::new` 会被 sync 测试调用，不能无条件 spawn_local）；console 读线程 5s 重试窗配套。
   - **pkill 自杀三连**：`pkill -f` 模式匹配到自己 shell 的命令行（rustfmt 参数含 `mousehop-console/...`、启动路径含二进制名）——清理进程的命令与引用该名字的命令必须分两条 Bash 跑，或用 `[e]` 括号技巧且确保同命令行无其他真实匹配串。
2. **冒烟环境**（沿用 D30②/D38⑧）：daemon 真身证书 `~/.config/mousehop/mousehop.pem`（74:ba 指纹已入 B config）；`MOUSEHOP_LOG_LEVEL=debug`（RUST_LOG 无效）；daemon/console/Xvfb 用 `setsid nohup ... & disown` 起，否则工具会话结束即被回收；本机 IP 192.168.101.16 未变。**本次冒烟进程已全部清理**（仅存上个会话遗留 Xvfb :99）。
3. **语义定案**：A 侧画面默认关（自动 dial 已移除）；B 侧 `[video] enabled` 仍是总闸；offer 跨会话存档 + view 偏好跨会话持久（重连自动恢复画面，若开关还开着）。

## 下一步精确动作（T5：PLI 关键帧重请求 + 可选 XOR parity，task claim T5 开始）
1. `task claim T5`。设计锚点：DESIGN-m3a-video.md §6（拍板项②已定：seq 缺口 → KeyframeRequest(PLI) + 可选 XOR parity N+1 默认关；不做 NACK 站）、D33③（包格式 flags 已留关键帧/parity 位）、D33⑤（`mousehop-video` transport 落点）。
2. 调研先行：`mousehop-video/src/transport/mod.rs` 现有 `Reassembler`（seq 缺口检测已有 `lost_frames()`）与 `Packet` flags 位定义；PLI 回传通道选型——**视频通道回传**（receive_loop 的 conn 已有双向能力，B 侧 read_loop 现在只观察关闭，v1 无反向流量，正好接 PLI）vs 控制通道（要经 emulation reply 路径，绕）。建议：视频通道回传，`ProtoEvent` 不动（PLI 是传输层信号，走 1 字节包类型或 flags 即可，注意 `Packet::from_bytes` 前向兼容）。
3. parity 开关落 `[video] parity = false`（config 宽松解析惯例照抄 source）；编码侧 N 包+1 奇偶包一组，解码侧缺口 XOR 恢复；单测注入丢包表（DESIGN §10）。
4. 验收：mousehop-video 单测（丢包注入矩阵：丢 1 包恢复/丢 2 包放弃→PLI）+ daemon E2E（可复用 /tmp/mh-video1，人为丢包用 tc 或临时 hook——注意无 root 时 tc 不可用，备选 = Reassembler 层注入测试）。
5. T5 后候选：增量⑥剩余池（D39③）→ 真机批次（Wayland portal + M2c 复验，等公子）。

## 风险与阻塞
- **改动未提交**：T3（12+1 文件）与 T4 叠加在工作区（20 modified + 3 new + HANDOFF-022/023）。提交建议分两笔：`feat(video): M3a increment 3 — portal capture backend (ashpd+pipewire-rs)` + `feat(video): M3a increment 4 — console picture panel, view toggle IPC, video feed sidechannel (fixes pre-Hello offer race)`，relay 记账随 chore 提交。
- portal 采集与 1080p 真机验收（延迟 <150ms、软编 CPU 占用）仍排真机批次，等公子。
- mousehop-video 引入 ashpd 后 daemon/console 同 cargo 调用构建禁令继续有效（D18/D24）。
- UI 细节欠账（不阻 T5）：画面面板在详情滚动区内，面板出现时需手动滚动才可见（Xvfb 870 高即如此）；帧计数 stats 行在面板下方同样需滚动。若公子要一屏可见，可把画面面板提到详情顶部或加自动滚动——待拍板。
- 勿做：gpui-kit 上报（D29）；真机 Wayland / M2c 真机复验（等公子批次）。

## 上下文指针
- 本次实现：`mousehop-ipc/src/video_feed.rs`、`mousehop-console/src/console/video.rs`（均新文件）、`mousehop/src/video.rs`（grep `set_view`/`StoredOffer`）、`mousehop/src/connect.rs:121`（白名单）、`mousehop-console/src/console/detail.rs`（grep `render_video_section`）
- 设计：`.relay/DESIGN-m3a-video.md`（§6 传输/§8 渲染/§9 落点）；D39 = PLAN.md 增量④全文；上篇：HANDOFF-022
- 冒烟配方：/tmp/mh-video1（A/B config + run.log + 截图；A config 无 [video] 段属正常——A 侧看画面不需要）
- 计划：`.relay/PLAN.md`（焦点已刷，T4 已勾）；任务账本 plan.json 4/5 done
