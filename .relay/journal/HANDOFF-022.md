# HANDOFF-022（M3a 增量③ portal 采集落地 → 增量④ console RenderSurface）
会话：#15（session 未登记 ctx id，relay state.json 为准）  日期：2026-10-25  基线：feat/usbip-emulation@fbf11b2 + 未提交工作区（见下）

## 一句话状态
T3 **portal 采集后端全量落地**（mousehop-video `portal` feature + `[video] source` + daemon `video_portal` 接线 + VideoOffer 实际几何语义；测试/clippy 全绿）；**T4 = console RenderSurface 挂接 + 画面开关 IPC**；额外战果：②b 预存竞态实证并钉死与 T3 无关（D38⑦）。

## 已完成（含证据，写前已验证）
- **改动面**（git status 12 modified + 1 new，435+/37-）：
  - `mousehop-video/Cargo.toml`：`[features] portal = ["dep:ashpd","dep:pipewire"]`；deps 挂 `cfg(target_os="linux")` 表（ashpd 0.13.9 `default-features=false, features=["screencast","tokio"]` + pipewire 0.10.1）；dev-dep tokio 加 `test-util`。
  - `mousehop-video/src/capture/portal.rs`（新，~600 行含测试）：PortalCapture（ashpd 会话 + pw 泵线程）/PortalFrames（fps 限速 latest-wins Stream）/bgra_from_bgrx（stride+alpha）。设计全文见 PLAN.md **D38①-⑤**。
  - `mousehop-video/src/{capture/mod.rs,error.rs}`：portal 模块 cfg 门控（`all(target_os="linux", feature="portal")`，非 Linux `--features portal` 也编译通过）；VideoError 加 `Portal(String)`/`PipeWire(String)`。
  - `mousehop/Cargo.toml`：`video_portal = ["video","mousehop-video/portal"]`（不进 default）。
  - `mousehop/src/config.rs`：`VideoSourceKind{Dummy,Portal}` + `TomlVideo.source: Option<String>`（宽松解析，未知值 warn 回落 dummy）+ `VideoConfig.source`；测试 4 个（原 3 扩展 + lenient fallback 新增）。
  - `mousehop/src/video.rs`：imp 内 `portal_glue` 双变体（Linux+video_portal 真实现 / 其余 no-op stub，同签名，集成点零 cfg）；Host 增 `portal: Slot`，spawn 时 `source==Portal` 则 `spawn_setup`（eager 预启，弹框不阻塞）；`offer()` portal 几何未协商前返回 None（dummy 照旧 config 尺寸）；send_loop 源工厂（dummy=原 DummyVideoSource 逐字节不变；portal=subscribe + 8s/300ms 等待预算）；halt() 级联 stop capture。
  - `README.md`：Screen Sharing 段补 `source` 选项。
- **测试**：mousehop-video 21 绿（--features portal，含 portal 单测 4：bgra 转换 stride/alpha/拒绝 + pacing latest-wins（tokio paused clock 确定性）+ sender-drop 终止）/ 17 绿（无 portal）；daemon `--no-default-features --features x11_capture,x11_emulation,video(_portal)` 114×2 绿（113+1 新 config 测试）；clippy 四组合净（仅预存 `start_service` dead_code）。
- **E2E 定案**（findings「T3 E2E 定案」+ D38⑦⑧）：修正指纹后输入 DTLS+协议握手稳定建立；线上 `VideoOffer(:52575, codec=0, 320x180@30)` 与 ②b 逐字段一致（TRACE 实证）；视频全链今日不可达 = **②b 预存竞态**（基线 fbf11b2 与 T3 分支行为逐位一致——01:11-01:33 期间 stash 未恢复跑的全是基线二进制，意外完成完美对照组）。

## 关键决策与死路（防重蹈）
1. **D38 全文在 PLAN.md，必读**。最疼三条：
   - pipewire-rs 的 attach 回调要求 `'static` → 泵线程必须全 Rc 变体（MainLoopRc/ContextRc/CoreRc/StreamRc），`StreamBox<'c>`（PhantomData 借 Core）进不了闭包；`connect_fd_rc(OwnedFd)` 是 ContextRc 的方法（不是 connect_fd）。
   - ashpd 0.13.9 API 名：`Screencast`（非 ScreenCast）、`SelectSourcesOptions` builder、`start(...).await?.response()?` 返回 `Streams`（restore_token 在这，不在 session 上）、`open_pipe_wire_remote` 返 OwnedFd；`ashpd::enumflags2::BitFlags`、`ashpd::desktop::{CreateSessionOptions,Session,PersistMode}`（session 模块私有）。
   - **②b 竞态**：B 在 Connected 即 announce，A 的 connect.rs Hello 前丢 offer（`ignoring pre-Hello event`，DEBUG 级）；昨日 780 帧全通是时序运气。**T4/后续会话勿在旧假设「announce 必达」上设计**；修复归增量⑥（announce 挪 Hello 后 / A 缓存 pre-Hello offer + halt 后 offer() 返 None 或重启 accept loop——第二处不一致：断连后 accept loop 永久关但 offer() 照宣死端口）。
