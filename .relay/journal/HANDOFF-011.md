# HANDOFF-011（M2b 增量六·布局画布 → 会话 #N+1）
会话：（本会话，见 state.json）  日期：2026-10-23  基线：feat/usbip-emulation@a9ac60e + 本次提交

## 一句话状态
M2b **增量六完成并冒烟全交互验证**：交互式布局画布（6 槽拖拽）已落地。下一步 = 公子拍板两件事（访问密码设计稿三问、D23 未布置语义），之后调试日志栏增量。

## 已完成（含证据）
- **布局画布**（console/layout.rs 新文件 + mod.rs 接线）：右栏下方「屏幕布局」常显画布——上排 ≤3 槽（<3 时补 1 个 dropzone）/ 中排 左+本机+右 / 下排 1 槽；绿边条标记共享边界朝向（Left 设备亮右缘、Top 亮下缘…）。设备列表行 `on_drag`（ghost = DeviceDrag 实体，s6 截图可见浮动 chip），dropzone `drag_over` 高亮 + `on_drop` → `place_device`：容量守卫（左/右/下占用拒绝、上排满 3 拒绝，`placement_rejection` 纯函数两例单测）→ `FrontendRequest::UpdatePosition`。离线设备半透明 0.6 + 「· 离线」标注可预布局；点槽内盒子 = 选中设备。
- **验证**：console 8 测试全过、clippy 零告警、fmt 干净。单实例冒烟（Xvfb :96 + 4486，/tmp/mh-inc6/，config 预置 alpha=left / beta=right）：**s2** 双设备按 config 入槽（离线半透明）；**s3** beta 拖→下方 dropzone，**config-a.toml `position="bottom"` 落盘**；s4/s11 alpha→上方/右侧同理落盘 top/right；**s12** alpha 拖→已占用下方 → toast「下方屏已有「beta-desk」，先移出再放置」且 config 不变；**s13** 成功 toast「alpha-book」已放到左侧屏 + config left；console 重启后画布按 config 恢复。
- 冒烟期间**发现并修复一个真 bug**：drop 监听器内 `notify_now`（再入 `window.update`）被静默吞掉、toast 不渲染（s3/s11 佐证：功能生效但无 toast，而 s10 搜索 toast 正常）——`place_device` 改收 `&mut Window` 直接 `window.push_notification`。
- 提交：见 git log（含 .relay 账本）。

## 关键决策与事实（D23 已录 PLAN）
- **「未布置」状态不落 daemon**（D23①）：`Position` 仅四向，加 `Position::None` 波及 capture/emulation/GTK/config 兼容——超出本增量，**留公子拍板**（`Position::None` 全链路 vs 映射「停用」）；槽内「× 移出」暂缓，画布现状 = `config.pos` 实时视图 + 放置时容量守卫。
- gpui 拖拽 API 归属（0.3.9 实测）：`on_drag` 在 StatefulInteractiveElement（元素须 id）；`drag_over::<T>`/`on_drop` 在 InteractiveElement（普通 div 可用）；ghost struct 必须 `#[derive(Clone)]`（on_drag ctor 收 `&T`，否则 `drag.clone()` 得引用 → `W=&T` 不满足 Render，编译期即报）。
- **元素监听器内弹通知必须 `window.push_notification` 直推**（监听器本有 `&mut Window`）；`notify_now` 只用于无 window 的订阅/feed 回调路径。
- `StyleRefinement: Styled`——drag_over 闭包内可用全部 Styled builder（bg/border_color/text_color/opacity）。
- 冒烟规程：xdotool 链式命令在 windowfocus 后会把子命令当文本打字（输入框出现 "sleep0.3keyReturn" 字样）——**分段独立调用**；无 WM 输入前必须 `xdotool windowfocus <id>`（`xwininfo -root -tree` 取 id）。`pkill -f` 自戕坑二次确认（D21⑤）：清理与重启必须分开两次调用，模式用 `mh-inc6[/]config` 括号法。
- 长驻进程：console 用宿主 run_in_background 稳定存活；同调用 nohup+& 偶发随调用回收（本次 console 起三次才稳）。
- 沿用：daemon `--no-default-features` / console 分开构建；`RUSTUP_TOOLCHAIN=1.96.1`；Xvfb SVG 图标不渲染（本增量 UI 全文字，不受影响）。

## 下一步精确动作
1. **等公子拍板**：① `.relay/DESIGN-access-password.md` 三问（是否落指纹/nonce 挑战版/限速 3 次 30s）→ 浮层加「安全」区；② D23① 未布置语义（建议：`Position::None` 全链路，GTK 下拉补「不参与」项，IPC 序列化加 `"none"` 变体并按 AGENTS.md 规则升版本）→ 补槽内「× 移出」。
2. 无需拍板可先行：**调试日志栏**增量（D14⑦：底部追加日志栏，设计稿 `.mh-logbar`）；详情页补发送侧共享选项（`SetClientClipboardSend` 等 per-client 字段，daemon 请求已存在）。
3. gpui-kit bug 最小复现上报（D21④⑤ 两枚）。
4. UI 集成测试 `#[gpui_kit::test]` 随手补。

## 风险与阻塞
- 真机验证欠账：Xvfb 图标不渲染 + 本增量拖拽观感（ghost 跟手、dropzone 高亮）都待真机确认。
- 冒烟清理已做：daemon/console 已停、Xvfb :96 已杀（:91/:93/:94/:97 非本会话产物未动）；/tmp/mh-inc6/ 保留证据（s1–s13 有效：s2/s3/s5/s6/s10/s11/s12/s13 关键）。
- 上排满 3 的拒绝路径仅单测覆盖（本机只有 2 台设备），真机多设备时可补验。

## 上下文指针
- 计划：`.relay/PLAN.md`（D14–D23）；上篇：HANDOFF-010（增量五）
- 设计稿：/home/zero/.zcode/v2/visualizations/3d252e46…/mousehop-console-v2.html（布局画布 CSS `.mh-canvas/.mh-scrbox/.mh-dropzone`，JS `placeDevice` 容量规则）
- 拖拽 API 源码：~/.cargo/registry/src/*/gpui-pre-0.3.9/src/elements/div.rs（on_drag 1620 / drag_over 1191 / on_drop 1229）
- 冒烟：/tmp/mh-inc6/（config-a.toml 两客户端模板可直接复用）
