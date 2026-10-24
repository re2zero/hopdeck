# DESIGN-m2c-tunnel-forward · 反向隧道转发设计蓝本（v1，待拍板）

日期：2026-10-24（会话 #12）　状态：**设计 v1，等公子拍板后动代码**（M2b 先例：设计拍板才实现）
需求来源：公子 2026-10-24 ——「需要支持单向网络隧道反向转发；转发的配置需要支持 IP 端和域名，类似有的 VPN 里的路由规则」
参考实现：dde-cooperation `tunnel` 分支（26a08dbd 反向隧道 + c7c27cc5 文件传输 + 6 条修复），本机路径 `/home/zero/work/repo/github/dde-cooperation`

## 1. 场景与方向定义

单向网络拓扑（M2a Reverse 已覆盖会话建立）：被控端 B 可外呼、不可被呼入；控制端 A 可达性更好。
本需求 = B 上的**普通 TCP 应用**要访问「只有 A 能到达」的目标：

- A 上的 loopback-only 服务（sshd/HTTP 等，刻意不暴露到网卡）；
- A 所在网络的内网段（NAS、管理口等，B 侧路由不可达）；
- 仅 A 侧能解析的域名（split-horizon DNS / mDNS / hosts）。

方向命名（全文以此为准，防混淆）：

| 流 | 方向 | 说明 |
|---|---|---|
| 会话拨号 | B → A | M2a `ConnectMode::Reverse`，UDP+DTLS |
| 键鼠输入 | A → B | 骑在上行会话上（M2a-ii 收养） |
| **隧道转发** | **B → A** | B 本地监听 → 隧道 → A 侧拨真实目标 |

「反向」指隧道载荷方向与键鼠输入方向相反；与会话拨号同向（B 外呼）。与 D13「输入单向限制」（B 控制 A 需另一条反向拨号会话）**无关**，是正交的两件事。

## 2. 传输选型（拍板项 1）

| 方案 | 做法 | 代价 | 结论 |
|---|---|---|---|
| **T：独立 TCP 隧道通道** | B→A 拨一条 TCP（默认 4253/TCP），帧多路复用其上 | 新增一个入站端口（firewall xml + 文档）；TCP 免费给可靠/有序/拥塞控制，只需移植 dde-cooperation 的帧+信用窗设计（tokio 版约 600–800 行） | **推荐** |
| R：骑现有 UDP+DTLS 会话 | 自研可靠有序流层（seq/ack/重传/窗口 ≈ mini-QUIC 数据面） | 约 1000+ 行高风险代码；唯一优势是零新端口 | 备选：仅「出口按端口白名单放行」场景不可替代，该场景罕见，且 M3a 独立视频通道（D5）同样无法在其下工作——项目实际已放弃该约束 |
| Q：QUIC（quinn）独立通道 | 流/可靠/流控全白送，复用同证书 | 引入第二个 TLS 栈与握手管理 | 记录：若 M3a 视频通道上 QUIC 可再统一，本期不做 |

选 T 的其余理由：与输入 UDP socket 完全隔离（D5 同理由：大流量不得竞争输入）；dde-cooperation 参考实现即「TCP 上的帧复用」，概念可 1:1 移植；鉴权直接复用现有证书与授权表（见 §5）。

## 3. 帧协议（独立 TCP 通道的应用层，mousehop-proto 零变更）

帧头 9 字节，**大端**（对齐 mousehop-proto 的 `to_be_bytes` 惯例）：

```
[u8 tag][u32 stream_id][u32 payload_len][payload]
```

| tag | 名称 | payload | 语义 |
|---|---|---|---|
| 1 | OPEN | JSON `{"target":"10.0.0.5","port":22,"nonce":"..."}` | B 请求开流；A 校验白名单后拨目标 |
| 2 | OPEN_ACK | JSON `{"ok":true,"nonce":"..."}` 或 `{"ok":false,"nonce":"...","reason":"..."}` | 对 nonce 应答；`ok:false` 时 B 对本地 TCP 回 RST |
| 3 | DATA | 原始字节（≤16KB/块） | 流数据 |
| 4 | ACK | 4 字节累计已消费量 | 归还信用窗 |
| 5 | CLOSE | 空 | 半关闭：本侧不再发送；两侧均 CLOSE 或任一侧出错即毁流。TCP FIN ↔ CLOSE，RST/错误 ↔ 立即毁流 |

限制：帧 ≤64KB；并发流/通道 ≤64；每对端转发监听 ≤16；target ≤256 字节。
流 id：**B（转发侧）奇数、A（出口侧）偶数**，各自从 1/2 递增——确定性命名空间切分，杜绝 dde-cooperation 28091a7b 的双侧计数器共享冲突。

## 4. 流控与背压

- 每流初始信用 256KB（dde-cooperation 同值），DATA 块 ≤16KB；接收端在**本地 TCP socket 写入完成后**回 ACK 归还信用；信用归零发送端即停读本地 socket。
- tokio 语义下「信用为零不读」即天然背压，不存在 QAbstractSocket 式用户态无限缓冲（2c43cd97 教训在 tokio 下自动消解）；仍以有界队列显式兜底 per-stream pending。
- 目标拨接超时 5s，失败即 `OPEN_ACK{ok:false}`，**无重试循环**（v1；dde-cooperation 的 4×300ms 重试是为他们本地服务的竞速，此处不需要）。

## 5. 安全模型

