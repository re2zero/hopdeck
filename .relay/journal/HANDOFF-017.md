# HANDOFF-017（真机联调收口 → M3a 设计）
会话：#11（session_c91c31d66c3c9eab4aa1347b851a452f 续，上篇 HANDOFF-016）  日期：2026-10-24  基线：feat/usbip-emulation@b30fe07 + 本会话 relay 文档提交

## 一句话状态
公子授权真机联调（uzero@10.8.15.62）**全部完成**：真实两机直连/反向、USB/IP 真机键鼠注入、X11↔USB-IP 双向热切、console 真机观感——M1/M2a/M2b 真机欠账仅剩「真机 Wayland」一项；两处真机新发现缺陷已记账（D31）。下一步 = M3a 画面链路设计蓝本。

## 已完成（含证据）
- **真机联调七项全过**（报告 + 配方 + 证据索引：`.relay/journal/REALMACHINE-REPORT.md`）：
  1. 真实两机直连 + 真实桌面注入（xdotool 指针 Lissajous）
  2. 真实两机反向外呼（B 拨入 A 收养 + 注入）——前置：双向指纹预授权
  3. USB/IP 真机·鼠标：vhci attach → HID 枚举 → 隧道流注入真实指针
  4. USB/IP 真机·键盘：evdev 层捕获 "hello world" 全 DN/UP 序列（hello↔KEY_H… 映射核对）
  5. X11↔USB-IP 会话存活双向热切（M2b 验收项 5 真机版）
  6. console 真机渲染完美（软件 GL）——Xvfb 图标之谜告破
  7. 无 portal 环境注入生效（M1 验收第三条）
- **报告落盘**：REALMACHINE-REPORT.md + PLAN D31 + M1/M2b 里程碑注记更新。
- 本会话零产品代码改动（真机探针 bin 已删，git 无痕）。

## 关键决策与事实（D31 要点）
- **真机缺陷 1（连接侧，待修）**：反向启动竞速不自恢复——首拨 fatal alert（对端未授权或 listener 未就绪）后保活心跳死槽、永不重拨；修复方向 = 保活 tick 检测「active reverse 无活会话」强制重拨/清槽。
- **真机缺陷 2（上游相关）**：GPUI 在 Radeon HD 6450（TeraScale2 无 Vulkan）真实桌面花屏；`LIBGL_ALWAYS_SOFTWARE=1` 完全修复；Xvfb 图标之谜同案（Xvfb=软件 GL，图标资产无恙）。
- usbip 用户态工具 UOS 无包 + /usr 只读 → kernel 6.6 子树免 autotools 直编（配方在报告）；modinfo 用下划线名查不到连字符文件名。
- **自戕第四变体**：ssh 远端复合命令 pkill + 字面路径共存仍自杀——kill 必须独立成条（第四次 reaffirm）。

## 下一步精确动作
1. **M3a 设计蓝本**：起草 `.relay/DESIGN-m3a-video.md`——采集（Linux PipeWire portal / Windows DXGI）、硬编解码选型调研（VAAPI/NVENC/MFT）、独立 DTLS 通道帧协议（capabilities 位 + 新 tag 追加，D6 先例）、分帧/FEC（D5 留白）、RenderSurface CPU 路径对接（ely surface.rs 语义）。素材：README 画面章节、M2b-REPORT 下一步建议、REALMACHINE-REPORT。成文后等公子拍板再实现（M2b 先例）。
2. 若公子在场可拍板：反向启动竞速缺陷是否排连接侧增量（D31④）、Wayland 测试机安排、console 自动重连语义（D27③）。
3. 勿做：gpui-kit 上报（D29 延后至全部完成）。

## 风险与阻塞
- 真机 Wayland 验证无测试机（GNOME 45+/KDE 6.1+）——M1/M2b 字面验收的最后缺口，等公子安排硬件/环境。
- M3a 工作量大（采集/编码/传输/解码/渲染五段），必须增量推进 + 设计先行。
- 62 上部署残留 `~/mh-rt/`（二进制/日志/截图）已保留作证据；模块已卸载、进程已清、后端已回 auto。

## 上下文指针
- 计划：`.relay/PLAN.md`（D14–D31）；上篇：HANDOFF-016
- 真机报告：`.relay/journal/REALMACHINE-REPORT.md`（配方 + 缺陷 + 里程碑影响）
- M2b 报告：`.relay/journal/M2b-REPORT.md`；gpui-kit 材料：`.relay/journal/GPUIKIT-ISSUES.md`（D29 延后）
- 冒烟/联调证据：/tmp/mh-inc13/（一控多）、/tmp/mh-rt/（真机，含 console 截图三态）、62:~/mh-rt/（daemon 日志）
