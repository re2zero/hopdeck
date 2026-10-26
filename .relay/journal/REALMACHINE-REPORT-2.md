# 真机联调报告 2：UOS 25 虚拟机（Wayland + portal 采集中间链路）

日期：2026-10-26（会话 #23）  分支：`feat/usbip-emulation`
环境：本机 192.168.101.16（deepin rolling，控制端 B）↔ 虚拟机 uos@192.168.101.74（UOS 25 / VirtualBox 8 vCPU / glibc 2.38，被控端 A）
部署：release 二进制直拷（`video,video_portal,x11_capture,x11_emulation,usbip_emulation` 特性集），配置/证书在 `/tmp/hd-rm/` 两侧隔离，端口 A=4374 / B=4373。

## 会话/portal 环境（复现要点）

1. **DDE portal 在此 VM 不可用**：X11 会话下无 `org.freedesktop.impl.portal.ScreenCast`（只有 Wayland 插件实现）；手工拉起 treeland 0.10 Wayland 会话后，DDE portal 后端卡在 `org.deepin.Compositor1.XWaylandName` 重试，最终 "Failed to prepare XWayland session"（VM 的 XWayland 准备缺陷），无法用于采集验证。
2. **实际验证路径 = treeland + 私有 dbus 会话 + xdg-desktop-portal-wlr 0.7**：`XDG_CURRENT_DESKTOP=sway` 让 wlr 插件被选中；`xdg-desktop-portal-wlr` + `xdg-desktop-portal` 前端跑在 `dbus-run-session` 里，daemon 侧 unset `DISPLAY`/`WAYLAND_DISPLAY`（daemon 用不到，且可避免 arboard 在无 X 连接时阻塞单线程 runtime）。
3. **portal 授权与 RestoreToken**：每次 daemon 启动都会走完 ScreenCast 授权并拿到 `restore token`（本次样本 `z41TTe_FDE_LqBdI689_fQ` 等，INFO 日志可见）。v1 只记录不持久化，属既定增量；wlr portal 对单输出自动选择，**没有出现交互式 picker 弹框**——picker 本身（DDE/桌面门户的选屏交互）在此 VM 仍属未验面。
4. **VM 屏幕 = 1600x1200**（非 1920x1080），软件 OpenH264 编码，30fps；`[video]` 里请求的 1920x1080 只是转码前配置，实际协商尺寸以 portal 为准（见缺陷 3 的日志修正）。
5. **VirtualBox guest 时钟速率异常（测延迟前必须处理）**：clocksource=tsc 时 guest 墙钟约 +4.6% 快、systemd-timesyncd 周期性回跳；`kvm-clock` 无改善，切到 **`acpi_pm`** 后 60s 内偏移稳定在 ~20ppm（实测 -58.88 → -58.91 ms）。该切换是运行时操作，重启 VM 后需重做；否则单向延迟公式会得到负值。

## 缺陷 1（已修）：portal shm 缓冲区 PROT_NONE 导致 daemon SIGSEGV

- **现象**：daemon 日志 `portal capture negotiated 1600x1200 BGRx` 后立即段错误，core 落在 `portal.rs` 的 BGRx→BGRA 拷贝（`dst[px*4] = src[px*4]`）。
- **根因**：portal-wlr 0.7 的 spa buffer 只声明 `MAPPABLE`（flags=0x8）未声明 `READABLE`；libpipewire 按其语义把 shm MemFd 映射为 `PROT_NONE`，`spa_data.data` 指向该映射，首字节访问即 `SEGV_ACCERR`（strace 可见 `mmap(..., 7680000, PROT_NONE, MAP_SHARED, fd)` 之后没有 mprotect）。
- **修复**（`crates/hopdeck-video/src/capture/portal.rs`）：stream 连接去掉 `MAP_BUFFERS`；process 回调对 `DataType::MemFd` 用 `fd/mapoffset/maxsize` 自行 `mmap(PROT_READ, MAP_SHARED)` 读一帧、转换后 `munmap`；非 MemFd 路径保留 `data.data()`。`hopdeck-video` 新增 `libc` 依赖（仅 Linux 目标）。
- **验证**：修复后连续 720 帧 `decoded ... lost=0`，落盘帧转 PNG 为 VM 的 UOS 桌面壁纸（1600x1200），画面内容正确。

## 缺陷 2（已修）：输入会话断开误杀 video 监听，画面共享永久失效

