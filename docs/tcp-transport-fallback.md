# TCP/TLS 传输回退：实施计划

状态：提案（Phase 1 待实施）
日期：2026-10-28
适用版本：HopDeck 0.17.1
相关机器：10.8.12.63（UOS 20，AMD Oland）↔ 10.8.15.62（UOS 25，AMD Caicos）

---

## 1. 背景与已验证事实

### 1.1 现场问题

两台机器之间：

- **本机 → v25 的 UDP 通**（UDP 9999/4252 抓包与内核计数确认）；
- **v25 → 本机的 UDP 被网络/域管策略丢弃**（本机 UDP 监听收不到 v25 的包）；
- **TCP 双向通**（v25 → 本机 TCP 9997 成功；本机 → v25 TCP 4252 用临时监听实测成功，双向收发正常）；
- 两端主机防火墙均未阻断：v25 `iptables -S` 全 ACCEPT；本机 ufw/firewalld/nftables 服务 inactive；路由均为各自网关直连，无 VPN 路由；
- v25 上存在 `dde-cooperation-forward` NAT 表，只把去 10.8.12.0/24 的 **TCP** 重定向到 37237，UDP 不经过协同代理。

HopDeck 的输入/控制会话是 **UDP + DTLS**。DTLS 握手要求服务端的回应回到客户端临时源端口；该方向 UDP 被丢弃，因此：

- 客户端 5 秒超时（`Connection timed out`）；
- 服务端握手推进不到证书校验，不会产生 `ConnectionAttempt`，控制台看不到授权请求。

结论：**这是网络层的 UDP 单向限制，不是端口配置、不是应用 bug**。TCP 回退可以解决该场景。

### 1.2 已修复且必须保留的前置改动（未提交）

1. `apps/hopdeck/src/listen.rs`：移除了 `spawn_accept_task` 里每 2 秒取消一次 `listener.accept()` 的 `select!`。
   `webrtc_dtls::listener::DTLSListener::accept()` 把整个 DTLS 握手放在 future 里，取消会丢掉握手状态，导致入站连接静默失败。**不要再引入任何 2 秒取消/超时**。
2. `apps/hopdeck-console/src/main.rs`：检测 `/sys/class/drm/card*/device/driver == radeon` 时自动设置 `LIBGL_ALWAYS_SOFTWARE=1`，解决 v25 Caicos 上 wgpu/r600 花屏。
3. `packaging/debian/build-deb.sh` 的用户改动（依赖行）不要覆盖。

这两处改动已用新 deb 验证；本计划的所有工作都应建立在它们之上。

---

## 2. 目标与非目标

### 目标（Phase 1）

在保持 UDP 优先的前提下，为输入/控制会话增加 **TLS over TCP** 传输：

- UDP 可用时继续走 UDP（延迟最低）；
- UDP 握手在短预算内无响应时，自动回退 TCP；
- 对端同时监听 UDP 与 TCP（同一端口 4252，可配置）；
- 复用现有证书、指纹授权、访问密码和 `ProtoEvent` 编解码；
- 控制台能显示当前传输（Phase 1 可只做状态字段，UI 展示 Phase 2）；
- 不改变现有 UDP 路径的正常行为，不改变线协议帧格式。

### 非目标

- 不做 DTLS over TCP（数据报协议不要架在字节流上）；
- 不新增第三方依赖（`rustls`/`tokio-rustls` 已在 `apps/hopdeck/Cargo.toml`）；
- 不引入 vendored crate；
- 不重写 UI、不改视频传输（视频仍是独立 UDP；UDP 单向网络下视频需要单独的 TCP/中继设计）；
- 不做完全单向网络的中继/会合服务器（Phase 3）；
- 不改 `ProtoEvent` 的 wire 格式/版本号。

---

## 3. 现状架构与代码地图

### 3.1 数据/控制路径

```
input-capture → hopdeck daemon → hopdeck-proto (UDP + DTLS) → hopdeck daemon → input-emulation
frontends (console/CLI) ↔ hopdeck-ipc (Unix socket JSON lines) ↔ daemon
```

### 3.2 出站（本机主动拨号）

