# 发现与证据（append-only）

<!-- 磁盘工作记忆：每 2 次探索/检索、每解决一个错误，就追加一条，不要攒到会话末尾。
     一条一行，带证据：文件:行号 / 命令输出摘要 / 结论。本文件只追加不删改。
     写 HANDOFF 时从这里摘录关键项进「关键决策与死路」。
     命令：relay.py note "内容" -->

- 2026-10-24 20:32 [M3a·#12] M2c 反向隧道转发设计蓝本 v1 成文（.relay/DESIGN-m2c-tunnel-forward.md）：参考 dde-cooperation tunnel 分支（26a08dbd+c7c27cc5+6修复），推荐独立 TCP 通道（4253/TCP+mutual TLS）+ 帧复用/信用窗 + 受限侧 tunnel_forwards（IP/域名目标）/出口侧 allow 白名单；4 拍板项待公子；PLAN 加 M2c 里程碑与 D32
