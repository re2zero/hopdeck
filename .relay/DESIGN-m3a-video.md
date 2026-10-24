# DESIGN-m3a-video · 画面链路设计蓝本（v1，待拍板）

日期：2026-10-24（会话 #12）　状态：**设计 v1，等公子拍板后动代码**（M2b 先例：设计拍板才实现）
边界继承：D2/D5/D7（独立 UDP 通道、gpui-pre 0.3.9 + gpui-kit/ely、RenderSurface 先行）＋ HANDOFF-016/017 下一步精确动作。

## 1. 目标与验收（PLAN M3a 原文）

采集（PipeWire portal / DXGI）→ 硬编 → 独立通道 → 硬解 → RenderSurface 全链路可用；LAN 1080p 端到端 <150 ms；画面关闭后键鼠共享延迟与 CPU 占用对比 M2 基线无显著退化。
**现实约束（D31②）**：真机验收机 Radeon HD 6450 是 TeraScale2 老 GPU——**无 VCE 硬编能力**，真机验收必然走软编路径（见 §5 拍板项①）。

## 2. 与 M3b 的分界（决定本设计形态）

ely `RenderSurface`（`Ely-GPUI-Components/src/rendering/surface.rs`，148 行实测）：`draw` 闭包按元素**设备像素尺寸**产 BGRA 单帧 `RenderImage`（进 atlas），`revision` 变化才重绘——**CPU 像素路径，非零拷贝**。推论：
- 接收端解码产物 = CPU 内存 BGRA ⇒ **v1 硬解无收益**（解码后仍要读回 CPU）；v1 解码 = 软件解码 + NV12→BGRA 转换。硬解+DMA-BUF 导入是 M3b 的事（PLAN 原文即如此拆分）。
- 硬编保留在发送端（编码位流无需读回，VAAPI/NVENC/MFT 收益真实）。

## 3. 总体链路

```
被控端 B                                    控制端 A
采集 PipeWire portal / DXGI (BGRx/BGRA)
  → BGRA→NV12 (CPU SIMD 转换)
  → 编码 H.264 baseline（硬编增量 / 软编兜底）
  → 分帧 [seq|flags|ts|payload≤1200B]
  → 独立 UDP+DTLS 通道（D5）──────────▶ 重组 → 缺口检测/parity 恢复
                                          → 软解 H.264 → NV12→BGRA
                                          → RenderSurface（revision 驱动）
```

## 4. 采集段

- **Linux = PipeWire portal**（M3a 验收原文；覆盖 GNOME/KDE/wlroots）：`ashpd`（xdg-desktop-portal ScreenCast，RestoreToken 持久化授权避免每次弹确认）+ `pipewire-rs`（stream 拉帧，BGRx shm 起步，DmaBuf 后续）；产物 BGRA 帧流。**注意**：本机 X11 capture 仍是 stub（D·事实 E2E），M3a 不依赖它——portal 路径不经过 input-capture 的 X11 后端。
- **Windows = DXGI Desktop Duplication**（`windows-rs`，同会话免确认），BGRA8。
- **dummy 视频源**（测试先例同 dummy capture）：彩条+帧计数 pattern，无 portal 环境全链路测试靠它（Xvfb 无 portal）。
- X11 XShm 降级采集：v1 不做（拍板项③）。

## 5. 编解码段

- **抽象**：`Encoder`/`Decoder` trait（input: BGRA 帧流 / output: H.264 NAL），后端按平台与硬件探测选择，日志声明（AGENTS 惯例）。
- **编码 H.264 baseline**：无 B 帧、无 lookahead、紧凑码流——最低延迟；全平台兼容（VAAPI/NVENC/MFT 解码器与软解都有）。
- **v1 兜底 = 软编 OpenH264**（`openh264` crate，Cisco 静态库许可干净）：1080p30 约 15–30% 单核，真机 HD 6450（无 VCE）只能走它——**真机验收走软编，需公子确认（拍板项①）**。
- **硬编增量**：Linux VAAPI（libva FFI）、NVIDIA NVENC（nv-codec-headers FFI）、Windows MFT（`windows-rs` Media Foundation）——有探测即用，无则回落软编。
- **解码 v1 = 软解**（同 OpenH264）→ NV12→BGRA 转换后交 RenderSurface；硬解留 M3b（§2 推论）。
- **转换**：BGRA↔NV12 CPU SIMD（capture 侧一次、render 侧一次），记录进 CPU 预算。

## 6. 传输段（D5 独立 UDP+DTLS 通道；D5 留白的分帧/FEC 在此定案）

