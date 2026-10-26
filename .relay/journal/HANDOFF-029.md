# HANDOFF-029（会话 #22 → 会话 #23）
会话：#22  日期：2026-10-26  基线：feat/usbip-emulation@0074922（改名+重构已提交）

## 一句话状态
产品彻底改名 **HopDeck（桌联）** 并完成工作区架构重构，已提交 **0074922**（D51）；测试/ clippy（除 GTK 包）全绿。**唯一未验面＝`hopdeck-gtk` 未在本 agent shell 编译**（无 nix / 无 gtk4 pkg-config）。下一会话第一件事：在公子 nix 环境或 CI 跑默认特性全构建 + 双实例 E2E；P1.5（断开会话+真拒绝 IPC）仍未开工，真机批次仍待公子安排。

## 已完成（含证据）
- **同步**：`git fetch --all --prune` + `git pull --ff-only`，fast-forward `28a6daa..20f5f63`（远端 console 窗口控件去重修复：gpui-component 0.8 TitleBar 自带控件与自绘重复）。
- **目录重构（D51①）**：`crates/` 放 8 个库（hopdeck-input-event / input-capture / input-emulation / proto / ipc / video / cli / gtk），`apps/` 放 2 个可执行（hopdeck = daemon+GTK，hopdeck-console = GPUI）；根 workspace `members`/`workspace.dependencies` 路径全部改写，新增 `[workspace.package]`（version/edition/license/repository）并由各 crate 继承。
- **全量改名（D51②）**：crate/包名、二进制、`com.hopdeck.HopDeck`（desktop/icon/metainfo/libei/tray/gresource/bundle id）、`$XDG_CONFIG_HOME/hopdeck/`、`hopdeck.pem`、`hopdeck-{socket,video,gui}.sock`、`HOPDECK_*`、`_hopdeck._udp.local.`、systemd `hopdeck.service`、firewalld `hopdeck.xml`、release 资产 `hopdeck-*`、AUR `hopdeck{,-bin,-git}`、Nix 包/模块 `programs.hopdeck`、文档全文。中文名「桌联」补进 README/桌面文件/metainfo（`Name[zh_CN]`/`summary xml:lang`）。
- **架构收口（D51③）**：`AppIdent`/`HostKind`/`RunningApp`/`host_app_ident` 从 hopdeck-ipc 下沉到新文件 `crates/hopdeck-input-event/src/app.rs`（ipc 只 re-export 保持前端 API 不变）；`hopdeck-input-capture` 删除 ipc 依赖，`cargo tree -p hopdeck-input-capture` 实证只剩 input-event。
- **硬断决策（D51④）**：`PROTOCOL_MAGIC = b"HOPDECK\0"`（原 `MOUSEHOP`）；`MACOS_KEEP_AWAKE_EVENT_TAG = 0x484F_5044_4543_4B00`；DOC/CHANGELOG 明确 Mousehop 与 HopDeck 互拒。
- **清理（D51⑤）**：删除 `workflow-drift-check.yml`（上游 jondkinney cohort 专用，改名后必红）；4 个 shared workflow 头注释改为 vendored 说明；修复 release/flatpak/nix/scripts 中所有随目录变化的路径（含 cargo-bundle 的 `../../target` 与 `_up_/_up_` 资源扁平化兼容）。
- **验收证据**：`cargo fmt --all -- --check` 净；`cargo metadata` OK；测试 **input-event 10 / proto 22 / ipc 20 / input-capture 70 / input-emulation 10 / video 32+4 loopback / daemon 115 / console 21** 全绿（daemon 走 `-p hopdeck --no-default-features --features video`，console 走 `--features ui-test-support`）；clippy `-D warnings` 对 daemon(no-gtk)/ipc/proto/input-event/input-capture/input-emulation/video/cli/console 全净；`cargo build -p hopdeck --no-default-features --features video` 产出 `target/debug/hopdeck`。