- `apps/hopdeck/src/connect.rs`
  - `type ArcConn = Arc<dyn Conn + Send + Sync>`（`webrtc_util::Conn`）
  - `HopDeckConnectionError`：`Bind`/`Dtls`/`Webrtc`/`NotConnected`/`Timeout` 等
  - `connect(addr, cert)`：UDP socket + `DTLSConn::new`，5 秒超时（`DEFAULT_CONNECTION_TIMEOUT`）
  - `connect_any(addrs, preferred, cert)`：多地址竞速（happy-eyeballs 风格）
  - `HopDeckConnection::send`：已有活动连接直接发；否则检查 `connecting` 防重入，按 `should_attempt` 退避，`spawn_local(connect_to_handle(...))`
  - `connect_to_handle`：解析候选地址/锁定/模式 → `connect_any` → 安装 `ConnectionSlot` → `hello_handshake` + `ping_pong` + `receive_loop`
  - `hello_handshake`：`ProtoEvent::hello` 重传，等待对端 Hello 回显
  - `ping_pong` / `receive_loop`：心跳、Pong/延迟、AuthRequired/AuthPassword/AuthResult、Hello 校验、输入事件
  - `record_peer_fingerprint`：当前通过 `DTLSConn` downcast 取对端证书指纹
  - `adopt_inbound`：reverse 模式把拨出的传输交给监听侧
  - `DEFAULT_CONNECTION_TIMEOUT = 5s`；`INITIAL_RETRY_BACKOFF = 1s`、`MAX_RETRY_BACKOFF = 30s`

### 3.3 入站（对端接受连接）

- `apps/hopdeck/src/listen.rs`
  - `HopDeckListener::new(port, cert, authorized_keys, access_password)`：
    - 每个本地 IP 绑定一个 UDP DTLS listener（`try_bind_listener`）
    - `verify_peer_certificate`：授权指纹 → 放行；配置了访问密码 → 放行交给协议层挑战；否则把指纹推入 `connection_attempts` 并返回 `ErrVerifyDataMismatch`
    - `spawn_accept_task`（已修复，不再 2 秒取消）
    - `register_accepted_conn`：当前 downcast `DTLSConn` 取指纹，分配 `ListenerSession`，发 `ListenEvent::Accept`（密码模式下先扣留），启动 `read_loop`
    - `read_loop`：Hello 校验、AuthRequired/AuthPassword/AuthResult、`RECV_IDLE_TIMEOUT=8s`、消息转发
    - `self_reply`：认证挑战直接回复
    - `adopt` / `take_conn` / `reply` / `get_certificate_fingerprint`：当前部分依赖 `DTLSConn` downcast
    - `spawn_supervisor_task`：接口 up/down、端口变更、监听重建
  - `ListenEvent`：`Accept { addr, session, fingerprint }` / `Msg` / `Disconnected` / `Rejected { fingerprint }`

### 3.4 服务/前端

- `apps/hopdeck/src/emulation.rs`
  - `ListenEvent::Accept` → `EmulationEvent::Connected { addr, session, fingerprint }`
  - `ListenEvent::Rejected` → `EmulationEvent::ConnectionAttempt { fingerprint }`（2 秒去重）
- `apps/hopdeck/src/service.rs`
  - 构造 `HopDeckListener`（约 265 行）
  - `ConnectionAttempt` → `FrontendEvent::ConnectionAttempt`
  - `DeviceConnected` / `incoming_conns` / `live_incoming`（按 `SocketAddr`）
- `apps/hopdeck/src/client.rs`：`ClientManager` 的 `set_alive` / `set_active_addr` / `set_peer_fingerprint` 等
- `crates/hopdeck-ipc/src/lib.rs`：`ClientConfig`（309 行附近）、`ClientState`（496 行附近）、`FrontendRequest/Event`
- `apps/hopdeck/src/config.rs`：`TomlClient`（206 行）、`ConfigClient`、`From` 映射（515/573 行）
- `apps/hopdeck-console/src/console/mod.rs`：直连请求（约 750–830 行）、在线判断/重试（402/792 行）
- `apps/hopdeck-console/src/console/detail.rs`：连接模式/状态展示

### 3.5 可复用的 TLS/证书设施

- `apps/hopdeck/src/tunnel/tls.rs`
  - `AcceptAny`（客户端证书 verifier / 服务端证书 verifier）
  - `server_config(cert)`：TLS 1.3，`with_client_cert_verifier(AcceptAny)`，单证书
  - `client_config(cert)`：TLS 1.3，自定义 verifier，带客户端证书
  - `peer_fingerprint(certs)`：叶证书指纹，格式与 `[authorized_fingerprints]` 一致
