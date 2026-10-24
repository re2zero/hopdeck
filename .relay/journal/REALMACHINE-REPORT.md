# 真机联调报告：测试机 uzero@10.8.15.62（UOS Desktop 25 专业版）

日期：2026-10-24（会话 #11 续）  分支：`feat/usbip-emulation@b30fe07`（零产品代码改动）
环境：本机 10.8.15.60（deepin rolling，控制端 60，dummy capture）↔ 测试机 10.8.15.62（UOS 25 / 内核 6.6.0-amd64-desktop / X11 会话 / Radeon HD 6450，被控端 62）
部署：debug 二进制直拷（两侧 glibc 同为 2.38，免编译）至 `62:~/mh-rt/`；`/usr` 只读故工具装 `~/mh-rt/bin/`。

## 验证矩阵与结论

| # | 项目 | 结论 | 证据 |
|---|---|---|---|
| 1 | 真实两机直连会话 + 真实桌面注入 | ✅ | A `client (0) connected @ 10.8.15.62:4402`；B `accepted inbound DTLS session 1 from 10.8.15.60:60890`；`DISPLAY=:0 xdotool` 指针 841,583→524,632→653,540（Lissajous） |
| 2 | 真实两机反向外呼（M2a 真机欠账） | ✅ | B `client (0) connected @ 10.8.15.60:4401` + A `client (0) adopting inbound transport from 10.8.15.62:60161`；62 真实桌面指针 941,283→1004,171→730,177 |
| 3 | USB/IP 真机链路（M1 真机欠账·鼠标） | ✅ | `switching emulation backend → usbip` + `virtual HID device exported on 127.0.0.1:3240 (busid 1-1)`；`usbip attach` 后 dmesg 枚举 `USB HID v1.11 Keyboard`（input34/hidraw0）+ `Mouse`（input35/hidraw1）；反向隧道 dummy 流经 USB/IP 注入，62 真实指针 546,90→988,0 |
| 4 | USB/IP 真机链路（M1 真机欠账·键盘） | ✅ | 临时探针（`InputEmulation` usbip 后端直驱）打 "hello world"：vhci 虚拟键盘 evdev 节点捕获完整 `HDN HUP EDN EUP LDN LUP LDN LUP ODN OUP SPACEDN SPACEUP WDN WUP ODN OUP RDN RUP LDN LUP DDN DUP` |
| 5 | X11↔USB-IP 会话存活双向热切（M2b 验收项 5 真机版） | ✅ | ① usbip+attach 指针 546,90→988,0 → ② IPC `{"SetEmulationBackend":"x11"}` 热切后指针 1019,256→853,183→782,72 → ③ 切回 usbip+re-attach 指针 706,252→1490,425；全程隧道不拆 |
| 6 | console 真机观感（Xvfb 图标欠账） | ✅（带缓解） | 真实桌面渲染完美：SVG 图标（窗口控制 −□×、状态点、徽标边框）、设备 0「在线」+「反向外呼」徽标、右槽高亮、状态栏全绿（console-final.png） |
| 7 | 「关闭 libei portal 后注入仍生效」 | ✅ | 62 无 Wayland/portal，纯 X11 桌面，USB/IP 注入全程生效 |

截图：本机 `/tmp/mh-rt/console-real.png`（花屏原始帧）/`console-sw.png`（软件 GL 修复）/`console-final.png`（在线终态）；62 `~/mh-rt/`（daemon 日志全量）。

## 环境配方（复现要点）

