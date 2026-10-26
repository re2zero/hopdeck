# HANDOFF-026（会话#19 改名头脑风暴 + UI×功能缝隙盘点 → 两项待公子拍板）
会话：#19  日期：2026-10-26  基线：feat/usbip-emulation@dcffe27（纯调查轮，无代码改动）

## 一句话状态
公子提出「项目已重构，改名 + 功能与 UI 衔接逐一完善」；本会话完成 deskgo 撞名查证（否决）、候选名头脑风暴、全仓 UI×功能缝隙实测盘点，整理成四批路线（PLAN D42）——**名字与批次两项均待公子拍板，无自动可推进的代码任务**。

## 已完成（含证据）
- **deskgo 否决**：三重撞名——腾讯 DeskGo（桌面整理工具，目标用户群直接重叠，最致命）、DESKGO!（Softonic 商务协作 v3.0）、Deskgo（Windows 音乐下载器）；另 `-go` 后缀易误读 Golang。阵亡名单：DeskHop（开源硬件 KVM）/Deskflow/DeskLink（Windows 内置功能名+eMAM+Android+desklink.ai）/OneDesk/Unidesk。
- **候选与推荐**：HopDesk（WebSearch 查无明显占用；风险＝与 DeskHop 词序近似）、DeskWeave、中文品牌派 桌联/一桌/屏渡；推荐双名策略（英文标识 + 中文产品名「桌联」主打）。改名 sweep 范围与时机（真机批次收口 + 合 main 后单独 rename 分支）见 D42①。
- **UI×功能缝隙盘点**（Explore very thorough，两份文件:行号级清单：console 全控件×数据来源、daemon 全功能面×用户入口）：主干已实（详情/九宫格/编辑表单/共享三卡/画面开关均真 IPC+落盘）；三处说谎点、P1 断链五项、GTK 独占能力面、暗装清单、落盘核对全部成文（要点已灌入 D42②；完整两清单在本会话上下文与 memory `deskgo-cross-device-collab`「会话#19」条，必要时可重派 Explore 复现——快，30 分钟级）。
- 建议起点＝P0 全部 + P1 前 2 项（说谎点清除 + IPC 自动重连 + 视频共享端 UI），与真机批次并行不阻塞。

## 关键事实（防重蹈）
- 命名结论与阵亡名单见 D42①——**后续会话勿再提议 deskgo/DeskLink/DeskHop 系**。
- daemon 11 种事件（Error/RemoteHostState/IncomingDisconnected/DeviceEntered/ReleaseBind/ReleaseThreshold/MdnsDiscovery/SuppressedAppsUpdated/RunningApps/NoSuchClient 等）console 兜底丢弃（mod.rs:484）；`ListRunningApps` 是死协议面（daemon 已实现、无任何前端调用）。
- 落盘缺口三处：ChangePort（无 config::set_port）、SetVideoView 偏好（仅 daemon 内存）、UpdateEnterHook（不触发保存）。
- DEMO 模式（MOUSEHOP_CONSOLE_DEMO=1）所有操作静默无效（writer None）——验 UI 必须真 daemon。

## 下一步精确动作
- **衔接第一批已实现（同会话续四，D46，未提交）**：P0.1 ChangePort 落盘（config::set_port + PortChanged 臂 save）／P0.2 「拒绝」→「忽略」两处诚实化／P1.1 console IPC 自动重连（feed_tx 唯一 sender 化，修 D27③ 根因）——daemon 114 + console 13 + UI 17 绿、clippy 净、E2E 双实证（端口落盘重启生效 + console 断线自愈）。改动 6 文件：mousehop/{config,service}.rs、mousehop-console/src/{ipc.rs,console/mod.rs,console/detail.rs} + .relay 记账。
- **等公子**：① 过目 D46 改动（说「提交」走 commit-submit，建议 feat(console)+fix(daemon) 分账或合一）；② 真机批次（Wayland portal + RestoreToken + M2c 复验 + LAN 1080p）；③ 下一批拍板：默认 P1.2 视频共享端 UI 先行，其余 P1 池（出站回写 fingerprint / Error toast / 断开+真拒绝 IPC）排队。
- 定名已收口（D45：桌联 + HopDeck）；rename sweep 等真机批次收口 + 合 main 后单独分支。gpui-kit 上报维持 D29 延后。
- 自动接力保持关闭（D40 约定不变，勿重开）。
- 拍板后新会话：P0 各项改动面小（set_port+保存链 / daemon 拒绝语义或改文案「本次忽略」/「来源」记账），单会话可收口一批；P1.1 IPC 重连与 P1.2 SetVideoEnabled 各自独立可并行。
- 原遗留不变：真机批次（Wayland portal 弹框 + RestoreToken + M2c 复验 + LAN 1080p <150ms）、出站会话回写 peer_fingerprint 拍板（已并入 P1）、gpui-kit 上报（D29 延后）。

## 风险与阻塞
- 全部依赖公子拍板；无代码阻塞、无未提交改动（本轮只动 .relay 与 memory）。
- 自动接力保持关闭（D40 约定不变，勿重开）。

## 上下文指针
- 计划：.relay/PLAN.md（D42 + 遗留待公子⑤⑥）
- memory：deskgo-cross-device-collab「会话#19·方向转变」条（含盘点要点与撞名证据链接）
- 上篇：HANDOFF-025
