# gpui-kit / gpui-component 上游问题清单（待公子上报）

环境基线：gpui-kit 0.8.0 / gpui-component 0.8.0 / gpui-pre 0.3.9（非 gpui-fast 路径）/ Linux Xvfb（无 WM）。
本项目：mousehop-console（GPUI 控制台，源码随仓库 `feat/usbip-emulation` 分支可查）。

---

## 1. gpui-kit `test-support` feature 破坏下游依赖解析（gpui-fast 精确钉子 vs gpui-pre 浮动版本）

**现象**：下游 crate 只要声明 `my-feature = ["gpui-kit/test-support"]`（**即使从不启用**），workspace 的
lockfile 解析立即失败；把该声明放进 `[dev-dependencies]`（`gpui-kit = { features = ["test-support"] }`）同样失败。
报错均为 gpui-fast 系 crate 的精确版本钉子与锁内 gpui-pre 系的浮动版本冲突，至少三例：

```
cocoa:               gpui-fast-macos 0.1.10 要求 =0.26.0，gpui-pre-macos 0.3.9 浮动 ^0.26.0（锁 0.26.1）
unicode-properties:  gpui-fast-web   0.1.10 要求 =0.1.3， gpui-pre-web   0.3.9 浮动 ^0.1.3（锁 0.1.4）
cfg_aliases:         wgpu 30.0.1（经 gpui-fast-wgpu）要求 ^0.2.2，锁内 0.2.1（nix 0.30.1 路径）
```

**最小复现**（工作区任一成员，依赖 gpui-kit 0.8.0 默认特性，解析正常）：

```toml
# mousehop-console/Cargo.toml  追加以下声明，不启用：
[features]
ui-test-support = ["gpui-kit/test-support"]
```

```sh
cargo build -p mousehop-console        # feature 未启用，仍然失败：
# error: failed to select a version for `unicode-properties`.
#   ... required by package `gpui-fast-web v0.1.10` ... of `gpui-kit v0.8.0`
```

**对照实验**（同一工作区逐项验证）：

| 声明 | 普通构建（feature 关） |
|---|---|
| 无任何声明 | ✅ 通过 |
| `ui-test-support = ["gpui-kit/test-support"]` | ❌ 解析失败 |
| `ui-test-support = ["gpui/test-support", "gpui-base/test-support", "gpui-component/test-support"]` | ✅ 通过 |

**判读**：触发面精确收敛在 gpui-kit 自己的 `test-support` feature 图——它把 gpui-fast 平台 crate 带进解析
（具体激活边未逐环定位；kit 的 test-support 表面全是 `?` 引用，疑似经某个子 crate 的非 `?` 引用或 dev-deps 并入）。
而 gpui-fast 0.1.10 对 cocoa/unicode-properties 用 `=` 精确钉、gpui-pre 0.3.9 同名传递依赖用浮动语义，
两系共存时只要锁内偏好新版本即无解。

**建议**：① 对齐 gpui-fast 与 gpui-pre 的公共传递依赖版本约束（去掉 `=` 或同步升版）；
② 让 `gpui-kit/test-support` 不再（直接或间接）激活 gpui-fast 平台 crate。

**我方 workaround**：绕开 gpui-kit 的 feature，直接 `ui-test-support = ["gpui/test-support", "gpui-base/test-support",
"gpui-component/test-support"]`（见 mousehop-console/Cargo.toml `[features]` 注释）。另注意：给 gpui-kit 发
dev-dependency 也会触发同一冲突，下游无法用常规姿势做 UI 集成测试。

---

## 2. gpui-component 0.8.0 在 gpui-base/test-support 下编译失败（Observed 泄漏）

**现象**：下游启用 `gpui-base/test-support`（例如做无头 UI 测试）但未启用 `gpui-component/test-support` 时，
gpui-component 0.8.0 编译失败：

```
error[E0599]: no method named `vertical_scrollbar` found for struct `Observed<E>`
  --> gpui-component-0.8.0/src/menu/popup_menu.rs:1537:22
```