- **包格式**（大端，对齐 proto 惯例）：`[seq u16][flags u8][ts u32][len u16][payload ≤1200B]`。一帧 H.264 按NAL/分片拆多包（1200B 低于常规 MTU，规避分片黑洞）；`flags` = 帧首/帧末/关键帧/parity。
- **丢包策略（拍板项②）**：推荐 = 接收端 seq 缺口检测 → 回发 **KeyframeRequest**（PLI 语义，控制通道或视频通道回传均可）+ **可选 XOR parity**（N 包 + 1 奇偶包一组，丢 1 包可恢复；LAN 丢包率低，parity 默认关、可配）。不做完整 NACK 重传站（WebRTC 级复杂度，LAN 不值）。
- **节奏**：无重排抖动缓冲（≤1 帧窗口），渲染端 revision 节流天然丢晚到帧；延迟预算：采集 ~10ms + 编码 ~15ms + 网络 <5ms + 解码 ~15ms + 转换/渲染 <20ms ≈ **<70ms**，余量充足满足 <150ms。
- 画面关闭 = 采集/编码任务停（零 CPU 占用），通道可保留待命。

## 7. 通道建立与鉴权

- `CAP_VIDEO = 1<<3`：Hello capabilities 追加位（D6 先例：旧对端解码为 0 自动不启用；17 字节 legacy Hello 语义不变）。
- 新 `ProtoEvent::VideoOffer{port, codec, max_fps, width, height}`（tag 追加）：被控端宣告 video 监听端口与参数，控制端按需拨号。
- video UDP socket：DTLS 同证书（crypto.rs 复用）+ `authorized_fingerprints` 同表校验；Magic 校验同主通道。
- **方向限制**：v1 仅直连拓扑（A 拨 B）；reverse 单向网络下画面链路暂不支持（后续可借 M2c 隧道经验扩展，明示于 README）。

## 8. 渲染对接（console，gpui-pre 0.3.9）

- 设备详情页挂 `RenderSurface::new(id, ratio, draw)`；`draw` 闭包从解码环形缓冲取最新 BGRA 帧、按给定 `Size<DevicePixels>` **缩放/裁剪成恰该尺寸**（surface 断言尺寸必须精确匹配）。
- 帧到达 → bump revision + `cx.notify` → 下一渲染周期重绘；渲染周期慢于帧率时自然丢帧（surface 语义免费送的帧丢弃）。
- 帧缓冲：双缓冲 + 覆盖写（旧帧可丢），解码线程与 UI 线程以 revision/序列号交接，无锁等待。

## 9. 模块落点

- 新 crate **`mousehop-video`**：`capture`（portal/dxgi/dummy 三后端，tight cfg 惯例）/ `codec`（trait + openh264 + 硬编增量）/ `transport`（分帧/parity/重组）/ `source`（dummy pattern）。root `Cargo.toml` 加 feature `video`；workspace 成员注册。
- daemon：VideoOffer 会话处理 + video 通道任务（与输入 select 循环并列，不互阻）。
- console：RenderSurface 挂接 + 画面开关（IPC 新动词，控制通道传）。
- mousehop-proto：`CAP_VIDEO` 位 + `VideoOffer`/`VideoStop` tag（追加式，版本位随能力位机制自动兼容）。
- 依赖新增：ashpd、pipewire-rs、openh264、windows-rs（Windows 分支）；硬编 FFI 增量随后。

## 10. 测试策略

- **全链路自动化（无 portal 环境可跑）**：dummy source（彩条+帧计数）→ 软编 → 本机环回通道（含**丢包注入**）→ 软解 → 像素断言（计数器可读 ⇒ 端到端零错序/零丢失语义成立）。
- parity 恢复与 PLI 关键帧重请求单测（注入丢包表）。
- 延迟基准：ts 打点贯穿五段，CI 记录端到端分布。
- 真机手动：portal 授权弹框 + RestoreToken 复用、真实桌面延迟观感（HD 6450 软编路径）。

## 11. 范围外（记录不做）

M3b 全部（零拷贝渲染进程/DMA-BUF/硬解导入）；X11 XShm 采集（拍板项③可入）；多路画面同传；AV1/硬件多码流；画面+键鼠的画质自适应码率（v1 固定码率可配）。

## 12. 待拍板项（公子）

1. **编解码策略**：软编 OpenH264 基线（全平台兜底）+ 硬编 VAAPI/NVENC/MFT 增量；真机 HD 6450 验收走软编是否接受（或另配 GPU 验收机）。
2. **丢包策略**：PLI 关键帧重请求 + 可选 XOR parity（推荐）vs 完整 NACK 重传 vs 纯裸流容忍花屏。
3. **X11 XShm 采集**：本期补（X11 环境免 portal 弹框）vs 留后续。
4. **方向限制确认**：reverse 单向网络拓扑 v1 无画面（后续扩展），是否接受。
