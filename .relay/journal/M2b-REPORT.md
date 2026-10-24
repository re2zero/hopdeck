# M2b 技术报告：GPUI 控制端 UI（十二个增量 + 一控多会话级冒烟）

日期：2026-10-24（会话 #11/#12）  分支：`feat/usbip-emulation`  基线：`2b53d5a`
设计稿 v2.2：/home/zero/.zcode/v2/visualizations/3d252e463bc1b58d89f0863df626aae944882a3148442cbafe958cd4e62fbe9a/mousehop-console-v2.html

## 交付范围

控制端 crate `mousehop-console`（gpui-pre 0.3.9 + gpui-kit 0.8.0 单一入口，D17），十二个增量全部落地：

| # | 提交 | 内容 | 关键决策 |
|---|---|---|---|
| 1 | `1b6ac56` | GPUI 控制台骨架接通 daemon IPC | gpui-kit 单一入口；ely 暂缓（无 crates.io 发布） |
| 2 | `969a882` | 搜索框 IP 回车直连、设备详情操作、列表实时徽标 | D18 ConnectTo 语义（复用/新建+激活+即时拨号） |
| 3 | `edca231` | 授权流（对话框+备注）、信任区、直连重试 | D19（重试以 alive 为终止条件；指纹回写） |
| 4 | `264c55c` | 连接徽标、信任回填、前端同步 tick | D20（capture 泵死时状态冻结的根修） |
| 5 | `a9ac60e` | 设置浮层 + per-peer 共享选项三卡 | D22（剪贴板/滚动/灵敏度 keyed by 指纹） |
| 6 | `f479b91` | 交互式布局画布（拖拽放置） | D23（gpui 原生 drag 三件套；监听器内直推通知） |
| 7 | `ff7c218` | 调试日志栏（journal + facade 双写入方） | D24（默认 info 档防刷屏；400ms 轮询 gen） |
| 8 | `73aa0a4` | 详情页 per-device 剪贴板发送开关 | 发送侧 `clipboard_send` |
| 9 | `24775f1` | 设置浮层「后端/USB-IP」区 + 无头 UI 集成测试三枚 | D25（test-support 解析雷→三件套直连） |
| 10 | `6850683` | 布局移出=停用 + 编辑表单键映射字段 | D26①②③（Activate(handle,false)，非 Position::None） |
| 11 | `76d70ac` | 仿真后端 live 热切 | D26④/D27（SwitchBackend 三处接入；USB/IP 导出实证） |
| 12 | `6f775e7`+`afce68d` | 会话访问密码挑战（proto+daemon+console 全链） | D28（门禁上移协议层，emulation.rs 零改动） |

协议层影响仅增量十二：`CAP_ACCESS_PASSWORD`(bit2) + 事件 tag 26–28（定长 33/33/2，`MAX_EVENT_SIZE` 26→33），wire 追加 tag、老二进制被 capability 门禁快速拒绝，无版本 bump。

## 验收对照（PLAN M2b 七项）

| # | 验收项 | 结论 | 证据 |
|---|---|---|---|
| 1 | 设备添加与发现三路径（自动/手动/反向外呼） | ✅ | mDNS（daemon 既有）；搜索框 IP 回车→`ConnectTo`（mod.rs）；详情页连接方式 chips→`UpdateConnectMode`（detail.rs） |
| 2 | 拖拽布局 | ✅ | 增量六六槽画布+容量守卫+ghost，冒烟 /tmp/mh-inc7/ s8/s9 |
| 3 | per-device 连接方式 | ✅ | `ConnectMode{Direct,Reverse}`；列表反向外呼徽标 + 详情 chips；转发端口=direct+转发地址（README） |
| 4 | 发起一控多会话 | ✅（2026-10-24 补齐） | 三实例会话级冒烟，见下节 |
| 5 | 接收端 libei↔USB-IP 切换后共享正常 | ⚠️ 真机欠账 | 机制齐：config `emulation_backend` + 增量十一 live 热切（daemon 侧 USB/IP 导出 127.0.0.1:3240 已实证）；vhci 注入验证等真机 |
| 6 | GPUI 前端崩溃不影响 daemon | ✅ | kill -9 console → daemon 存活、UDP 保持监听、新 console 即连恢复（/tmp/mh-inc9/ u1–u3） |
| 7 | USB-IP「需要接收端配置」显式提示与指引 | ✅ | 设置浮层后端区：警告 chip + 三步指引 + 两条 mono 命令（u3-settings.png） |

测试资产：console 16 测试（含 3 枚 UI 集成测试，`--features ui-test-support`）、daemon 84、proto 21；clippy 双配置零告警。

## 一控多双被控会话级冒烟（2026-10-24，/tmp/mh-inc13/）

### 拓扑与方法

