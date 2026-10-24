# HANDOFF-016（M2b 收口 → M3a 设计）
会话：#11（session_c91c31d66c3c9eab4aa1347b851a452f，relay.py 登记；上篇 HANDOFF-015 为 session_e5a2… 的增量十二会话）  日期：2026-10-24  基线：feat/usbip-emulation@2b53d5a + 本会话 relay 文档提交

## 一句话状态
M2b 实现侧**全部收口**：一控多三实例会话级冒烟双相位通过（验收项 4 补齐）+ M2b 技术报告落盘；M2b 剩余三项均为外部依赖（真机硬件 / gpui-kit 上报延后 D29 / console 重连语义），下一里程碑 = M3a 画面链路设计蓝本。

## 已完成（含证据）
- **一控多双被控会话级冒烟 ✅**（/tmp/mh-inc13/，方法与坑全录 PLAN D30）：
  - 拓扑：A 控制端（10.8.15.60:4301，dummy capture）/ B 被控（192.168.56.1:4302，Xvfb :98 X11 emulation）/ C 被控（172.17.0.1:4303，Xvfb :99）；单机三实例用三个本机网段 IP 规避 client 按 IP 匹配歧义；指纹预授权（openssl DER sha256 冒号 hex 写入 B/C `[authorized_fingerprints]`）；IPC 直发 `{"ConnectTo":[ip,port]}` 拨号。
  - 相位一（B 右 C 左）：A 日志 `client (0) connected @ 192.168.56.1:4302` + `client (1) connected @ 172.17.0.1:4303`；C 指针 Lissajous（515→853→510→858）；**s1-a-console.png**：设备 0/1 双在线 + 左右两槽同亮 + 「设备 2 台」。
  - 相位二（换位 B 左 C 右，A 重启 + console 重启）：双会话重建；B 指针动（737→624→748）、C 冻结；**s2-a-console-p2.png** 槽位换位仍双在线。
  - 结论：双会话并存 ✓ 两槽同亮 ✓ 输入流随位置路由、两槽会话均能承载注入 ✓。
- **M2b 技术报告 ✅**：`.relay/journal/M2b-REPORT.md`——十二增量盘点表（提交号+决策）、验收 7 项对照（6✅+1⚠️真机）、冒烟方法、已知限制、下一步建议。
- **PLAN 更新**：M2b 里程碑注记（实现收口、真机欠账不勾选）、当前焦点重写、D29/D30 追加。
- 本会话零产品代码改动（纯验证 + 文档）；工作区改动仅 `.relay/`。

## 关键决策与事实
- **D29（公子拍板）**：gpui-kit issues 上报**延后至全部完成**——后续会话勿再列为待办前置，材料在 `.relay/journal/GPUIKIT-ISSUES.md`。
- **D30 冒烟事实**（防重蹈）：① Direct 模式懒拨号，`activate_on_startup` 只激活不拨号；② dummy Begin 每实例一次、若早于会话建立被消费为拨号触发器并 release——`MOUSEHOP_DUMMY_CAPTURE_DELAY_MS` 必须 > 双会话建立时刻，窗口内先 IPC 建会话；③ CLI 无 connect 动词，IPC socket 直发行 JSON；④ **自戕第三变体**：同一复合命令含括号 kill 模式 + 后续字面路径 = 自杀，kill 与含路径字面量的命令必须完全分条；⑤ A 重启后旧代际 HandoverAck 重发被安全忽略（stale 日志无功能影响）。

## 下一步精确动作
1. **M3a 设计蓝本**（下一里程碑第一步）：按 D2/D5/D7 已定边界起草 `.relay/DESIGN-m3a-video.md`——采集（Linux PipeWire portal / Windows DXGI）、硬编解码选型（VAAPI/NVENC/MFT 调研）、独立 DTLS 通道帧协议（capabilities 位 + 新 tag，D6 先例）、分帧/FEC（D5 留白）、RenderSurface CPU 路径对接（ely surface.rs 语义）；素材：README 画面章节、M1-REPORT、M2b-REPORT 下一步建议。成文后**等公子拍板再实现**（M2b 先例：设计 v1→v2.2 拍板后才动代码）。
2. 若公子在场：三件待决事项可一并问——console 自动重连语义（D27③）、M2b 真机批次安排、M3a 设计评审。
3. 勿做：gpui-kit 上报（D29 延后）、真机项（无硬件）。

## 风险与阻塞
- M1/M2b 的真机欠账不消除则里程碑无法正式勾选（等硬件/环境，公子决定是否降级验收）。
- M3a 工作量大（采集/编码/传输/解码/渲染五段），单会话做不完设计+实现，必须增量推进。
- 冒烟环境固化坑：三实例同 IP 会串（用三网段 IP）、Begin 时序、`MOUSEHOP_LOG_LEVEL`、`--capture-backend dummy` 两侧都要。

## 上下文指针
- 计划：`.relay/PLAN.md`（D14–D30）；上篇：HANDOFF-015（增量十二）；本篇证据：/tmp/mh-inc13/
- M2b 技术报告：`.relay/journal/M2b-REPORT.md`（含十二增量索引）
- 一控多冒烟配方：D30（config 模板在 /tmp/mh-inc13/{a,b,c}/config.toml）
- gpui-kit 上报材料：`.relay/journal/GPUIKIT-ISSUES.md`（D29 延后，勿动）
- 设计稿 v2.2：/home/zero/.zcode/v2/visualizations/3d252e463bc1b58d89f0863df626aae944882a3148442cbafe958cd4e62fbe9a/mousehop-console-v2.html
