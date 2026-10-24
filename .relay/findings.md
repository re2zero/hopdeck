# 发现与证据（append-only）

<!-- 磁盘工作记忆：每 2 次探索/检索、每解决一个错误，就追加一条，不要攒到会话末尾。
     一条一行，带证据：文件:行号 / 命令输出摘要 / 结论。本文件只追加不删改。
     写 HANDOFF 时从这里摘录关键项进「关键决策与死路」。
     命令：relay.py note "内容" -->

- 2026-10-24 20:32 [M3a·#12] M2c 反向隧道转发设计蓝本 v1 成文（.relay/DESIGN-m2c-tunnel-forward.md）：参考 dde-cooperation tunnel 分支（26a08dbd+c7c27cc5+6修复），推荐独立 TCP 通道（4253/TCP+mutual TLS）+ 帧复用/信用窗 + 受限侧 tunnel_forwards（IP/域名目标）/出口侧 allow 白名单；4 拍板项待公子；PLAN 加 M2c 里程碑与 D32
- 2026-10-24 20:38 [M3a·#12] M3a 画面链路设计蓝本 v1 成文（.relay/DESIGN-m3a-video.md）：RenderSurface CPU 路径⇒v1 软解；OpenH264 软编基线+VAAPI/NVENC/MFT 硬编增量（HD6450 无 VCE 走软编）；D5 留白定案 seq 包+PLI/可选 XOR parity；CAP_VIDEO=1<<3+VideoOffer；新 crate mousehop-video+feature video；4 拍板项待公子；PLAN D33
- 2026-10-24 23:25 [M3a·#13] M2c 反向隧道转发实现收口（e07e45e）：tunnel 模块六文件~2.3k 行，双实例 E2E 五项全过（loopback 转发 4MB md5 一致/白名单拒绝/A 重启自恢复/会话未建前拒绝/流控有界），test 110 全绿 + clippy 净；关键坑= rustls 双 provider 统一 panic（tokio-rustls 须 ring-only）+ 出口门禁须查 ClientManager 连接侧；D31① 保活僵尸槽修复随实现落地
