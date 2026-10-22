# M2a 技术报告：连接层功能（per-device 连接方式 + 反向隧道输入桥接）

日期：2026-10-23  分支：`feat/usbip-emulation`  里程碑提交：`e6d973b`（增量一）、`44de4d0`（增量二 / M2a-ii）

## 交付范围

M2a 验收对照（PLAN 原文）：

| 验收项 | 状态 | 说明 |
|---|---|---|
| ClientConfig 支持连接方式（直连 / 反向外呼 / 转发端口） | ✅ | `ConnectMode{Direct,Reverse}`（serde lowercase + FromStr）；「转发端口」由 direct + 指向转发地址覆盖（UDP 需转发，README 已写明） |
| daemon 拨号方向按配置执行 | ✅ | reverse 激活即拨、5s 保活 tick（无会话=惰性拨号、有会话=心跳）；`UpdateConnectMode` 即时 kick |
| 反向外呼场景两机经单侧发起建立会话 | ✅ | 增量一建立会话（DTLS 握手）；增量二让**控制输入流**真正走该隧道（见下） |
| `mousehop-cli` 可配置与验证 | ✅ | `set-connect-mode <id> <direct|reverse>`，README 用法齐全 |
| mousehop-proto 无变更 | ✅ | 纯既有事件类型（Hello/Ping/Pong/Handover*），capabilities 位未动 |

## M2a-ii：控制输入流桥接（增量二）

### 架构（收养式桥接）

Mousehop 会话角色与拨号方向耦合（拨号=发送/capture 角色，接听=注入/emulation 角色）。反向场景中**被控端 B 拨号、控制端 A 接听**，与角色恰好相反，故双向各做一次「收养」：

```
B（被控端，Reverse）                          A（控制端）
connect_to_handle 拨号成功                      listener accept
  ├ slot + active_addr + alive=true            ListenEvent::Accept
  ├ 不 spawn hello/ping_pong/receive_loop      Connected{addr,fingerprint}
  └ EmulationRequest::AdoptReverse ────────►   maybe_adopt_reverse_session:
    listener.adopt(conn)                         IP 匹配 + Reverse + 非拨号回声
    = register_accepted_conn（与 accept 共用）     + 无活跃传输
    → read_loop(唯一读者) → 注入管线              ├ EmulationRequest::TakeConn ← listener
                                                └ adopt_inbound: 新 session + slot
                                                  + hello_handshake/ping_pong/receive_loop
```

capture 的发送路径（`MousehopConnection::send` / `send_on_session`）零改动——收养只是把连接装进 `conns` 表并设 `active_addr`。

### 关键正确性设计

- **拨号回声两跳闭环**：B 收养自己拨的连接后，listener 会发 `Accept`→`Connected`，B 的 `maybe_adopt_reverse_session` 按「addr == 该客户端配置的拨号目标」识别为自家拨号回声并跳过二次收养——否则连接会被从 listener 挖走装进 capture，两侧都成 capture 角色、无读者，隧道自杀。
- **代际守卫复用**：`adopt_inbound` 分配新 session，任何在途惰性拨号因 `sessions.is_current` 失败被现有完成检查自动废弃；无需新的互斥逻辑。
- **alive 语义分裂**（有意）：B 侧 `set_alive(true)` 无 Pong 佐证（该侧无 receive_loop）；存活由 listener 心跳（last_response，A 的 500ms ping 供血）+ 掉线后 tick 重拨兜底。A 侧走标准 Pong 校验。
- **孤儿 watchdog 退避**（D12，修复一个真 bug）：accept 时启动的 `hello_watchdog` 持 conn 引用，take_conn 后 12s 必杀已收养连接。修复为关闭前校验 `accepted_connection_is_current`——已离开 listener 连接表的会话其 watchdog 退避。僵尸 read_loop 收养后至多吃一个数据报即自退且不误发 Disconnected（现有 stale 守卫覆盖），Hello 重传预算（8×750ms）天然吸收该一次性丢包。
- **单向输入限制**（D13）：反向隧道上 B 可接收注入；B 控制 A（反方向注入）需要反向的拨号会话，本期不做，README 已注明。

## 验证

- 单元/集成：mousehop 83、input-capture 70、input-emulation 23 全绿（`RUSTUP_TOOLCHAIN=1.96.1`，D9）；clippy 仅预存 `start_service` dead-code 警告（gtk feature 关闭所致）。
- 端到端（单机双实例 + 双 Xvfb，等价「A 不可达 B」拓扑）：
  - A（控制端，:93）：client B 标 reverse、**端口故意配为不可达的 4399**（强制 A 的直连拨号永远失败，排除 direct 干扰）；capture 用 dummy 后端。
  - B（被控端，:94）：client A 标 reverse 指向 A:4342；emulation 落 X11。
  - 时序：B 先起 → A 后起 → 反向拨号+双侧收养（日志双确认）→ `MOUSEHOP_DUMMY_CAPTURE_DELAY_MS=12000` 让合成越边在隧道建立后触发 → Handover 越边（HandoverEnter/Ack）→ dummy 连续 Motion 以 Input 形式流经**收养连接** → B 的 Xvfb 指针持续跟踪 Lissajous 轨迹（`xdotool getmouselocation` 连续采样 640→839→568→562→690），60s+ 零真实掉线。
  - 脚手架：/tmp/mh2a/（a.toml 含 4399 端口配置；b13/a12 日志为最终验证轮）。

## 已知限制与差距

1. **真实两机验证缺位**：本机单机双实例 + Xvfb 模拟；真实 NAT/防火墙单侧可达场景（本特性的目标场景）待用户环境验证。
2. **输入单向**（D13，见上）。
3. **双方互标 reverse 且双向可达**（局域网）时会出现冗余连接：A 的直连拨号被 B 收养进 capture 后无 Pong 应答，~2s 内自清理；期间 active_addr 可能短暂抖动。真实部署（单侧可达）不会出现。
4. **take_conn 的模拟侧簿记**：`EmulationEvent::Connected` 已通知前端 DeviceConnected，随后会话被取走但无反向通知——前端展示语义正确（设备确实在线），只是该会话的 emulation 侧事件（PeerHello 除外）不再上报。
5. mousehop-proto / capabilities 无变更，老版本对端与反向模式互通性未测（反向模式要求双侧为本次之后的版本——老对端无 ConnectMode 概念，不会触发收养，退化为增量一行为：会话建立但输入不走隧道）。

## 下一步建议

- M2b（GPUI 控制端）：连接方式三路径的 UI 呈现，reverse 选项附「需要接收端配置」提示；等用户确认设计稿后开工。
- 真机验证清单（等环境）：真实 NAT 单侧可达两机、M1 vhci 注入、Windows 实验性标记复核。