- **现象**：任一侧 daemon 重启/会话断开后，A 的 video DTLS 监听 socket 消失；新会话仍收到 offer（端口号是旧的），B 拨号 DTLS 握手 5s 超时，A 侧无任何日志（`ss` 确认无监听）。在真机上这意味着「画面共享看起来开着，实际永远连不上」，直到手动关/开共享或重启 daemon。
- **根因**：`Service` 的入站 `Disconnected` 分支调用 `VideoHost::halt()`——它会取消 accept loop 并关闭监听；而 `Connected` 只重发 `offer()`，没有重建宿主。注释里「下一次 Connected 会重新宣告并重拨」与实现对不上。
- **修复**（`apps/hopdeck/src/video.rs` + `service.rs`）：`Host` 新增 `streams: Rc<RefCell<CancellationToken>>`（halt 的子 token）。断开时调用新的 `stop_sending()`：取消并替换当前发送会话 token，**监听与 portal 采集保持存活**；`halt()` 仍只用于关闭共享/停机。新增单测 `stop_sending_keeps_the_listener_announceable`（offer 仍可用、旧会话 token 已取消、新 token 未取消）。
- **真机验证**：A 保持运行、kill B daemon → A 日志 `sender ... stopped after 630 frames`，`ss` 仍见 `:49081` 监听；重启 B → 新会话拿到**同一端口**的 offer → 拨号成功 → `streaming ... 1600x1200@30fps` → 120 帧 `lost=0`。

## 缺陷 3（已修，日志）：流的尺寸日志说谎

发送端启动日志原本打印 `announced {config.width}x{config.height}`，portal 源实际是 1600x1200 却显示 1920x1080。现改为 portal 源优先取协商几何，日志与画面一致（`streaming ... 1600x1200@30fps`）。

## 延迟验收：LAN 端到端 < 150ms ✅（软件编码）

**方法**：临时探针（提交前已删除）在三个点打墙钟：portal 帧就绪（A）、编码后首片发出（A）、解码完成（B）。两侧时钟偏移用持久 SSH 往返取最小 RTT 样本（RTT 0.12–0.17ms），测量窗口内偏移稳定（-58.88 / -58.91ms 前后括号）。发送 790 帧 / 解码 793 帧 1:1 对齐，`lost=0`。

| 分段 | p50 | p95 | p99 | max |
|---|---|---|---|---|
| 采集→编码后发出（含 latest-wins 排队） | 52 ms | 73 ms | 79 ms | 89 ms |
| 网络（编码后→接收首片，UDP+DTLS） | 2.1 ms | 2.1 ms | 3.1 ms | 30 ms |
| 首片→解码完成（重组+软解） | 10 ms | 13 ms | 18 ms | 26 ms |
| **采集→解码完成（端到端）** | **62 ms** | **85 ms** | **93 ms** | **103 ms** |

结论：p99 93ms、最大 103ms，**低于 150ms 验收线**。口径说明：① 分辨率为 VM 的 1600x1200（1.92MP，1080p 为 2.07MP，约 +8%）；② 端点为「解码完成」，不含 console 上屏绘制（若计入约再加 ≤1 帧 16–33ms，仍有余量）；③ 编码为纯软件 OpenH264。

## 缺陷 4（已修）：反向会话被 Entered 门控误判为僵尸，每 5s 抖动

- **现象**：反向拓扑下输入会话每 5s 重建一次（A 日志 `session 13/14/15… connection closed` + `reverse client 0 keeps a transport … with no live session — resetting so it re-dials`），隧道通道随之反复重连；B 侧每次都要重新收养。
- **根因**：D31④ 的僵尸检测用 `incoming_session(addr)` 判断会话存活，而它读的 `incoming_conn_info` 只在 `Entered`（指针真正划入）时填充；反向收养的会话在指针划入前是健康的，却被每个 tick 当成僵尸重置。
- **修复**（`apps/hopdeck/src/service.rs`）：改用 `live_incoming`（P1.2 引入的完整 `Connected` 会话表，插入/替换/断连维护齐全）作为存活源。
- **真机验证**：修复后 20s 内 `no live session`/断连计数为 0，隧道通道一次建立后不再重连；允许目标取到 marker，拒绝目标仍被 allow 规则拦下。

## M2c 反向隧道真机复验 ✅（含 allow-list）

