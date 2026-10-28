# HANDOFF-033（会话 #26 → 会话 #27）
会话：#26  日期：2026-10-28  基线：`feat/usbip-emulation@c9b5fae`（本会话未提交代码）

## 一句话状态
按公子要求完成「同步分支 → 编译 deb → 本机/VM 安装 → 联调」：分支从 `8bb1fab` 快进到 `c9b5fae`（10 提交，核心是 TCP/TLS 回退传输），重建并安装 deb（VM 系统级、本机用户级），两端双向 UDP 与强制 TCP 会话均验证通过（0.1–0.3ms）。本机因 sudo 密码未知未做系统级安装，DDE 菜单仍指向旧包；另发现出/入站双会话并存时 `ClientState.transport` 单值互相覆盖，需要拍板。

## 已完成（含证据）
- **分支同步**：`git fetch` 后快进 10 提交到 `c9b5fae`（`docs(connect): describe the reprobe cache seeding accurately`）。新增能力：TLS over TCP 回退、同一端口 4252 同时监听 DTLS/TCP、Auto 先 UDP（5s 预算）再 TCP、`transport`/`last_transport` 配置落盘、console 详情可切 auto/udp/tcp、UDP 600s 后台复探、入站会话按 peer fingerprint 去重、radeon 自动软件 GL、deb 依赖 `libgcc1 | libgcc-s1` + `libxcb1`。
- **构建**：`packaging/debian/build-deb.sh` 两段 release 构建通过（daemon 1m40s，console 2m06s），产物 `target/deb/hopdeck_0.17.1_amd64.deb`，12,402,492 字节，sha256 `c37d5f8caa8a3b260bbde63d75a9b875dbe1d54185b2030cacc9d9bd25fb7521`。
- **VM 安装（uos@192.168.101.74，密码 1）**：scp + `sudo dpkg -i` 成功；`/usr/bin/hopdeck --version` 显示 `commit_hash:c9b5fae9`；`/usr/share/applications/hopdeck.desktop` 与 `hicolor/scalable/apps/hopdeck.svg` 就位，旧 `com.hopdeck.HopDeck.*` 已清；`dpkg-query` 版本 0.17.1。
- **本机安装（用户级）**：`dpkg-deb -x` 到 `~/.local/opt/hopdeck`，软链 `~/.local/bin/{hopdeck,hopdeck-console}`；`which hopdeck` 命中新路径，`hopdeck --version` 为 `c9b5fae`。**系统级 dpkg 未执行**：本机 sudo 密码不是「1」，非交互验证失败；docker 可运行但容器内 `chroot` 被宿主 sandbox 拦截（报 `/sysroot/ostree` 重挂失败）。
- **联调（两端均 `--capture-backend dummy --emulation-backend x11`）**：
  - 正向：本机 `ConnectTo 192.168.101.74:4252` → `plan: [Udp, Tcp]`、`connected over udp`；本机状态 `alive=true incoming=true live=udp`，VM 入站 `incoming=true`。
  - 强制 TCP：本机 `SetTransport(0,"tcp")` 后重拨 → `plan: [Tcp]`、`connected over tcp`，VM 日志 `tcp client connected, ip: 192.168.101.16:48276`；状态 `transport="tcp"`、`last_transport="tcp"`。
  - 反向：VM `ConnectTo 192.168.101.16:4252` → UDP 入站，双方 `alive=true incoming=true`。
  - 延迟：本机侧 0.2–0.3ms，VM 侧 0.1–0.2ms。
  - 恢复 auto：先 `SetTransport("udp")` 再 `"auto"` 重拨，最终两端 `pref=auto last=udp alive=true incoming=true live=udp`；配置 `last_transport = "udp"`。
  - 截图：本机 `/tmp/hopdeck-local-shot.png`（列表显示 tt-v25 在线）、VM `/tmp/hopdeck-vm-shot.png`（等同拷贝在 VM 同路径）。
  - 重启韧性：VM 端关闭 console（launcher 联动停 daemon）后以系统包重启，重新拨号即恢复双向在线。
- **验收**：release 构建与 deb 打包通过；本会话无代码改动，未跑测试（沿用上一会话 daemon 116/console 22/ipc 21 的绿基线）。