单机三实例，**三个本机网段 IP** 规避 client 按 IP 匹配的歧义（`10.8.15.60`→A 自身、`192.168.56.1`→B、`172.17.0.1`→C；docker0 虽 DOWN 但本机 UDP 可达，实测源地址回落 `10.8.15.60`）：

- A：控制端，port 4301，`--capture-backend dummy`，config 预置两条 `[[clients]]`（B=right, C=left, `activate_on_startup=true`）
- B：被控，port 4302，Xvfb :98，X11 emulation（`--no-default-features --features "x11_capture,x11_emulation,usbip_emulation"` 配方）
- C：被控，port 4303，Xvfb :99，X11 emulation
- 预授权：A 先空跑一次生成证书 → `openssl x509 -outform DER | sha256sum` → 冒号 hex 小写（与 `crypto::generate_fingerprint` 同格式）→ 写入 B/C `[authorized_fingerprints]`，免对话框流
- 拨号：IPC socket 直发换行 JSON `{"ConnectTo":["192.168.56.1",4302]}` / `{"ConnectTo":["172.17.0.1",4303]}`（CLI 无 connect 动词；`FrontendRequest` 为行分隔 JSON）

### 时序关键：dummy Begin 的消费语义

`MOUSEHOP_DUMMY_CAPTURE_DELAY_MS` 必须 **大于双会话建立时刻**。Direct 模式是懒拨号（`activate_on_startup` 只激活不拨号）——Begin 若在会话建立前触发，会被 capture.rs 消费为 first-crossing 拨号触发器并 release capture，此后 dummy 不再发 Begin，输入流状态机永不启动。解法：Begin 延迟 12–15s，窗口内经 IPC `ConnectTo` 把两个会话先建起来。

### 相位一（B 右 C 左）

- A daemon 日志：`client (0) connected @ 192.168.56.1:4302` + `client (1) connected @ 172.17.0.1:4303`（双会话并存）
- C 侧注入：`DISPLAY=:99 xdotool getmouselocation` 连续采样 515→853→510→858（Lissajous ±200px 轨迹）——输入流经左槽会话注入
- console 截图 `s1-a-console.png`：设备 0/1 双双「在线」、布局左槽=设备1 右槽=设备0 **两槽同亮**、状态栏「设备 2 台 · 服务已连接 · 采集 开 · 注入 开」

### 相位二（换位 B 左 C 右）

- A config 换 position 后重启（**daemon 重启 console 必须重启**，D27③）→ IPC 重拨 → 双会话重建（B/C 各自日志 `accepted inbound DTLS session`）
- B 侧注入：:98 指针 737→624→748 持续运动；C 侧 :99 指针冻结——**路由随位置切换，非活跃侧静默**
- console 截图 `s2-a-console-p2.png`：槽位换位（左=设备0 右=设备1），仍双双在线

### 结论

验收项 4 会话级补齐：一台控制端同时维持两条被控会话、布局两槽同亮、两个会话都能承载输入流且按屏幕位置互斥路由。

## 已知限制与欠账

1. **真机欠账**（等用户硬件/环境）：M1 vhci 注入打字验证、真实两机反向场景、Xvfb 图标不渲染的真机观感确认（SVG 资产加载无报错，疑 Xvfb 环境限制）。
2. **console 不自动重连** daemon 重启（D27③）：feed 不会重拨 IPC socket，需重启 console——产品语义待公子定。
3. **访问密码 v1**（设计内简化）：明文落 config（README 注记）、接收端无设置 UI（手填 config.toml）、错密码后 console 无专用失败提示（行回离线，重连再弹框）。
4. **一控多环境限制**（本冒烟新知，非产品缺陷）：dummy capture 单边单 Begin——只有位置激活侧（Left 槽）能验证注入；真实多屏路由由真实 capture 后端驱动。A 中途重启后旧会话代际 HandoverAck 重发被安全忽略（27 条 stale 日志，无功能影响）。
5. 冒烟方法坑（已录 PLAN D28/D29）：daemon 日志开关 `MOUSEHOP_LOG_LEVEL`（RUST_LOG 无效）；无 capture 后端时 capture 泵退出（双实例两侧都要 `--capture-backend dummy`）；复合命令里 kill 模式与字面路径共存会自杀（本次第三变体：括号模式 + 同命令内字面 `--config .../a/config.toml`）——**清理与启动必须完全分条**。

## 下一步建议

1. gpui-kit issues 上报：材料就绪（`.relay/journal/GPUIKIT-ISSUES.md`，5 项，优先级 1>2>3>5>4）——2026-10-24 公子拍板**延后至全部完成**（D29），不阻塞任何里程碑。
2. 真机验证批次（vhci 注入、两机反向、图标观感）——等硬件/环境。
3. console 自动重连语义拍板（自动重拨 IPC socket vs 保持手动）。
4. M3a 画面链路（可选里程碑）可启动：PipeWire portal / DXGI 采集 → 硬编 → 独立 DTLS 通道 → RenderSurface（D5/D7 边界已定）。