2. **冒烟环境**（D30② 修订）：daemon 真身证书 = `~/.config/mousehop/mousehop.pem`（指纹 74:ba:ad:da:56:6d:7f:75:16:b2:e1:1c:2e:3f:0e:26:51:5d:a4:e7:73:2f:be:3e:a1:cc:67:72:61:57:34:29），config.toml 旁的 cert.pem 是摆设；/tmp/mh-video1/b/config.toml 已改用真身指纹。机器 10-24 20:01 重启过，昨日环境结论须重验。daemon 日志开关是 `MOUSEHOP_LOG_LEVEL`（RUST_LOG 无效，D28⑤ 复验）。
3. stash 自坑：本会话 01:11 stash 后忘 pop，2 小时实验跑在基线上——**stash 操作与构建必须同条命令链完成并立即验证 `git status`**。

## 下一步精确动作（T4：console RenderSurface 挂接 + 画面开关 IPC，task claim T4 开始）
1. `task claim T4`。半增量调研先行：A 侧 VideoPeer（`mousehop/src/video.rs` grep `VideoPeer`）现只 log/计数，解码帧无 consumer——T4 给它接 console。设计锚点：D33①（RenderImage 单帧 BGRA + revision 驱动重绘，CPU 路径定案）、D17（console = gpui-kit 0.8.0 单一入口，ely 暂缓）、D22/D24/D25（console 冒烟规程与坑）。
2. 框架预估（按 HANDOFF-020:23 既有设计推进）：VideoPeer 解码帧交 consumer channel → IPC 事件或 console 直读 → console 新增画面元素（gpui render 闭包把 BGRA `Vec<u8>` 转 `RenderImage`，尺寸用帧自带 w/h）；画面开关 = 新 `FrontendRequest` 变体（mousehop-ipc，注意 serde 版本纪律：IPC 前向兼容用 `#[serde(default)]`），daemon 侧 dial/hangup video 通道（VideoPeer 已有 dial/teardown 原语，接线即可）。
3. 画面开关语义先想清再动工：v1 建议 = A 侧「查看画面」toggle → 主动拨 offer 端口 / 断开；B 侧 `[video] enabled` 仍是总闸（config 热重载已有 watcher）。与 D26④ 后端热切的 UI 模式对齐。
4. 验收：console 冒烟（Xvfb :97/:98 惯例，≥1300×870，D25④）+ daemon/console **分开构建**（D18 ashpd 互斥雷）；完成 `task done T4` + findings + 刷 PLAN。
5. T4 后 = T5（PLI+parity）；增量⑥候选池见 D38⑦（②b 竞态修复），建议排 T5 后、真机批次前。

## 风险与阻塞
- ②b 竞态未修 → **T4 的画面冒烟会因 offer 丢失而看不到流**（与 T3 无关）。T4 冒烟若复现：临时解法 = A 侧 TRACE 确认 `ignoring pre-Hello` 后重启 B 换新会话赌时序（今日 0/7），或先修增量⑥竞态再冒烟（约 +1 增量体积，公子拍板）。
- portal 采集无头环境不可自动化——真机验证（picker 弹框/RestoreToken 复用/真实桌面帧流）排真机 Wayland 批次（与 M2c 复验同批，等公子）。
- mousehop-video 引入 ashpd 后 daemon/console 同 cargo 调用构建禁令继续有效（D18/D24）。
- 勿做：gpui-kit 上报（D29）；真机 Wayland / M2c 真机复验（等公子批次）。

## 上下文指针
- 本次实现：`mousehop-video/src/capture/portal.rs`（未跟踪新文件）、`mousehop/src/video.rs`（grep `portal_glue`）、`mousehop/src/config.rs`（grep `VideoSourceKind`）、PLAN.md **D38**
- 改动未提交（12 modified + portal.rs untracked）；提交建议 = `feat(video): M3a increment 3 — portal capture backend (ashpd+pipewire-rs)`，relay 记账随 chore 提交
- 设计：`.relay/DESIGN-m3a-video.md`（§4 采集/§6 传输/§9 落点）；T4 锚点：HANDOFF-020:23、D33①、D17
- 计划：`.relay/PLAN.md`（焦点已刷）；上篇：HANDOFF-021；冒烟配方：/tmp/mh-video1（B 配置已带真身指纹 74:ba）