## 关键决策与环境事实
- **本机无 root 的安装边界（新增）**：`zero` 在 sudo 组但密码未知；docker 虽可用，chroot 被宿主 sandbox 拦。用户级安装可让终端优先用新版，但 DDE 会话进程 PATH 只有 `/usr/local/bin:/usr/bin:...`，系统 entry 的 `Exec=hopdeck` 仍解析到 `/usr/bin/hopdeck`（旧包 `98e46af`）；`~/.local/state/hopdeck/desktop-install-done` one-shot marker 会阻止首次启动写用户级 entry。**系统安装需公子本人执行**：`sudo apt install --reinstall ./target/deb/hopdeck_0.17.1_amd64.deb`（needrestart 提示答 No 或 `NEEDRESTART_SUSPEND=1`）。
- **传输状态单值合并（待拍板）**：出站与入站是两条独立会话，`alive`/`incoming` 可同时为 true，但 `ClientState.transport` 只有一个字段。实测强制 TCP 出站成功后，VM 反向拨入 UDP 把状态覆盖为 `udp`，控制台显示「UDP」与「仅 TCP」偏好矛盾。可选方向：拆分 `outbound_transport`/`inbound_transport`，或 UI 优先展示出站传输；未改代码。
- **dummy 采集日志噪声**：dummy 后端固定 1ms 间隔发事件，VM 侧触发 `Ack-gated input has no pending handover; releasing capture` + `release-warp` 循环，约 2000 行/s，8 分钟 96MB（本机不复现）。VM 已用 `HOPDECK_LOG_LEVEL=error` 重启，日志归零；底层循环仍在。X11 真实采集仍是代码缺口，本次只能验证会话/传输，不能验证真实键鼠事件跨机。
- **两端现场**：两端实例仍在运行（本机用户级 c9b5fae，VM 系统包 c9b5fae），均已互授指纹；本机配置客户端 VM `192.168.101.74`（position right），VM 配置客户端本机 `192.168.101.16`（position left）。备份：本机 `~/.config/hopdeck/config.toml.pre-joint-20261028`，VM `~/hopdeck-deb-backup/config.toml.pre-joint-20261028`。
- **IPC 直连方法（复用）**：Unix socket `/run/user/1000/hopdeck-socket.sock`，行分隔 JSON；`{"ConnectTo":["<ip>",4252]}`、`{"SetTransport":[0,"tcp"]}`；`Enumerate`/`State` 事件每条含 `[handle, ClientConfig, ClientState]`。

## 下一步精确动作
1. **拍板 transport 单值语义**（新）：是否拆分出/入站传输或 UI 优先展示出站；涉及 `hopdeck-ipc::ClientState`、daemon `set_transport`/`refresh_incoming_flags`、console `transport_label`。
2. **P1.5 断开会话 + 真拒绝 IPC**（PLAN 唯一剩余 P1 项）：`KillSession`/`RejectPeer` 的会话击杀语义、踢 `live_incoming`/出站 conn、走 Disconnected 拆除链（含 video `stop_sending`）；详情「断开」按钮 + 认证卡「拒绝」；auth 冷却守卫。
3. **本机系统安装**：等公子执行上面那条 sudo 命令后，确认 `/usr/bin/hopdeck` 为 c9b5fae、系统 entry 换成 `hopdeck.desktop`、旧 `com.hopdeck.HopDeck.desktop` 消失。
4. **可选：dummy 采集告警限流**（`hopdeck::capture` ack-gated warn 每事件一条），或文档写明测试必须设 `HOPDECK_LOG_LEVEL=error`；X11 真实采集实现仍是更大缺口。
5. **CI 两段构建核对**：`default-members` 对 `cargo build --release` 的生效范围、release workflow tar/zip/dmg 产物。
6. **真机默认启动**：Wayland portal picker 仍未验；老 GPU 花屏可用 `LIBGL_ALWAYS_SOFTWARE=1` 排障。
7. **沿用**：USB-IP 注入复验、键盘真实采集源。

## 上下文指针
- 分支/提交：`feat/usbip-emulation` = `origin/feat/usbip-emulation` = `c9b5fae`；工作区仅 `.relay/state.json` 为本会话簿记修改。
- 产物：`target/deb/hopdeck_0.17.1_amd64.deb`（sha256 见上）；VM 安装包副本 `/tmp/hopdeck_0.17.1_amd64.deb`。
- 截图：`/tmp/hopdeck-local-shot.png`、`/tmp/hopdeck-vm-shot.png`。
- 计划：`.relay/PLAN.md`（会话#26 / D59）；上篇 HANDOFF-032（本地 deb 打包 + VM 安装启动）。
- 测试配方：daemon `cargo test --workspace --exclude hopdeck-console --all-features`；console `cargo test -p hopdeck-console --all-features`；打包 `packaging/debian/build-deb.sh` / `--skip-build`。