## 关键决策与死路
- App ID 选 `com.hopdeck.HopDeck`（延续“产品域名”风格）。**若上 Flathub**：需持有 hopdeck.com，或改用 `io.github.re2zero.HopDeck`；只在打包层改，不影响代码。
- 改名即硬断：mDNS 类型与 Hello magic 都变，**不做 Mousehop 兼容层**（D45 的彻底改名要求；旧配置目录/证书也不迁移 → 需重新配对）。
- 旧 `mousehop-ipc` 里 `ClipboardSuppression` 仍留在 ipc（配置/前端语义），只下沉真正跨层的 App 标识类型——保持最小移动面。
- **GTK 编译未验**：本 agent shell 无 nix/direnv（`.envrc=use flake`）且系统无 gtk4/libadwaita pkg-config，`hopdeck-gtk` 无法构建。已做文本核查：gresource 前缀 `/com/hopdeck/HopDeck`、7 个 `#[template(resource=...)]`、`resources_register_include!("hopdeck.gresource")`、tray icon 别名、`HopDeckWindow` 类名、app id 全部对齐；仍需真编译确认。
- Cargo 的 `default-features is ignored ...` 警告是**改名前的既有告警**（workspace dependency 未声明 default-features），本次未扩大处理；clippy 仍净。
- 删除 drift-check 是对上游自动化的一处主动裁剪；其余 release workflow 保留，但其中 GitHub secrets（`HOPDECK_SIGNING_PASSWORD`、`CACHIX_AUTH_TOKEN`、`SITE_DEPLOY_KEY`、`AUR_SSH_KEY`）与 `hopdeck-site` 仓库需公子在 GitHub 侧配置/改名，否则发布链不会全通。

## 下一步精确动作
1. **先验 GTK + 默认特性全构建**（唯一未验面）：在公子 nix 环境跑 `cargo build --workspace` / `cargo clippy --workspace --all-targets --all-features`；重点看 hopdeck-gtk 的 gresource 与模板生成、macOS/Windows 条件编译路径。
2. **双实例 E2E 冒烟**：同一机器 A/B 各带独立 `--config`（或 `XDG_CONFIG_HOME`），验证：mDNS `_hopdeck._udp.local.` 发现、指纹授权、输入双向、IPC 自动重连、video offer（`--features video`）。**注意**：本机主 IP 192.168.101.16；启动 daemon 用简单命令（无 `cd &&`、无复合 pkill），pid 用 `pgrep -fa` 记。
3. **P1.5 断开会话 + 真拒绝 IPC**（新会话专门做）：先设计 daemon 会话击杀语义 `FrontendRequest::KillSession(handle)` / `RejectPeer(fingerprint)` → 踢 `live_incoming`/出站 conn，走 `EmulationEvent::Disconnected` 拆除链（含 video halt）→ UI 详情「断开」按钮 + 认证卡「拒绝」；一并设计 auth 冷却/重试轰炸守卫。
4. **真机批次**（唯一外部依赖）：Wayland portal 弹框 + RestoreToken + M2c 真机复验 + LAN 1080p <150ms，待公子安排。
5. **发布链配置**（如要出包）：GitHub Secret 改名 `HOPDECK_SIGNING_PASSWORD`；cachix cache 建 `hopdeck`；AUR 三个包名 `hopdeck`/`hopdeck-bin`/`hopdeck-git`；Flatpak App ID 域名问题（见上）。

## 风险与阻塞
- GTK 包未编译 → 模板/gresource 若有拼写级问题只会在真实构建暴露（已尽量文本对齐）。
- 改名硬断 → 已配对的 Mousehop 对端升级后需重新授权/配对；旧 `~/.config/mousehop` 不会被读取（可手工迁移 config.toml，但证书换新）。
- release-plz 会尝试发布 `hopdeck*` 到 crates.io；若名字被占用/不想发，需调整 `release-plz.toml` 或关闭该 workflow。
- `.relay` 历史 journal/HANDOFF 仍保留 Mousehop/DeskGo 旧名（历史记录，不重写）；新的计划层已改为桌联 HopDeck。

## 上下文指针
- 提交：`0074922 refactor!: rename product to HopDeck and restructure workspace`；同步基线 `20f5f63`。
- 计划：`.relay/PLAN.md`（D51 已记，当前焦点已刷新）；上篇：HANDOFF-028（D50）。
- 文档：README「Repository layout」、DOC「Workspace layout」「Rename compatibility」、AGENTS「Workspace layout」。
- memory：deskgo-cross-device-collab（会话#22 条待更新）、commit-after-green-tests。
