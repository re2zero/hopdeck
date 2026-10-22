# HANDOFF-002（会话 #2 → 会话 #N+1）
会话：session_749de8ba88987545b88bb35b0b825e8e  日期：2026-10-23  基线：feat/usbip-emulation@1ff7c99（.relay/ 未提交）

## 一句话状态
M1（USB/IP 注入后端）代码完成并已提交，测试/冒烟全绿；真机内核验证待用户硬件；下一棒按 PLAN 做 M2a（连接层反向外呼/端口转发功能），**不得开工 M2b UI**（用户要求先看设计稿确认）。

## 已完成（含证据）
- commit `1ff7c99` feat(input-emulation): add USB/IP HID injection backend
  - `input-emulation/src/usbip/{mod,hid,keymap}.rs`：复合虚拟 HID（键盘+鼠标，busid `1-1`），快照式 handler，loopback server 3240..=3250 端口探测。
  - 接线：`Backend::Usbip`（不进自动探测）、`UsbipEmulationCreationError`、`EmulationBackend::usbip`、mousehop feature `usbip_emulation`（default）。
  - `scripts/usbip-receiver-setup.sh`（attach/detach/status）+ README 后端矩阵更新。
- 验证（本机实测）：`cargo test -p mousehop-input-emulation --features usbip` → 23/23；`cargo test -p mousehop --no-default-features --features usbip_emulation` → 80/80；真实二进制冒烟：daemon 导出 → 外部探针 DEVLIST（1 设备、双 HID 接口）+ IMPORT 成功。
- clippy（usbip feature，--all-targets）无本改动警告；fmt 已过。
- M1 技术报告：`.relay/journal/M1-REPORT.md`（版本表/限制/验收差距）。

## 关键决策与死路
- 全部决策在 `.relay/PLAN.md`「决策与死路」D1–D9。注意 D7（GPUI 线 = gpui-pre 0.3.9，ely+gpui-kit）、D8（连接方式三态；内置中继默认不做）、D9（RUSTUP_TOOLCHAIN=1.96.1 偏离）。
- 死路：控制端建在 bite-gpui 上（改 D7）；ely 有 Draggable 原语（interaction 模块只有 resizable/rotatable/roving/selection，拖拽需自研）。

## 下一步精确动作
执行 `.relay/PLAN.md`「当前焦点」的 M2a 清单（1–6 步），从读 `mousehop/src/connect.rs`/`listen.rs` 确认拨号方向现状开始。**M2a 完成后停下等用户确认 M2b（UI），并继续排自动接力。**

## 风险与阻塞
- 真机 vhci 验证未做（无模块/sudo）——用户硬件到位后按 M1-REPORT 的命令清单验证。
- 反向外呼的实现形态取决于 connect.rs 现状（第 1 步调查决定），可能只需少量改动（对端拨入的会话复用）也可能需要新增拨号发起路径。
- 1.95.0 钉子工具链缺失（D9）。

## 上下文指针
- 计划：`.relay/PLAN.md`；M1 报告：`.relay/journal/M1-REPORT.md`；上篇：HANDOFF-001
- 设计稿（M2b 素材，待确认）：`/home/zero/.zcode/v2/visualizations/97edd8bc06635e4af0dc6710e96b262848aa423f5b02ef52c79aa28b460977e9/mousehop-console-design.html`
- 本地库：`/home/zero/zero/up/Ely-GPUI-Components`（compat/kit 验证 gpui-kit 同窗）
- /tmp/usbip-crate（usbip 0.9 源码克隆，调查用，可删）
