# HANDOFF-031（会话 #24 → 会话 #25）
会话：#24  日期：2026-10-26  基线：`feat/usbip-emulation@83685d3`（本次提交后）

## 一句话状态
按公子指令完成两件事：① **两端 X11 联调补测**——键鼠会话（dummy 采集 + X11 注入）、键盘实测、dummy 视频管线均通，同时确证 X11 真实采集/portal ScreenCast 是代码缺口；② **GTK 彻底移除、默认前端切 GPUI**——`hopdeck-gtk` 整包删除，`hopdeck` 无参启动守护 daemon 并拉起 `hopdeck-console`，Cargo/CI/release/Flatpak/Nix/AUR/文档全部去 GTK。默认启动冒烟在 Xvfb 上全链路通过（窗口出现、退出联动、端口与 socket 回收）。**同日续（10-27）**：默认前端 × daemon 与 UOS 25 VM 完成双向直连联调；控制台搜索框新增设备全链实证；修复「收到入站会话却显示离线」并双端截图确认在线（e8cafbc）。

## 已完成（含证据）
- **X11↔X11 键鼠**：dummy 采集 → DTLS → X11（XTest）注入，会话建立且指针连续变化；`test-emulation --keyboard` 已补实际按键发送（KEY_A press/release 每 2s），VM 的 xev 收到 KeyPress/KeyRelease。
- **X11 视频**：dummy 源管道跑满 208+ 帧 @640x480（编码/传输/解码全链）；真实屏幕采集在 X11 不可用——`X11 input capture is not yet implemented :(`，且 DDE/gtk/wlr portal 在 X11 均无 ScreenCast（`Portal request didn't succeed with no information`）。这是待实现代码，不是配置。
- **GTK 移除**：删除 `crates/hopdeck-gtk`（29 文件）及 workspace 成员/依赖、`hopdeck` 的 `gtk` 默认特性、`release-plz.toml` 引用、`scripts/copy-macos-dylib.sh`；图标源统一为 `apps/hopdeck/assets/com.hopdeck.HopDeck.svg`。
- **GPUI 默认启动**：`src/main.rs` 无参分支 → 定位同级 `hopdeck-console`、spawn 并 `wait`；同进程守护 `hopdeck daemon` 子进程（异常退避重启，控制台退出后 SIGINT→3s→SIGKILL 清理，Windows 直接 kill）。macOS bundle 将 console 复制进 `Contents/MacOS` 并由 `sign-and-notarize.sh` 单独签名；控制台加 Windows GUI subsystem 属性。
- **打包/CI**：release Linux 依赖改 `libwayland-dev libxkbcommon-dev`（保留 X11/xtst/dbus），tar 包同时收 `hopdeck` + `hopdeck-console` + 新图标路径；Windows 删除 gvsbuild/GTK/DLL，只发两个 exe；macOS brew 去 gtk4/libadwaita；Flatpak/Nix/AUR 安装 console 并使用新图标路径；metainfo/README/DOC/AGENTS 与主截图（`screenshots/hopdeck-console.png`，本次 Xvfb 实拍）同步。
- **冒烟证据**：Xvfb `:99` + openbox、隔离 `XDG_CONFIG_HOME`/`XDG_RUNTIME_DIR`。`pgrep` 见 launcher、`hopdeck daemon`、`hopdeck-console` 三进程；`wmctrl -l` 见 `HopDeck` 窗口；`hopdeck-socket.sock` 与 UDP 4252 就绪；`wmctrl -c HopDeck` 后 5s 内全部退出、socket 删除、端口关闭、launcher 退出码 0。
- **前后端 × VM 联调（10-27 续，e8cafbc）**：本机 Xvfb 与 UOS 25 VM（DISPLAY `:1`，`LIBGL_ALWAYS_SOFTWARE=1`）各跑默认 `hopdeck` 三进程；双向直连后两端控制台互见在线（0.7–1.2ms），详情页显示信任指纹/备注/版本。控制台搜索框输入 `192.168.101.99` 回车 → daemon `connect-to ... client 1` 重拨 + 配置落盘 + 列表「连接中」，全链打通（随后恢复配置备份）。关窗联动双端复验：本地 `wmctrl -c`、VM `xdotool key alt+F4` 后进程全退、socket 删除、端口关闭。
- **入站在线显示修复（e8cafbc）**：VM 明明在接收输入却显示「离线」——`alive` 只代表出站 transport。新增 `ClientState.incoming`（serde default）+ `ClientManager::set_incoming` + Service 在 Connected/Disconnected 刷新 `live_incoming` 对应客户端；console 在线判定与 journal 改为 `alive || incoming`。VM 实测接收侧 Sync `alive=false incoming=true` 且截图在线，拨号侧 `alive=true`。
- **验收**：fmt clean；daemon 116 / input-capture 70 / ipc 21（+1 前向兼容用例）/ input-event 10 / input-emulation 10 / proto 22 / video 36 与 console 22（+1 incoming 在线用例）全绿；两段 clippy（daemon workspace 与 console，`--all-features -- -D warnings`）均净。