- `apps/hopdeck/src/crypto.rs`
  - `generate_fingerprint(cert_der)`、`certificate_fingerprint(cert)`、`access_password_proof(...)`
  - `load_or_generate_key_and_cert` / `Certificate`（与 DTLS 共用同一张自签证书）
- `apps/hopdeck/Cargo.toml` 已有 `rustls`、`tokio-rustls`、`rcgen`、`sha2`，无需新增依赖。

---

## 4. 设计总览

新增传输：**TLS 1.3 over TCP**，与 UDP DTLS 并存。

- 端口：默认与 UDP 相同（4252 的 TCP + UDP 可共存）；Phase 3 再做可配置 TCP 端口/443。
- 身份：复用同一张自签证书；rustls 层 `AcceptAny`（自签无法链验证），握手后取叶证书指纹，走现有 `authorized_fingerprints` / 访问密码逻辑。与 DTLS 的信任模型一致。
- 会话层：TCP 字节流用 **长度前缀帧** 适配成 `webrtc_util::Conn`，上层 `hello_handshake` / `ping_pong` / `receive_loop` / `read_loop` / `self_reply` / `reply` 全部复用，不感知传输。
- 回退：`transport = auto`（默认）先 UDP（短预算 2s），纯传输超时才 TCP；认证拒绝（DTLS alert / 未授权指纹 / 密码错误）不回退，直接提示。
- 传输缓存：内存中记住每个 handle 上次成功的传输；后续重连优先使用；Phase 2 再做持久化和定期 UDP 重探。
- 入站：每个本地 IP 同时绑定 UDP 和 TCP；TCP accept 后按连接 spawn TLS 握手任务，避免一个慢客户端阻塞 accept。

---

## 5. 详细设计

### 5.1 帧适配器：`TlsConn`

新增（建议放 `apps/hopdeck/src/tcp_transport.rs` 或 `connect.rs` 邻近模块）：

```rust
struct TlsConn {
    read: tokio::sync::Mutex<ReadHalf<TlsStream<TcpStream>>>,
    write: tokio::sync::Mutex<WriteHalf<TlsStream<TcpStream>>>,
    local: SocketAddr,
    remote: Option<SocketAddr>,
    fingerprint: String,
    transport: TransportKind,
}
```

实现 `webrtc_util::Conn`（注意该 trait 用 `#[async_trait]`，且要求 `Send + Sync`）：

- `send(buf)`：写 4 字节大端长度 + payload，`write_all` + `flush`，返回 `buf.len()`；写锁串行化并发发送，避免帧交错。
- `recv(buf)`：读 4 字节长度，校验 `len <= MAX_CLIPBOARD_SIZE`（`hopdeck-proto` 里是 4096；含帧头可放宽到 `MAX_CLIPBOARD_SIZE + 4`），再 `read_exact` 到 `buf`；返回 n。
  - **EOF/连接关闭必须返回 Err，不能返回 Ok(0)**：`receive_loop`/`read_loop` 对 `n == 0` 是 `continue`，会忙等。
- `close()`：`shutdown` 写半端；幂等。
- `local_addr()` / `remote_addr()`：来自 `TcpStream`。
- `connect` / `recv_from` / `send_to`：面向连接，返回“unsupported”错误（不会被调用）。
- `as_any()`：返回自身，供需要时 downcast 取 `fingerprint`/`transport`。
- 建连后对底层 `TcpStream` 调 `set_nodelay(true)`。

帧头：`u32` 大端；发送端 payload 最大就是现有 `MAX_CLIPBOARD_SIZE`/`MAX_EVENT_SIZE`。接收端对超长帧直接关闭连接（防 DoS）。

### 5.2 TLS 建连与认证

复用 `tunnel::tls`：

- 服务端：`TlsAcceptor::from(Arc::new(server_config(&cert)?))`，`accept(tcp_stream)`。
- 客户端：`TlsConnector::from(Arc::new(client_config(&cert)?))`，`connect(ServerName::try_from("ignored")?, tcp_stream)`。
- TLS 握手后：
  - 服务端：`tls.get_ref().1.peer_certificates()` → `tunnel::tls::peer_fingerprint`；
  - 客户端：同样取服务端叶证书指纹，用于 `ClientState.peer_fingerprint`/UI 信任视图。