1. **vhci 内核支持**：UOS 25 内核 6.6 三模块在树（`vhci-hcd.ko`/`usbip-host.ko`/`usbip-core.ko`，CONFIG_USBIP_VHCI_HCD=m 15 端口）。注意 `modinfo vhci_hcd`（下划线）查不到——文件名是连字符 `vhci-hcd.ko`；`modprobe vhci-hcd` 可用。
2. **usbip 用户态工具**：UOS 源无包、`/usr` 只读 → kernel.org 6.6 tarball 抽 `tools/usb/usbip/` 子树，**免 autotools 直编**：`gcc src/{usbip,usbip_attach,usbip_detach,usbip_list,usbip_bind,usbip_unbind,usbip_port,usbip_network,utils}.c libsrc/*.c -Ilibsrc -Isrc -pthread $(pkg-config --cflags --libs libudev) -DUSBIP_VERSION=0x0111 -DPACKAGE_STRING='"usbip 6.6"' -DPACKAGE_VERSION='"6.6"'`。
3. **双向预授权**：反向场景两侧都要预授权对端指纹——控制端 A 的 DTLS verify 对未授权对端直接 fatal alert（B 端日志表象：`failed to connect: Alert is Fatal or Close Notify`）。指纹 = `openssl x509 -outform DER | sha256sum` 冒号 hex 小写。
4. **Attach**：`sudo usbip attach -r 127.0.0.1 -b 1-1`（daemon 导出后按日志提示执行）；explorer 关闭（daemon 退出）自动分离。
5. **console 真机渲染**：`LIBGL_ALWAYS_SOFTWARE=1` 必须——见下「缺陷 3」。

## 缺陷与新知（后续跟进）

1. **反向启动竞速不自恢复**（连接侧，建议修复）：被控端拨号早于控制端 listener 就绪（≈2s 窗口）时首拨 fatal alert 失败；此后反向保活 tick 继续对旧连接槽心跳（UDP 写死连接静默成功），**永不重拨**，隧道直到手动重启才建立。两次独立复现（05:46、06:01）。修复方向：保活 tick 检测「active reverse client 无活会话」时强制重拨/清槽。
2. **互标 reverse 的角色混沌**（设计观察）：双方条目互标 reverse 且双向可达时，控制端也会执行反向拨号（connect_to_handle 的 reverse 分支无条件把自家拨号连接交给自家 listener = 控制端自角色反转），对端把它收养进 capture 后因无 Pong ~2s 自清理（D13 冗余连接），系统收敛到「被控端拨入、控制端收养」的正确单隧道。设计拓扑（单侧可达/NAT）不受影响；M2a-REPORT 的「控制端配不可达端口」实为抑制此混沌的手段。记录备查，暂不改动。
3. **GPUI 真机渲染花屏**（老 GPU 兼容）：Radeon HD 6450（TeraScale2，无 Vulkan）真实桌面 GPUI GL 路径稳定花屏（黑块+撕裂帧）；同一二进制在 Xvfb（软件 GL）完美。`LIBGL_ALWAYS_SOFTWARE=1` 完全修复。上游相关（gpui/gpui-kit 对老 GL 的兼容或 DRI3 路径）；产品侧建议：console 启动检测花屏/提供 `MOUSEHOP_CONSOLE_SOFTWARE_GL` 环境变量包装。**这同时解释了 Xvfb 图标之谜**：Xvfb 无 GPU，图标渲染正常路径与真机软件 GL 一致——图标资产本身无问题。
4. **glibc 2.38 平移**：debug 二进制（319MB daemon + 685MB console）跨机直拷即用；release 构建可大幅瘦身部署。
5. 自戕教训第四次：ssh 远端命令里 `pkill -f` 与同一命令串中的字面路径共存 = 远端 shell 自杀——**kill 必须独立成条**，与 D28⑦/D30⑥ 一脉相承。

## 里程碑状态影响

- **M1**：X11 真机 vhci 注入（键盘+鼠标）+ 无 portal 环境生效 ✅；唯一剩余 = 真机 Wayland（GNOME 45+/KDE 6.1+）交叉验证——当前无 Wayland 测试机，保持不勾选，注记进度。
- **M2a**：真实两机反向场景 ✅（真机欠账清）。
- **M2b**：验收项 5 真机版（X11↔USB-IP 热切后共享正常）✅——字面的 libei↔USB-IP 需 Wayland 真机，与 M1 同源欠账；Xvfb 图标观感欠账清 ✅。