## 关键决策与环境事实
- **ashpd 构建互斥（新增铁律）**：gpui-base 启用 ashpd `async-io`，daemon 的 libei/portal 启用 `tokio`，同一 cargo 调用必然 `compile_error!`。root `[workspace]` 用 `default-members` 排除 `hopdeck-console`；构建/clippy/test 两段执行（daemon `--workspace --exclude hopdeck-console`，console `-p hopdeck-console`）。CI、`.githooks/pre-commit`、Nix `postBuild`、Flatpak、AUR、release.yml 已同步。
- **默认启动的产品语义变化**：GPUI 控制台关闭即退出 HopDeck（含键鼠共享），没有 GTK 时代的托盘/菜单栏常驻。README/Windows/macOS 说明已按实际行为改写；如需「关窗后继续共享」需另立产品需求。
- **本机 Xorg 已满**：`:0` 的 256 个 client 槽被 root 桌面会话占满（254 个 ESTAB），GPUI 报 `Maximum number of clients reached`；冒烟一律用 Xvfb（`:99` + openbox）。不要据此判断 GPUI 故障。
- **发布链未本地验证**：release.yml 的 Linux/Windows/macOS 与 Flatpak/Nix/AUR 仅做了文本与命令级一致性修改，完整打包需 CI 或 nix 环境。
- **X11 能力边界不变**：发送端（采集）仍只能 dummy/Wayland；X11 仅可作为被控端注入。README 路标 `X11 Input Capture` 仍未勾选。

## 下一步精确动作
1. **P1.5 断开会话 + 真拒绝 IPC**（PLAN 中唯一剩余 P1 项，需新会话专门做）：`FrontendRequest::KillSession` / `RejectPeer` 的会话击杀语义、踢 `live_incoming`/出站 conn、走 Disconnected 拆除链（含 video `stop_sending`）；详情「断开」按钮 + 认证卡「拒绝」；auth 冷却守卫。
2. **CI 跑一遍两段构建/测试**（含 release workflow 的 tar/zip/dmg 产物核对），重点确认 `default-members` 对 `cargo build --release` 的生效范围。
3. **真机验证默认启动**：在公子实际桌面（Wayland/X11）双击桌面项或运行 `hopdeck`，确认同目录 console 被拉起、关窗后 daemon 退出；Wayland portal picker 仍是未验面。
4. **可选**：无 Vulkan 老 GPU 的 GPUI 花屏缓解（D31⑤，`LIBGL_ALWAYS_SOFTWARE=1` 可作排障口径）。
5. **沿用**：USB-IP 注入复验（需编 usbip 用户态工具）、键盘真实采集源（X11 采集未实现）。

## 上下文指针
- 本次提交：`0a8d32c`（GTK 移除/默认 GPUI）、`fd1ade2`（键盘测试修复）、`6708ba2`（记账 D54）、`e8cafbc`（入站在线显示修复 + 前后端 VM 联调）。
- 计划：`.relay/PLAN.md`（会话#24 / D54 / D55）；上篇 HANDOFF-030（真机批次 #2）。
- 报告：`.relay/journal/REALMACHINE-REPORT-2.md`（X11 注入与视频现场）；本轮的 X11 边界结论并入 D54。
- 测试配方：daemon 侧 `cargo test --workspace --exclude hopdeck-console --all-features`；console `cargo test -p hopdeck-console --all-features`；旧的无 GTK 头环境配方 `cargo test -p hopdeck --no-default-features --features video` 仍可用。