- 建议加 ALPN `hopdeck/1`（两端都设置），用于未来区分/版本化；非 HopDeck 客户端在 TLS 阶段失败。
- 认证语义（必须与 DTLS 对齐）：
  - 指纹在 `authorized_keys` 中 → 放行，注册连接；
  - 未授权且配置了 `access_password` → 注册但 **扣留 `ListenEvent::Accept`**，由 `read_loop` 发 `AuthRequired`，密码通过后再发 `Accept`；
  - 未授权且没有密码 → **不注册**，推送 `Rejected { fingerprint }`（等价 DTLS 的 `ErrVerifyDataMismatch` 路径），关闭 TLS；对端控制台看到 `ConnectionAttempt`。
  - 注意：TLS 层 `AcceptAny` 会让未授权对端完成握手；这是可接受的，因为服务端在握手后立即按指纹拒绝，不进入 `read_loop`，不处理任何应用数据。若希望更严格，可在自定义 `ClientCertVerifier` 内检查授权表并返回错误；Phase 1 用“握手后检查”更简单。
- 访问密码：`access_password_proof`、`AuthGate`、速率限制、`AUTH_CHALLENGE_TIMEOUT` 全部复用，不改。

### 5.3 出站拨号与回退

1. 扩展返回类型：

```rust
struct Dialed {
    conn: Arc<dyn Conn + Send + Sync>,
    addr: SocketAddr,
    transport: TransportKind, // Udp | Tcp
    peer_fingerprint: Option<String>,
}
```

`connect()`（UDP/DTLS）改为返回 `Dialed`；新增 `connect_tcp()`：

- `TcpStream::connect(addr)` + `tokio::time::timeout`；
- `set_nodelay(true)`；
- TLS `connect` + 超时；
- 取指纹，构造 `TlsConn`。

2. 传输策略：

```rust
enum TransportMode { Auto, Udp, Tcp } // serde: auto | udp | tcp，默认 Auto
```

- `Auto`：先 UDP；UDP 的所有候选地址都在 `AUTO_UDP_TIMEOUT`（建议 2s）内超时/无连接 → TCP；TCP 同样多地址竞速。
- `Udp`：只走 UDP（保持现状）。
- `Tcp`：只走 TCP。
- 回退条件：**纯传输失败（Timeout / NotConnected）**；如果任何一个地址返回 DTLS alert / 认证错误，认为对端可达但拒绝，**不回退**，把错误/授权提示交给现有流程。
- 传输缓存：`HopDeckConnection` 增加 `Rc<RefCell<HashMap<ClientHandle, TransportKind>>>`；成功后记录；下次 `Auto` 先试缓存传输（TCP 失败再回退 UDP 一次）。Phase 2 再做持久化与定期 UDP 重探。
- `connect_any` 按传输复用：`connect_any_udp` / `connect_any_tcp`；错误分类汇总（是否有 timeout、是否有 alert）。
- 反向模式（`ConnectMode::Reverse`）：`Dialed` 里的 `transport`/`peer_fingerprint` 随 `EmulationRequest::AdoptReverse` 传过去，监听侧 `adopt` 用同一套注册逻辑。

3. `connect_to_handle` 改造点：

- `connect_any` 返回 `Dialed`；
- 移除 `record_peer_fingerprint` 的 `DTLSConn` downcast：直接用 `Dialed.peer_fingerprint` 调 `client_manager.set_peer_fingerprint(...)`；
- 日志/错误信息带 transport；
- 成功后把 transport 写入 `ClientState.transport`（Phase 1 供 UI 显示）；
- `receive_loop`/`ping_pong`/`hello_handshake` 不需要改（`Conn` 抽象已覆盖）。

### 5.4 入站 TCP 监听

在 `HopDeckListener::new` 里，与 UDP 一样按本地 IP 绑定 TCP：

- `TcpListener::bind((ip, port))`；失败只 `warn!`，继续 UDP（TCP 端口被占用时不应让整个 daemon 失败）。
- `TlsAcceptor` 用同一张证书构建一次。
- 每个 TCP listener 一个 accept task：
  - `accept()` 后 `set_nodelay(true)`；
  - **为每个连接 spawn 一个任务**做 TLS 握手 + 指纹检查 + 注册，避免一个慢 TLS 客户端阻塞后续 accept（UDP 的 `DTLSListener::accept` 把握手包在 future 里，当前是串行的；不要为了这个再引入取消逻辑，见坑 7）；
  - TLS 握手设置超时（如 10s）；
  - 指纹检查/密码逻辑见 5.2；
  - 通过后调用重构后的 `register_accepted_conn(...)`。
