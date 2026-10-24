# HANDOFF-018（M2c + M3a 双设计蓝本待拍板）
会话：#12  日期：2026-10-24  基线：feat/usbip-emulation@3d0c9f4 + 本会话设计文档提交

## 一句话状态
公子提出反向隧道转发需求后，本会话完成**两份设计蓝本**：M2c 反向隧道转发（`.relay/DESIGN-m2c-tunnel-forward.md`，D32）+ M3a 画面链路（`.relay/DESIGN-m3a-video.md`，D33）——均 v1 待公子拍板，零产品代码改动。

## 已完成（含证据）
- **M2c 反向隧道转发设计 v1 ✅**（D32）：按指示调研 dde-cooperation `tunnel` 分支（26a08dbd 反向隧道 + c7c27cc5 文件传输 + 6 修复），提炼帧协议/服务白名单/信用窗设计 + 三条修复教训（流 id 冲突→奇偶切分、用户态缓冲绕信用窗→tokio 背压、重连清理误伤→代际 id）。推荐独立 TCP 通道（4253/TCP，mutual TLS 同证书+指纹预授权+活跃会话绑定）；配置 = 受限侧 `[[clients.tunnel_forwards]]`（listen+target，**IP:端口与域名，域名由出口侧解析**）+ 出口侧 `[tunnel].allow` 白名单（IP/CIDR/`*.`通配域名，默认拒绝）。mousehop-proto 零变更；新依赖仅 tokio-rustls。实现前置 = 修 D31①（反向启动竞速）。
- **M3a 画面链路设计 v1 ✅**（D33）：ely surface.rs 实测（draw 按设备像素尺寸产 BGRA 单帧 + revision 驱动重绘）⇒ v1 解码=软解+NV12→BGRA（硬解留 M3b）；编码=OpenH264 软编基线（HD 6450 无 VCE，真机验收必走软编）+ VAAPI/NVENC/MFT 硬编增量；传输=包 `[seq|flags|ts|len|payload≤1200B]` + PLI/可选 XOR parity（D5 留白定案，延迟预算 <70ms）；`CAP_VIDEO=1<<3` + VideoOffer（D6 先例）；新 crate `mousehop-video` + feature `video`；测试=dummy 彩条源全链路。
- PLAN：M2c 里程碑插入 + 当前焦点两行重写（M2c/M3a 均待拍板）+ D32/D33 追加；findings.md 登记（relay.py note ×2）。
- 本会话零产品代码改动；提交 3d0c9f4（M2c 设计）+ 本提交（M3a 设计 + 交接）。

## 关键决策与事实
- **D32 要点**：参考实现底线「隧道永不接受对端指定 host/port」= 出口白名单强制；方案 T（独立 TCP）vs R（骑 UDP 会话自研 mini-QUIC）vs Q（quinn）——R 仅端口白名单场景不可替代且 M3a 独立通道同样受制，项目实际已放弃该约束；与 D8③/D13 正交。
- **D33 要点**：RenderSurface CPU 路径决定软解 v1；HW 编码探测即用否则回落软编；reverse 单向拓扑 v1 无画面（拍板项）；Xvfb 无 portal ⇒ 自动化测试必须走 dummy video source（同 dummy capture 先例）。
- dde-cooperation 现状参考：`tunnel` 分支 9 条自有提交（2 feature + 7 fix）；另有 `feature/subnet-forward` 分支 = nft 透明重定向 + 特权 helper + polkit 路线（M2c 范围外，已记录）。

## 下一步精确动作
1. **等公子拍板**（两份设计各 4 项）：
   - M2c：①传输选型（推荐独立 TCP）②端口 4253/TCP vs 4252 双协议 ③域名白名单按名匹配 v1 ④排期（与 M3a 先后）。
   - M3a：①软编基线+硬编增量、真机 HD 6450 软编验收是否接受 ②PLI+parity（推荐）③X11 XShm 本期否 ④reverse 拓扑 v1 无画面。
2. 拍板后按公子定的顺序实现；**M2c 实现前置 = 修 D31① 反向启动竞速缺陷**（保活 tick 检测「active reverse 无活会话」强制重拨/清槽，连接侧增量）。
3. 勿做：gpui-kit 上报（D29 延后）；真机 Wayland（无测试机）。

## 风险与阻塞
- 双设计均阻塞在公子拍板；M3a 真机验收受限于 HD 6450 无硬编（D33 拍板项①）。
- 上下文提示：HANDOFF-017 的「下一步精确动作」#1（M3a 设计蓝本）与公子临时插入的 M2c 需求均已完成，后续会话勿重做调研。

## 上下文指针
- 设计：`.relay/DESIGN-m2c-tunnel-forward.md`、`.relay/DESIGN-m3a-video.md`
- 计划：`.relay/PLAN.md`（D32/D33）；参考仓库：/home/zero/work/repo/github/dde-cooperation（tunnel 分支）
- ely 语义：/home/zero/zero/up/Ely-GPUI-Components/src/rendering/surface.rs
- 上篇：HANDOFF-017（真机联调收口）；gpui-kit 材料（D29 延后勿动）