拓扑：B（本机）为可达侧 exit——`[tunnel] listen = "0.0.0.0:4253"`、`allow = ["127.0.0.1"]`；A（VM）为受限侧——`connect_mode = "reverse"` + 两条 `[[clients.tunnel_forwards]]`（18080→127.0.0.1:8099 允许；18081→192.168.101.16:8099 预期被拒）。B 的 loopback 上跑 `python3 -m http.server 8099` 提供 marker。

| 项 | 结论 | 证据 |
|---|---|---|
| 隧道通道建立 | ✅ | B `tunnel channel established with 192.168.101.74:…`；A `tunnel channel to 192.168.101.16:4253 established`（一次建立，不再重连） |
| 允许目标转发 | ✅ | VM 上 `curl http://127.0.0.1:18080/marker.txt` → `hopdeck-m2c-marker-1793025718`（B 的 loopback 服务） |
| 默认拒绝 + allow-list | ✅ | `curl http://127.0.0.1:18081/…` 返回 000；日志 `target denied by exit allow rules` |
| 反向会话稳定性 | ✅（修复后） | 见缺陷 4；20s 零重置 |

**拓扑注意**：两侧互标 `reverse` 且双向可达时会互拨，各自把对端拨入的连接当成「自己拨号的回声」（日志 `echoes client 0's own reverse dial`），出现约 5s 的收养/重置循环。按 D35③ 的做法，在 exit 侧把对 A 的拨号端口指向不可达端口（本次 9999）即可抑制自拨回声，同时保留 reverse 标记供收养。

## 输入注入真机复验 ✅（指针；X11 后端）

- VM 已切回 X11（lightdm 自动登录 uos，display **:1**，1920x1080；原 treeland 会话的 bind mount 已卸载）。
- **X11 采集后端尚未实现**（运行时报 `X11 input capture is not yet implemented :(`，README 也标注未勾选），因此采集侧用 dummy 源（其合成运动正是 D31 用过的端到端驱动方式），注入侧用真实 **X11 emulation** 后端。
- 结果：VM 上 `DISPLAY=:1 xdotool getmouselocation` 连续采样 `800,600 → 656,502 → 807,563 → 658,596 → 454,450 → 435,634 → 671,488`——指针被远端 dummy 流真实驱动；B 侧日志 `[wp-begin] pos=left` + `adopting inbound transport`；A 侧 `EmulationStatus=Enabled`、`EmulationBackend=X11`、`alive=true`。
- 未覆盖：键盘注入（dummy 源只发指针运动）、USB-IP 后端（本轮未重编 usbip 用户态工具；X11 注入已足够证明会话→注入链路）。

## 其他观察（未改，记录备查）

1. **daemon 不会在无输入事件时自动重连**：B 侧 daemon 在 A 重启后停在 `alive=false`（重连是输入事件驱动的；dummy capture 无事件，故本报告用重启 B 的方式建新会话）。真实桌面有指针/键盘事件，预期可触发；是否要加独立重连定时器留待拍板。
2. **portal 帧无内嵌计数器**：接收端 `counter=65535` 是 `read_pattern_index` 对任意桌面像素的退化读数，仅日志观感问题（feed 的 index 同理）。
3. **headless 会话下 arboard 初始化阻塞**：`Clipboard::new()` 在无 X/Wayland 连接的会话里会同步等 X11 超时，拖住单线程 runtime；测试脚本已用 unset `DISPLAY`/`WAYLAND_DISPLAY` 规避，产品侧在纯 headless 服务场景值得关注。
4. **DDE portal 的 XWaylandName 卡死是 VM 缺陷**，与 HopDeck 代码无关；真机桌面（有完整 XWayland）上的 picker 交互仍待下一轮真机。
5. **X11 采集后端缺口**：`--capture-backend x11` 会启动失败（明确报未实现），X11 桌面目前只能做被控端注入；README 的未勾选项准确，无需改文档。

## 里程碑状态影响

- **M3a 真机批次**：portal 采集真实链路 ✅（wlr 替代 DDE，含 RestoreToken 发放）；LAN 端到端延迟 ✅（p99 93ms）；**交互式 picker 弹框仍未验**（wlr 自动选屏 + DDE portal 在 VM 不可用），保持注记。
- **M2c 反向隧道真机复验**：✅（含 allow-list 拒绝与缺陷 4 修复）。
- **输入注入真机复验**：✅ 指针注入（X11 后端）；键盘/USB-IP 不在本轮范围。
- VM 现状：X11 + 自动登录 uos（display :1），测试 daemon/HTTP 服务已停止，treeland 会话目录 bind mount 已卸载。