- `ListenerSlot` 增加 `tcp_accept_task`；supervisor 的端口变更、接口 up/down、reconcile 都要同时增删 UDP/TCP listener。
- `HopDeckListener::terminate` 要 abort TCP task 并关闭已接受连接。

### 5.5 去掉监听侧的 DTLS downcast

这三处是 transport 无关化必须改的：

1. `register_accepted_conn`：
   - 增加参数 `fingerprint: String`、`transport: TransportKind`；
   - DTLS accept task 从 `DTLSConn.connection_state().peer_certificates` 取指纹后传入；
   - TCP accept task 从 TLS 流取指纹后传入；
   - `AcceptedConnection` 增加 `fingerprint`、`transport` 字段。
2. `get_certificate_fingerprint`：直接读 `AcceptedConnection.fingerprint`，不再 downcast。
3. `adopt`（reverse）：增加指纹/transport 参数，或让调用方传入已计算的指纹；`EmulationRequest::AdoptReverse` 增加字段。
4. `record_peer_fingerprint`（出站）：由 `Dialed.peer_fingerprint` 替代 downcast。
5. 日志中的 “dtls client connected/disconnected” 改成 transport 无关措辞。

### 5.6 配置 / IPC / UI

- `crates/hopdeck-ipc/src/lib.rs`
  - `ClientConfig` 增加 `#[serde(default)] pub transport: TransportMode`（`auto|udp|tcp`）。
  - `ClientState` 增加 `#[serde(default)] pub transport: Option<TransportKind>`（仅运行时，显示用）。
  - 新增 `TransportMode` / `TransportKind` 枚举（serde 兼容旧配置）。
- `apps/hopdeck/src/config.rs`
  - `TomlClient` 增加 `transport: Option<TransportMode>`；`From<TomlClient>` / `From<ConfigClient>` 映射；默认 `Auto`。
- `apps/hopdeck/src/client.rs`
  - `ClientManager` 增加 `set_transport(handle, Option<TransportKind>)`（标记 dirty，随 `State` 广播）。
- `apps/hopdeck/src/service.rs`
  - 连接建立/接受时设置 transport；断开/删除时清空。
- `apps/hopdeck-console`（Phase 2）
  - 设备详情显示 “传输：UDP / TCP”；
  - 可选 `FrontendRequest::SetTransport(handle, TransportMode)` + 设置项；Phase 1 只需 `ClientState.transport` 字段和日志。

### 5.7 防火墙与文档

- `apps/hopdeck/src/firewall.rs`：`plan_for_host` 目前只加 `udp dport 4252`；同时加 **TCP** 规则（ufw/firewalld/nft/iptables/Windows；macOS 按现有实现补）。
- `hopdeck firewall` 的 help/文档说明现在同时放行 UDP 和 TCP。
- `README.md` / `DOC.md`：平台支持表、端口说明（UDP+TCP 4252）、受限网络说明。
- 注意域管环境：v25 的 TCP 到 10.8.12.0/24 会被 dde-cooperation 代理重定向到 37237；TLS 仍然端到端，代理只看到密文。TCP 4252 已实测可通。

---

## 6. 分阶段任务清单

### Phase 1（MVP，必须）

1. **帧适配器**
   - [ ] `TlsConn` 实现 `webrtc_util::Conn`（发送/接收/关闭/地址/as_any）；
   - [ ] 长度前缀编解码 + 最大帧保护 + EOF 返回错误（不返回 0）；
   - [ ] `TCP_NODELAY`；
   - [ ] 单元测试：拆包/粘包/超长帧/EOF/并发 send。

2. **TLS 客户端**
   - [ ] `connect_tcp()` 使用 `tunnel::tls::client_config` + `TlsConnector`；
   - [ ] 取服务端指纹，返回 `Dialed { transport: Tcp, peer_fingerprint }`；
   - [ ] 连接/TLS 超时。

3. **出站回退**
   - [ ] `Dialed`/`TransportKind`/`TransportMode` 类型；
   - [ ] `connect()` 改返回 `Dialed`；`connect_any` 按传输拆分；
   - [ ] `Auto`：UDP 2s 预算 → TCP；认证 alert 不回退；
   - [ ] 内存传输缓存；`connect_to_handle` 使用；
   - [ ] `record_peer_fingerprint` 去掉 DTLS downcast；
   - [ ] `ClientState.transport` 设置/清理。