1. **出口白名单强制**（参考实现底线：「隧道永不接受对端指定 host/port」）：A 只拨匹配 allow 规则的目标，默认拒绝，从不盲拨。
2. **通道鉴权**：mutual TLS（rustls——已在依赖树，升直接依赖），双方复用现有 rcgen 自签证书；A 侧要求对端指纹 ∈ `authorized_fingerprints` 且**存在一条活跃会话**（指纹+源 IP 对上）——隧道随会话生命周期：会话断 → 通道关 → 全流毁。
3. B 侧本地监听只绑 `127.0.0.1`（listen 字段仍开放成完整 addr 以便进阶用法，但默认值 loopback）；A 侧 `[tunnel].listen` 显式配置。
4. 拒绝 reason 不回显内部细节（白名单内容、解析失败原因分类即可）。

## 6. 路由规则配置（拍板项 3：域名语义）

受限侧（B，per-client）——**转发规则表**（对应「VPN 路由规则」的规则本体）：

```toml
[[clients]]
hostname = "office-controller"
ips = ["10.8.15.60"]
connect_mode = "reverse"

[[clients.tunnel_forwards]]
listen = "127.0.0.1:2222"
target = "127.0.0.1:22"        # A 的 loopback sshd——B 直连永远到不了

[[clients.tunnel_forwards]]
listen = "127.0.0.1:18080"
target = "nas.local:5000"      # 域名：由 A 侧解析（split-horizon / mDNS / hosts）
```

出口侧（A，全局）——**allow 白名单**（对应 VPN 的分流策略，默认拒绝）：

```toml
[tunnel]
listen = "0.0.0.0:4253"
allow = [
  "127.0.0.1",           # host，任意端口
  "10.0.0.0/8:22",       # CIDR + 单端口
  "*.corp.local:443",    # 通配域名 + 端口
]
```

匹配语义 v1：按 OPEN 携带的**原文名**匹配（先匹配后解析，解析只发生在 A 侧——这是域名支持的核心价值）；解析结果不二次复核 IP（DNS rebinding 风险记录在案，A 的 DNS 视为可信；如需复核，v2 加「解析后 CIDR 复验」）。规则项：host 形态 = 精确 IP / CIDR / 精确域名 / `*.` 通配域名；端口 = 单端口或省略（省略 = 该 host 全端口）。listen 冲突或重复 → 配置加载报错。首条匹配生效，按配置顺序。

## 7. 生命周期与重连

- **B 侧**：`tunnel_forwards` 非空的 client，会话建立后懒建隧道通道（首个监听被连接时或激活时）；通道断开按指数退避重拨（复用 connect.rs 反向保活的 backoff 模式）；会话死 → 通道立即关、所有监听停止转发，会话恢复 → 自动重建。
- **A 侧**：per-指纹单活跃通道；新通道握手成功 → 旧通道优雅关闭。吸收 054fca3f 教训：**通道代际 id 单调，清理只作用于本代际**——旧通道的断连清理不得误伤新通道。
- **前置依赖**：D31① 真机缺陷（反向启动竞速不自恢复、保活死槽永不重拨）应在 M2c 实现前修掉——隧道与会话共用「B 外呼 + 保活」机制，竞速缺陷同样会卡死隧道通道的首拨。

## 8. 代码落点与影响面

- `mousehop/src/tunnel/` 新模块：`frames.rs`（编解码）、`rules.rs`(匹配器)、`exit.rs`（A 侧 listener+白名单+拨目标）、`forward.rs`（B 侧本地监听+OPEN）。
- `mousehop/src/config.rs`：`TomlClient.tunnel_forwards`、`Config.tunnel`、`ConfigClient` 新字段（含 TOML 往返持久化，遵循「显式 opt-in 才落盘」惯例）。
- 新直接依赖：`tokio-rustls`/`rustls`；证书复用 crypto.rs 的 rcgen PEM（转 DER 供 rustls）。
- `firewall/mousehop.xml` 增加 4253/TCP；README「Connection Modes」补「反向隧道转发」节；DOC.md 补协议说明。
- **mousehop-proto 零变更**（独立通道应用层协议，不进输入协议；capabilities 不动）。CLI/console UI 本期不做（可选后续增量：cli show 透出转发状态、console 开关）。

## 9. 验收标准

1. 单向拓扑下（本机双实例与真机均验）：B 经隧道访问 A 的 **loopback-only** sshd 成功（22 只绑 127.0.0.1，B 侧无直达路由）。
2. 域名目标：target 为仅 A 侧 hosts/mDNS 有的名字 → 成功（证明解析发生在 A 侧）。
3. 白名单：未列入 allow 的 target → `OPEN_ACK{ok:false}`，B 本地连接被拒且 reason 可读。
4. 流控有界：>10× 信用窗的数据量（dd + cmp 校验零丢失）传输，两端内存占用有界。
5. 自恢复：杀通道/网络闪断自动重拨恢复转发；会话断开转发即停、恢复即续。
6. 隔离性：隧道满载传输时键鼠共享延迟对比基线无显著退化。
7. `cargo test` / `cargo clippy --workspace --all-targets --all-features` 全绿；frames/rules/流控/id 分配有单元测试。

## 10. 范围外（记录不做）

透明子网拦截（nft REDIRECT / TUN 设备——dde-cooperation `subnet-forward` 分支路线，需特权 helper + polkit，桌面系统集成另立里程碑）；SOCKS 动态转发；UDP 转发；多级跳板；console/CLI UI。

## 11. 待拍板项（公子）

1. 传输选型：T 独立 TCP 通道（**推荐**）/ R 骑会话自研可靠层 / Q QUIC。
2. 端口：4253/TCP 独立端口（**推荐**，防火墙 xml 加一行）vs 4252 TCP+UDP 双协议复用。
3. 域名白名单：v1 按原文名匹配（**推荐**）vs 解析后 IP 复核（v2 再加）。
4. 排期：M2c 与 M3a 设计蓝本的先后（本设计独立于 M3a，不阻塞其成文）。
