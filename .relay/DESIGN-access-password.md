# 访问密码（Access Password）小设计稿 v1

状态：**待公子确认**（D15 遗留需求，确认后实现）。日期：2026-10-23。

## 目标与场景

D15 认证两法：「对方确认」（已实现）+「访问密码」。密码场景：被控端无人值守 / 不想逐台点允许 / 对方重装后指纹变化——知道密码的设备可自助连入。

## 语义（默认取值待确认）

1. **密码是会话准入，不落指纹**：密码正确放行**本次会话**；不写入 `authorized_fingerprints`（持久授权仍走「对方确认」）。下次连接再验密码。备选：首验后自动落指纹免密（弱化密码价值，不推荐）。
2. **优先级**：指纹已授权 → 直接放行（密码不参与）；未授权且被控端启用密码 → 密码校验（不弹确认框）；未启用 → 弹确认框（现状不变）。
3. **失败**：密码错误 → 拒绝并断开，限速独立于 ConnectionAttempt（建议连错 3 次冷却 30s）。

## 协议设计（mousehop-proto 最小增量）

**核心事实**（listen.rs:165-186）：未授权对端在 **DTLS 层**被拒——`verify_peer_certificate` 回调查 authorized 表，未命中返回 `ErrVerifyDataMismatch`，握手当场失败、线上无任何回包。因此密码门禁**必须上移到协议层**：密码模式下放行 DTLS 握手（证书仍校验为合法证书并记录指纹），在 mousehop 协议上做挑战-应答后才准入。

- Hello **不改 wire shape**：追加 capability 位 `CAP_ACCESS_PASSWORD = 1 << 2`（老端忽略未知位，forward-compat 内建；serialization 未变 → 无版本 bump）。
- 新增事件（EventType 追加 tag，仅双向协商出该 capability 后收发）：
  - `AuthRequired { salt: [u8;16], nonce: [u8;16] }`（接收端→发起端）
  - `AuthPassword { proof: [u8;32] }`（发起端→接收端）
  - `AuthResult { ok: bool }`（接收端→发起端，ok=false 后断开）
- 口令学：落盘只存 `(salt, verifier)`，`verifier = SHA-256(salt || password)`，**明文密码不落盘**；proof = `SHA-256(salt || nonce || password)`，接收端现算比对。nonce 每次会话随机 → 线上 proof 不重放。

## 改动点

- **daemon（接收端）**：`verify_peer_certificate` 密码模式下记录指纹但放行；emulation.rs 握手路径在 Hello 前挂起等 `AuthPassword`（复用现有握手 watchdog 超时）；通过后本会话视同授权（会话级 allowlist，不动 authorized 表），后续 `DeviceConnected` 正常广播；console 信任区块自然不显示（指纹 ∉ authorized）。
- **daemon（发起端）**：connect.rs Hello 循环处理 `AuthRequired` → 新 `FrontendEvent::PasswordRequired { addr }`；console 弹密码框 → 新 `FrontendRequest::SubmitAccessPassword { addr, password }`（内存暂存，重连时作答；不落盘）。
- **console**：发起端密码对话框（连接中状态若收到 PasswordRequired → 「对方需要访问密码」）；接收端设置项在设置浮层增量里做（v1 先 config.toml 手填）。
- **proto**：3 个事件 + 1 个 capability 位 + 编解码单测。

## 风险

- 密码模式下未授权对端可完成 DTLS 握手（协议层才拒）——门禁仍在输入路径之前，但暴露面从「握手失败」变为「协议层拒绝」；限速必须做。
- 发起端 UX 多一轮往返（连接中 → 要密码 → 作答 → 上线）；后续可用 mDNS TXT 广播 `access=1`+salt 提前弹框（增强项，不进 v1）。

## 请公子拍板

1. 密码通过后**不**落指纹（默认）还是首验自动落？
2. nonce 挑战版（上文，推荐）还是简化 verifier 直比版？
3. 密码错误限速取「3 次冷却 30s」？