4. **入站 TCP**
   - [ ] 每 IP 绑定 TCP 4252（best-effort，失败不影响 UDP）；
   - [ ] 每连接 spawn TLS 握手任务，握手超时；
   - [ ] 指纹授权/密码模式/拒绝路径（`Rejected` → `ConnectionAttempt`）；
   - [ ] 重构 `register_accepted_conn`/`AcceptedConnection`/`get_certificate_fingerprint`/`adopt`；
   - [ ] supervisor 端口变更/接口变化同时管理 UDP+TCP；
   - [ ] 测试：未授权 TLS 连接产生 `ConnectionAttempt`；授权后 `DeviceConnected`。

5. **配置**
   - [ ] `TomlClient.transport` / `ClientConfig.transport`；
   - [ ] 默认 `auto`；旧配置兼容；
   - [ ] `transport = udp` 时行为与现在完全一致（回归）。

6. **防火墙/文档**
   - [ ] `firewall.rs` 加 TCP；
   - [ ] README/DOC 端口说明。

7. **验证**
   - [ ] `cargo fmt`；
   - [ ] `cargo test -p hopdeck`；
   - [ ] `cargo clippy -p hopdeck --all-targets --all-features`；
   - [ ] console 单独构建/测试；
   - [ ] 构建 deb（见附录）；
   - [ ] 在 10.8.12.63 ↔ 10.8.15.62 上实测：UDP 被挡 → 自动 TCP → v25 弹授权 → 授权后输入正常；UDP 可用时仍走 UDP。

### Phase 2（体验/健壮性）

- [ ] 控制台显示“UDP/TCP”，允许 `transport` 选择（`FrontendRequest::SetTransport`）；
- [ ] 传输选择持久化（per-client `last_transport`）+ 定期后台 UDP 重探；
- [ ] 入站会话按指纹/IP 去重（解决 TCP 临时端口导致同一 peer 多 session 的问题）；
- [ ] TCP 心跳/keepalive、事件合并（鼠标移动）、性能测量；
- [ ] `ConnectionAttempt` 去重与授权提示体验。

### Phase 3（受限网络）

- [ ] 可配置 TCP 端口（如 443）+ ALPN；
- [ ] 中继/会合服务器（双方主动出站 TCP），解决完全单向网络；
- [ ] 视频传输的 TCP/中继方案（独立设计）。

---

## 7. 测试计划

### 单元

- `TlsConn` 帧编解码：一次读半帧、一次读多帧、超长帧、EOF、并发 send 不交错；
- TLS 握手/指纹：复用 `tunnel/tls.rs` 的 `handshake_pins_both_fingerprints`，增加“未授权指纹”用例；
- 回退判定：timeout → TCP；DTLS alert → 不回退；`transport=udp` 不尝试 TCP。

### 集成（daemon 测试）

- 构造 TCP-only listener（UDP 不可用）→ `Auto` 客户端应选 TCP 并完成 Hello/Ping；
- 未授权 TCP 客户端 → 服务端 `Rejected`/`ConnectionAttempt`，客户端重试；
- 配置 `access_password` → TCP 上也走 `AuthRequired`/`AuthPassword`；
- `transport=udp` 时现有测试全部通过（回归）；
- 反向模式 `ConnectMode::Reverse` 下 TCP 的 adopt 路径。

### 手工（现场）

1. 两端安装新 deb，重启；
2. 本机输入 `10.8.15.62` 回车；
3. 预期：约 2–3 秒内回退 TCP；v25 控制台弹出带指纹的授权请求；
4. v25 授权；本机 `alive=true`、transport=TCP；
5. 鼠标/键盘跨屏正常；
6. 在 UDP 可用的网络（同网段）验证仍优先 UDP；
7. 断开/重连验证缓存生效（TCP 直连，不再等 2 秒）。

### 构建命令

```sh
PATH="$HOME/.cargo/bin:$PATH" LIBRARY_PATH="$HOME/.local/lib/hopdeck-build" \
  cargo build --release -p hopdeck
PATH="$HOME/.cargo/bin:$PATH" LIBRARY_PATH="$HOME/.local/lib/hopdeck-build" \
  cargo build --release -p hopdeck-console
PATH="$HOME/.cargo/bin:$PATH" LIBRARY_PATH="$HOME/.local/lib/hopdeck-build" \
  packaging/debian/build-deb.sh --skip-build
```

注意：console 与 daemon 不能放在同一个 cargo 调用（ashpd 的 `async-io`/`tokio` feature 冲突）。

---

## 8. 坑与风险（重点）

