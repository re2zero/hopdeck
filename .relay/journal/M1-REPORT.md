# M1 技术报告：USB/IP 注入后端 PoC

日期：2026-10-23 · 分支 `feat/usbip-emulation` · commit `1ff7c99`

## 实际使用的版本
| 依赖 | 版本 | 说明 |
|---|---|---|
| usbip (jiegec) | 0.9.0 (2026-10-01, MIT) | 硬依赖 rusb（需系统 libusb-1.0-dev，本机 1.0.28 已具备）与 nusb；不可 feature 关闭——纯软件路径仍会编译链接 libusb |
| tokio | 1.51.1（workspace 既有） | crate 要求 ^1.22，兼容 |
| rust 工具链 | 钉子 1.95.0 未安装且 static.rust-lang.org 离线超时 → 本机用 `RUSTUP_TOOLCHAIN=1.96.1` 构建（见 PLAN D9） | CI 恢复网络后应回钉子验证 |
| libusb-1.0-dev | 1.0.28 | 构建期需要 |

## 交付物
- `input-emulation/src/usbip/`（mod/hid/keymap ~700 行含测试）：复合虚拟 HID 设备（boot 键盘接口 0 + 5 键鼠标接口 1，单次 attach），快照式 handler（按键长按由 OS 复现、位移按轮询排空且逐报限幅、余量保留）。
- 插入点：`Backend::Usbip`（枚举/Display/with_backend，**刻意不进自动探测序列**——需接收端管理员配置，绝不静默启用）、`UsbipEmulationCreationError`、config serde 名 `usbip`、mousehop feature `usbip_emulation`（已入 default）。
- `scripts/usbip-receiver-setup.sh`（attach/detach/status，含发行版安装指引与失败排查）。
- README：仿真后端矩阵加 usbip 列 + 使用说明；本报告。

## 验证矩阵
| 验证项 | 结果 |
|---|---|
| 单元测试（键码映射/快照/位移排空/描述符） | 21/21 通过 |
| 集成测试：TCP 上手写 OP_DEVLIST/OP_IMPORT 帧与活服务器交换 | 2/2 通过 |
| 真实二进制端到端：`mousehop --emulation-backend usbip test-emulation` + 外部探针 | DEVLIST 返回 busid=1-1 双 HID 接口；IMPORT 成功 |
| **真机内核级注入（vhci attach 后打字/移鼠）** | **未验证**：本机无 vhci_hcd 模块、usbip 工具、sudo 需交互。验收标准中 Wayland/X11 接收端两条待用户硬件 |

## 已知限制
1. 相对位移方案：无 `MotionAbsolute` 语义，进入对端时光标不落边（默认 `warp_cursor` no-op，靠宿主侧相对流驱动）；若验收发现漂移问题，下一迭代加绝对指针（digitizer 型）接口。
2. 键盘 LED（SET_REPORT）不响应；媒体键（consumer page）不在 boot 描述符内，映射表直接丢弃。
3. 滚轮符号约定：`AxisDiscrete120` 按提交侧语义（正=向下）翻转为 HID（正=向上），AC Pan 不翻转——真机验证时如方向反了只需改 `consume` 中一处符号。
4. usbip crate 绑定失败会 panic（`server()` 内部 expect），`new()` 已做 3240..=3250 端口预探测，但探测与绑定间存在极小竞态窗口。
5. UDP 上无鉴权面：loopback-only 绑定 127.0.0.1，不暴露网络；勿改绑定地址除非加访问控制。

## 与验收标准的差距
- 「QEMU 或真机验证注入」「关闭 libei portal 仍生效」「X11 交叉验证」：未执行（环境限制）。协议层与设备枚举已验证；内核路径（vhci-hcd → evdev → 合成器）是 usbip crate 上游 CI 已覆盖的部分（其 QEMU e2e 测试引导真内核 attach 模拟设备）。
- 建议用户在有 vhci_hcd 的接收端上执行：`sudo scripts/usbip-receiver-setup.sh attach`，随后 `mousehop --emulation-backend usbip test-emulation --keyboard --mouse` 打字验证。

## 下一步建议
- M2a（连接层反向外呼/端口转发）按 PLAN「当前焦点」执行。
- 真机验证反馈后微调滚轮/位移符号与绝对指针取舍。
- 可向上游提 PR：rusb/nusb 改 optional（host 共享设备功能独立成 feature），消除纯软件路径的 libusb 构建依赖。