**原因**：gpui-component 源码内多处（popup_menu、sheet 等）在元素上链式调用 `.test_support()`。该方法是
gpui-base 的 `TestSupportExt`，随 **gpui-base 的** test-support feature 激活并改变返回类型（`Observed<E>`）；
而 gpui-component 为 `Observed<E>` 补的 trait 接线（如 `ScrollableElement`）却门在 **gpui-component 自己的**
test-support 上（`src/scroll/scrollable.rs:313`）。两层 feature 不同步即编译失败——基础层 feature 改变了组件层的类型，接线却跟组件层 feature 走。

**最小复现**：下游 `cargo add gpui-kit`（默认特性）后声明启用 `gpui-base/test-support`（不经 gpui-kit），构建即现。

**建议**：gpui-component 里这些 `Observed` impl 的 cfg 改为跟随其真正生效条件（`gpui-base/test-support`），
或在 `.test_support()` 调用点统一门控。

---

## 3. AlertDialog `on_ok` 确认后整窗输入锁死（D21④）

**现象**：`WindowExt::open_alert_dialog` 打开的对话框，**鼠标点击 OK（on_ok 路径）确认后**，整个窗口对所有
鼠标点击与键盘输入无响应（Esc 也无效），进程重启才恢复。取消路径未能复现。带输入框的授权对话框 Enter=on_ok
路径正常（D21①）。

**复现环境**：Xvfb :98（无 WM），本项目「移除设备」确认对话框：列表行右键/按钮 → AlertDialog（默认按钮 +
`.on_ok` 内发 IPC 请求 + `true`）→ 点击确认按钮 → 窗口锁死。

**代码形态**（mousehop-console/src/console/detail.rs，删除设备按钮）：

```rust
let _ = window.update(cx, |_, window, cx| {   // 订阅回调路径（无 &mut Window）
    window.open_alert_dialog(cx, move |alert, _, _| {
        alert.title("移除设备").ok_text("移除").cancel_text("取消")
            .on_ok(move |_, _, cx| { writer.send(FrontendRequest::Delete(handle)); true })
            .on_cancel(|_, _, _| true)
    });
});
```

**建议**：排查 on_ok 确认后对话框/焦点状态的清理（疑似 modal 输入泵未摘除）。无法稳定最小化到独立小工程，
可提供本项目的复现步骤与构建（仓库分支现成）。

---

## 4. ghost Button 命中死区：绘制可见、悬停无高亮、点击无效（D21⑤）

**现象**：设备详情信任区的「撤销信任」`Button`（ghost + 图标，与相邻「移除设备」按钮同构同排）绘制正常，
但 hover 无高亮、点击无效果——命中测试丢失。相邻同构按钮正常。Xvfb :97 验证，增量三起预存。

**代码形态**：两按钮同为 `Button::new(id).icon(..).ghost().small().on_click(cx.listener(..))`，仅 id/回调不同。
冒烟中以 IPC 请求绕过（`RemoveAuthorizedKey`）。真机桌面尚未复测（Xvfb 图标不渲染问题并存，见 D22⑤）。

**建议**：查 ghost 变体按钮的 hitbox/交互区计算；我方可提供复现构建。

---

## 5. 元素事件监听器内 `window.update` 再入被静默吞掉（D23③，已绕过）

**现象**：drop/点击等**元素事件监听器**（已持有 `&mut Window`）内，若再经 `AnyWindowHandle::update` 弹
Notification（`window.push_notification`），该再入更新被吞（错误被 `let _ =` 吸收），toast 不渲染、无任何报错；
同样的 `notify_now` 在**订阅回调**（无 `&mut Window`，如搜索框 PressEnter）路径工作正常。

**我方修法**：监听器内一律直接 `window.push_notification(note, cx)`（监听器本有 window），不再走
`AnyWindowHandle::update` 再入。

**建议**：a) 文档明示元素监听器内禁止 `AnyWindowHandle::update` 再入；b) 或对被吞的再入错误给
`trace!/debug!` 一类可见信号——静默丢失排查成本高（本项目定位该问题花了整轮冒烟）。

---

### 附：上报优先级建议
1（解析破坏，阻断一切下游 UI 测试，且有干净对照实验）> 2（编译失败，机制清晰）> 3（功能性锁死）>
5（静默失败，已绕过）> 4（疑环境相关，真机复测后再定）。