1. **不要把 DTLS 架在 TCP 上**。用 TLS 1.3 + 长度前缀帧；上层 `ProtoEvent` 不变。
2. **`Conn::recv` 的 `Ok(0)` 会忙等**：`receive_loop`/`read_loop` 对 0 是 `continue`；TCP EOF 必须返回错误。
3. **`webrtc_util::Conn` 用 `#[async_trait]`，方法 `&self`**：TCP 读写半端用 `tokio::sync::Mutex` 包；`TlsConn` 必须 `Send + Sync`；`send` 用 `write_all`，不可假设一次写完。
4. **不要取消 accept future**：`DTLSListener::accept()` 内含完整握手；2 秒取消是已修复的 bug。TCP 侧每个连接 spawn 独立握手任务，避免慢客户端阻塞。
5. **认证语义要与 DTLS 对齐**：未授权 + 无密码 → `ConnectionAttempt`，不能静默 close；有密码 → 扣留 `Accept`，走 `read_loop` 挑战；授权 → 注册。
6. **去掉 DTLS downcast**：`register_accepted_conn`、`get_certificate_fingerprint`、`adopt`、`record_peer_fingerprint` 都依赖 `DTLSConn`；改成“指纹随连接一起传递/存储”，否则 TCP 会 panic。
7. **TCP 临时端口与 `conns`/`live_incoming` 的 key**：入站 UDP 的 peer 地址是客户端临时端口，TCP 也是临时端口；重连会产生新 key，旧 session 靠 close/idle 清理。Phase 1 依赖“拨号方一次只用一种传输 + 旧连接关闭”；Phase 2 按指纹/IP 去重，避免同一 peer 双 session 重复输入。
8. **TCP 绑定失败不能拖垮 UDP**：端口被占用/无权限时只 warn，继续 UDP；`transport=udp` 用户不应被影响。
9. **端口变更/接口 up/down**：supervisor 要同时增删 UDP/TCP listener；`ListenerSlot` 要同时持有两个 accept task。
10. **错误分类**：只有纯传输超时才回退；DTLS alert/密码错误/对端离线要区分；否则会对未授权对端重复弹两次授权。
11. **控制台重试循环**（3 秒一次 `ConnectTo`）与 daemon 的 `connecting`/退避要配合，避免并行拨号和重复 `ConnectionAttempt`。
12. **传输缓存**：TCP 成功后直接走 TCP，避免每次 2 秒 UDP 超时；但网络变化后要能回到 UDP（Phase 2 定期重探）。
13. **防火墙/域管**：TCP 4252 需放行；受限网络可能只允许 443（Phase 3 可配置端口）；v25 的 dde-cooperation 只代理 TCP，TLS 是端到端加密，代理看不到明文。
14. **安全**：必须 TLS 1.3 + 客户端证书 + 指纹授权 + 访问密码；不能因为 TCP 回退降级为明文/无认证。
15. **协议版本**：TCP/TLS 只是传输；`Hello`/`PROTOCOL_MAGIC`/帧格式不变，避免版本分叉。
16. **帧长上限**：`MAX_CLIPBOARD_SIZE`（4096）+ 头；超长帧直接断开，防 DoS。
17. **跨平台**：Windows/macOS 的 `TcpStream` nodelay、防火墙规则、证书路径；`tunnel/tls.rs` 已是跨平台实现。
18. **别碰无关问题**：Firefox/Multica 黑屏是独立问题；console 的 radeon 自动软件渲染和 listen 的 accept 修复是前置改动，不要回退。
19. **不要 vendor**：`rustls`/`tokio-rustls` 已在依赖树；复用 `tunnel::tls`，不要引入新 crate 或复制大段 TLS 代码。
20. **UDP accept 串行**：`DTLSListener::accept` 把 UDP 握手串行化；当前修复已解决“2 秒取消”bug，但一个慢 UDP 握手仍会阻塞后续 UDP accept。若要做公平性，Phase 2 可改为直接使用底层 UDP listener + 每连接 `DTLSConn::new`，这属于独立重构，Phase 1 不做。

---

## 9. 验收标准（Phase 1）

- [ ] 正常网络（UDP 双向）：行为与当前完全一致，默认仍走 UDP；
- [ ] 本机 → v25（UDP 单向）：自动回退 TCP，约 2–3 秒内完成；
- [ ] v25 控制台出现带指纹的授权请求；授权后 `DeviceConnected`、`alive=true`；
- [ ] 输入/剪贴板/控制事件在 TCP 上正常收发；
- [ ] 访问密码在 TCP 上正常工作；
- [ ] `transport = udp` 可强制关闭回退；`transport = tcp` 可强制 TCP；
- [ ] 单元/集成测试、`cargo fmt`、`cargo clippy` 全绿；
- [ ] deb 可安装，两端重启后无需环境变量；
- [ ] 无新增第三方依赖、无 vendored crate、无 wire 协议改动。

