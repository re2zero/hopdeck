# HANDOFF-030（会话 #23 → 会话 #24）
会话：#23  日期：2026-10-26  基线：`feat/usbip-emulation@aacbeee`（真机批次 #2 收口）

## 一句话状态
公子提供的 UOS 25 虚拟机（uos@192.168.101.74）上完成了 **M3a 视频真机批次**（portal 采集崩溃修复 cc17907、监听生命周期修复、端到端延迟 p99 93ms 达标）、**M2c 反向隧道真机复验**（含 allow-list 拒绝与反向会话 5s 抖动修复 aacbeee）、**输入指针注入真机复验**（X11 后端）。报告在 `.relay/journal/REALMACHINE-REPORT-2.md`。剩余：交互式 portal picker 未验（VM 的 DDE portal 不可用、wlr 自动选屏）、键盘/USB-IP 注入未覆盖、P1.5 仍待新会话。

## 已完成（含证据）
- **portal 采集崩溃**（cc17907）：portal-wlr 的 spa buffer 只标 MAPPABLE → libpipewire 映射 PROT_NONE → 首读 SIGSEGV。修复＝去 MAP_BUFFERS + 自行 mmap MemFd；真机 720 帧 lost=0，帧转 PNG 为 UOS 桌面。
- **video 监听生命周期**（cc17907）：入站断开 `halt()` 连监听一起关，重连只发 offer 不重建宿主 → 画面共享永久失效。新增 `Host::stop_sending()`（取消/替换发送会话 token，监听与 portal 采集存活）+ 单测；真机重启 B 后同一端口重新拨号成功（120 帧 lost=0）。
- **延迟验收**：采集→解码 p50 62 / p95 85 / p99 93 / max 103 ms（<150ms），网络段 p50 2.1ms；口径 1600x1200、软编、不含 console 上屏。临时探针已全部移除。
- **反向会话抖动**（aacbeee）：D31④ 僵尸检测读 Entered 才填充的 `incoming_conn_info`，把健康会话每 5s 重置；改用 `live_incoming`（Connected 完整表）。真机 20s 零重置。
- **M2c**：B=exit（`[tunnel] 0.0.0.0:4253`，allow `127.0.0.1`），A=reverse + forwards；允许目标取到 B loopback 的 marker，目标越 allow 被拒。互标 reverse 的互拨回声用 D35③ 不可达端口（9999）抑制。
- **输入注入**：VM 切回 X11（display :1）；X11 采集后端未实现（README 未勾选项准确），采集用 dummy 源（`HOPDECK_DUMMY_CAPTURE_DELAY_MS`），注入用真实 X11 emulation；VM 指针实测连续变化，A 侧 `EmulationBackend=X11`/alive。
- **验收**：daemon 116 绿（+1 新单测）、video 4 绿、fmt/clippy 净。

## 关键决策与环境事实
- **VM 时钟坑（测延迟必读）**：VirtualBox guest 在 clocksource=tsc 时墙钟约 +4.6% 快并锯齿回跳，`kvm-clock` 无效；切 `acpi_pm` 后 ~20ppm 稳定。重启 VM 后需重做（运行时操作）。
- **DDE portal 在此 VM 不可用**：X11 无 ScreenCast 实现；treeland 下 DDE 后端卡 `XWaylandName`。视频验证走 wlr portal + 私有 dbus 会话；picker 交互仍未验。
- **X11 采集未实现**：`--capture-backend x11` 启动即报错；X11 桌面只能做被控端注入。
- **VM 收尾状态**：X11 + 自动登录 uos（display :1）；测试 daemon/HTTP 服务已停；treeland 的 `/usr/share/wayland-sessions` bind mount 已卸载；lightdm.conf 已恢复备份 + 自动登录两行。
- **后台进程坑（再次确认）**：`cd && nohup cmd &` 的 `$!` 是子 shell；pgrep/pkill 模式与同命令行字面路径共存会自杀；测试脚本一律拆条、用 `pgrep -fa` 记 pid。

## 下一步精确动作
1. **交互式 picker 真机验证**（唯一 M3a 未验面）：需要一台有完整 DDE/GNOME/KDE Wayland 会话的真机（非本 VM），观察 ScreenCast 选择器弹框 + RestoreToken 复用（token 持久化仍是后续增量）。
2. **P1.5 断开会话 + 真拒绝 IPC**（新会话专门做）：设计 `FrontendRequest::KillSession(handle)` / `RejectPeer(fingerprint)` 的会话击杀语义，踢 `live_incoming`/出站 conn，走 Disconnected 拆除链（含 video stop_sending）；UI 详情「断开」按钮 + 认证卡「拒绝」；auth 冷却/重试轰炸守卫。
3. **可选**：USB-IP 注入复验（需按 D31② 编 usbip 用户态工具并 attach）；键盘注入（需要真实键盘采集源，当前 X11 采集不可用）。
4. **待拍板**：daemon 在无输入事件时不自动重连（dummy capture 下 `alive=false` 停住），是否加独立重连定时器。
5. **沿用 HANDOFF-029 未清项**：GTK 包在 nix 环境/CI 的全特性构建验证；发布链 Secret/包名/AUR/cachix 配置。

## 上下文指针
- 提交：`cc17907`（portal+监听修复）、`5fa2799`（D52 记账）、`aacbeee`（反向存活判定修复）；本文件随 D53 记账提交。
- 报告：`.relay/journal/REALMACHINE-REPORT-2.md`（环境配方/缺陷 1–4/延迟表/M2c/输入/其他观察）。
- 计划：`.relay/PLAN.md`（D52/D53）；上篇 HANDOFF-029（改名+重构）。
- 测试配方：daemon `cargo test -p hopdeck --no-default-features --features video`（116 口径）；video `cargo test -p hopdeck-video --features portal`；release 构建特性 `video,video_portal,x11_capture,x11_emulation,usbip_emulation`。
