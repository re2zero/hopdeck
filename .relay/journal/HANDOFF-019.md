# HANDOFF-019（M2c 实现收口 → M3a 实现）
会话：#13（session_b92ea8c0aba91fd3715209409d7e78e9 续，上篇 HANDOFF-018）  日期：2026-10-24  基线：feat/usbip-emulation@e07e45e

## 一句话状态
公子拍板「两设计全按推荐实施」当日，**M2c 反向隧道转发实现收口**（e07e45e，~2.3k 行）+ **D31① 反向竞速缺陷修复**落地：双实例 E2E 五项全过、cargo test 110 全绿、clippy 净。下一步 = **M3a 画面链路实现**（增量推进，设计 `.relay/DESIGN-m3a-video.md` 已拍板）。

## 已完成（含证据）
- **D34 公子拍板**：M2c/M3a 全按推荐方案实施，问题后续迭代；顺序 = D31① 修复 → M2c → M3a。
- **D31① 修复**：`on_reverse_dial_tick` 检测僵尸反向槽（active_addr 有槽但无活会话）→ `reset_handle` 强制重拨（service.rs）。E2E 实证：A 整杀重启 → B 保活重拨 → 收养 → 隧道自动重建。
- **M2c 实现**（e07e45e）：`mousehop/src/tunnel/` 六文件（frames/rules/tls/channel/exit/forward）+ config `[tunnel]`/`tunnel_forwards`/`tunnel_port` + service 接线 + 防火墙 xml 4253/tcp + README/DOC。
- **E2E 冒烟五项 ✅**（/tmp/mh-m2c/，双实例）：A loopback http.server 经隧道可达（12222→18099）；**4MB 传输 md5 一致**（信用窗流控零丢失）；未授权 target（10.8.15.1:445）白名单拒绝（本地连接被重置）；A 整杀重启后会话+隧道自动重建；会话未建立前隧道连接被拒（门禁生效）。
- **测试**：110 全绿（新增 tunnel 24 项：帧编解码/规则匹配/TLS 指纹握手往返/通道复用与死亡释放/target 解析/config TOML 往返）；clippy 净（仅 D25⑤ main.rs 预存 dead_code）。
- PLAN：D34/D35 落盘，M2c 里程碑注记实现收口。

## 关键决策与事实（D35 要点，防重蹈）
1. **rustls provider 统一陷阱**：tokio-rustls 必须 `default-features=false, features=["ring"]`——默认 features 拉入 aws_lc_rs 与 ring 并存 → rustls 自动探测 panic，连坐 webrtc-dtls 内部隐式 builder，表象=被控端 DTLS 握手静默 5s 超时循环。
2. **出口侧会话门禁查 ClientManager**（`get_client(ip)` + `alive` + `peer_fingerprint` 匹配隧道证书指纹）——控制端的反向会话活在**连接侧**（收养的发送路径），listener 入站表里没有。
3. **D31⑥ 单机冒烟抑制**：A 侧指向 B 的条目配不可达端口，否则 A 自拨号抢收养位、B 真会话被 hello 看门狗杀、振荡不收敛（真机单向拓扑天然抑制）。
4. 会话 Pong 置 alive 前的首拨隧道被门禁拒（~2s race）——B 退避重拨自愈，符合「隧道随会话」设计。
5. v1 限制（README 已注明）：TCP only、forwards 启动快照（**改配置需重启 daemon**）、无 SOCKS/透明拦截、UDP 转发不做。
6. 本机网络已从 10.8.15.x 迁移到 **192.168.101.16/24**（下会话冒烟配方注意）。

## 下一步精确动作
1. **M3a 画面链路实现**（设计 v1 已拍板，`.relay/DESIGN-m3a-video.md` §9 落点）：建议增量顺序——① `mousehop-video` crate 骨架 + dummy 彩条源 + OpenH264 软编/软解 + 传输分帧（seq/flags/ts）环回 E2E（像素断言）；② CAP_VIDEO=1<<3 + VideoOffer/VideoStop tag（proto 追加）+ 独立 UDP+DTLS 通道；③ PipeWire portal 采集（ashpd+pipewire-rs）；④ console RenderSurface 挂接 + 画面开关 IPC；⑤ PLI + 可选 XOR parity。每增量过 fmt/clippy/test + 冒烟。
2. 若公子在场可确认：M3a 增量划分是否照此推进；M2c 真机复验排期（等真机批次）。
3. 勿做：gpui-kit 上报（D29 延后）；真机 Wayland（无测试机）。

## 风险与阻塞
- M3a 工作量大（采集/编码/传输/解码/渲染五段），必须增量推进，单会话做不完全部。
- console 侧（gpui）改动需 Xvfb + `LIBGL_ALWAYS_SOFTWARE=1` 冒烟环境（D25/D31 配方）。
- Xvfb 无 portal ⇒ 自动化测试必须走 dummy video source（同 dummy capture 先例）。

## 上下文指针
- 设计：`.relay/DESIGN-m2c-tunnel-forward.md`、`.relay/DESIGN-m3a-video.md`；计划：`.relay/PLAN.md`（D32–D35）
- M2c 代码：`mousehop/src/tunnel/`（提交 e07e45e）；冒烟环境：/tmp/mh-m2c/（配方在 D35 + HANDOFF-018/019）
- 上篇：HANDOFF-018（双设计成文）；dde-cooperation 参考仓库：/home/zero/work/repo/github/dde-cooperation