## 10. 回滚

- 配置 `transport = udp`（per-client）即可完全不使用 TCP；
- 代码回滚：TCP listener 为 best-effort，移除 TCP accept task/回退分支即可恢复原状；
- 不影响现有 DTLS/UDP 会话与协议兼容性。

## 11. 开放问题

1. TCP 端口是否始终与 UDP 相同？受限网络（只放 443）是否需要 `tcp_port` 配置（Phase 3）？
2. TCP 入站会话的 `addr` 用临时端口，UI/`incoming` 匹配是否需要改为按指纹/IP（Phase 2）？
3. 是否需要在 TCP 上做应用层心跳之外的 TCP keepalive？
4. 视频是否也要 TCP 回退，还是保持“UDP 被挡则无视频”？（Phase 3 单独设计）
5. `Auto` 的 UDP 预算 2 秒是否合适？是否需要按网络历史自适应？

---

## 附录 A：现场验证记录（2026-10-28）

- 本机 10.8.12.63（UOS 20，AMD Oland）→ v25 10.8.15.62（UOS 25，AMD Caicos）
- UDP：
  - 本机 → v25:9999 / 4252：通；v25 内核 `UdpInDatagrams` 计数增加；
  - v25 → 本机:9998：本机监听未收到（被丢）。
- TCP：
  - v25 → 本机:9997：连接成功并交换数据；
  - 本机 → v25:4252（v25 临时 TCP 监听）：连接成功，双向收发 `hello-from-local-4252` / `tcp-4252-ok`。
- 主机防火墙：v25 `iptables` 全 ACCEPT；本机 ufw/firewalld/nftables inactive；路由正常（各自网关）。
- v25 有 `dde-cooperation-forward` NAT：只把去 10.8.12.0/24 的 TCP 重定向到 37237，UDP 不经过。
- 修复验证：patched daemon 自拨（loopback）：
  - 未授权 TCP 对端 → `ConnectionAttempt`（指纹）；
  - 服务端 `AuthorizeKey` 后重拨 → `dtls client connected` / `DeviceConnected` / 客户端 `alive=true`。
- 同一份二进制在 v25 上不设 `LIBGL_ALWAYS_SOFTWARE`，console 自动检测 `radeon` 并软件渲染，UI 干净。

## 附录 B：关键代码入口速查

- 出站：`apps/hopdeck/src/connect.rs`
  - `connect`（230 行附近）、`connect_any`（281）、`HopDeckConnection::send`（462）、`connect_to_handle`、`hello_handshake`（1130）、`ping_pong`、`receive_loop`、`record_peer_fingerprint`
- 入站：`apps/hopdeck/src/listen.rs`
  - `HopDeckListener::new`（228）、`try_bind_listener`（607）、`spawn_accept_task`（625）、`register_accepted_conn`（679）、`read_loop`（963）、`adopt`（414）、`get_certificate_fingerprint`（505）、`spawn_supervisor_task`（750）
- TLS：`apps/hopdeck/src/tunnel/tls.rs`
  - `server_config`（122）、`client_config`（130）、`peer_fingerprint`（141）
- 证书/密码：`apps/hopdeck/src/crypto.rs`
- 协议：`crates/hopdeck-proto/src/lib.rs`（`ProtoEvent` 235、`MAX_CLIPBOARD_SIZE` 40、`PROTOCOL_MAGIC` 78）
- IPC：`crates/hopdeck-ipc/src/lib.rs`（`ClientConfig` 309、`ClientState` 496）
- 配置：`apps/hopdeck/src/config.rs`（`TomlClient` 206、`From` 515/573）
- 防火墙：`apps/hopdeck/src/firewall.rs`（`plan_for_host` 111 起）
- 控制台直连：`apps/hopdeck-console/src/console/mod.rs`（约 750–830）

## 附录 C：给新会话的接力提示词

见仓库外交付说明；核心约束：只做 Phase 1，不引入新依赖/vendor，不改 wire 协议，不重新引入 2 秒 accept 取消，保留 `listen.rs` 与 `console/main.rs` 的既有修复。
