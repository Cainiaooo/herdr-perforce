# ADR-0006：独立内容 pane 与最右侧导航

状态：Accepted
日期：2026-08-27

## Context

首版把 File/ Diff 预览和目录/CL 导航画在同一个终端 pane。长行会压缩或错位右侧树，预览也只能渲染当前静态页面，无法获得完整的纵向和横向滚动体验。导航树只需要较窄宽度，而代码和 diff 需要接近 Agent CLI 的主要阅读宽度，两者的尺寸目标并不一致。

社区 `herdr-sidebar` 证明了独立 TUI 内容 pane、语法高亮和 control-file 原地更新在 Herdr 中可行，但它把目录树放在内容左侧；本插件需要让树保持在屏幕最右侧。

## Decision

- Explorer/Review 共享一个最右侧的窄导航 pane；该 pane 不再渲染 File 或 Diff 正文。
- 默认只有 `Agent CLI 80% | Navigation 20%`。
- 首次打开 File、Diff 或 CL 文件列表时，把导航 pane 的左邻 pane 一分为二，得到 `Agent CLI 40% | Content 40% | Navigation 20%`。
- File、Diff 和 CL 文件列表复用同一个 Content pane，通过插件私有 control file 原地切换，路径不经过 shell quoting。
- Content pane 按当前宽度把长行稳定拆成多行并按显示行滚动，同时处理文本行号、文件语法高亮和 diff 语义颜色。
- 在 CL 文件列表中选择具有本地 client path 的文件，可在同一 Content pane 下钻到 Diff；`Esc` 返回 CL 文件列表。
- 最后一次 File/Diff/CL 请求由插件按 workspace 持久化；Herdr server 重启后先清理 session 恢复出的 Content shell，再以 `--no-focus` 重建中间 Content pane。

## Consequences

- 长文本不会再改变导航树的宽度或对齐。
- 内容关闭后 Herdr 会折叠对应 split，导航仍保留在最右侧。
- 宿主布局操作必须失败关闭；通过 `pane.layout` 的实际矩形定位导航左邻并在创建后验证 `Agent | Content | Navigation` 顺序。无法取得 `HERDR_PANE_ID`、无法确认布局或无法启动 viewer 时，导航 pane 显示错误但不退回内嵌预览。
- 预览继续遵守既有字节/行数预算；“可滚动”不代表无界读取超大或二进制文件。
