# ADR-0007：Content pane 使用整文件内联叠加 Diff

状态：Accepted
日期：2026-08-28

## Context

首版设计把 Diff 画成 `p4 diff -du` 的 unified patch：`---` / `+++` / `@@` 头、只有增删行、没有行号。这和 Agent CLI 对话里的一闪预览接近，但和专门用来审文件的表面不一致。

主流选择已经收敛：

- Codex CLI TUI：单列、行号 + `+/-` gutter、增删浅底、hunk 间用 `⋮`。
- Claude Code TUI：行底 `diffAdded` / `diffRemoved`，词级 `diffAddedWord` / `diffRemovedWord`。
- Cursor / VS Code Inline：当前文件当画布，删除行插在改动点上。
- 左右分栏在 Content pane（约 40% 宽）里不可用；pi-diff 在窄终端也会退回单列。

用户要求以文件内容为基底、改动显示在原地、按类型着色，并且远距未改区可折叠、可展开。

## Decision

- Diff 的画布是工作区当前文件（add 为整份新增，delete 为整份删除）。
- 删除行插在改动位置上方（红底、gutter `-`），新增行就地（绿底、gutter `+`），未改行保持 File 预览的语法高亮。
- 配对的删/增行做词级高亮（与 Claude / GitHub 的 word-diff 同类），无关替换不强行铺词底。
- 远距未改默认折叠，保留每侧 `diff_fold_context` 行（默认 5，`0` 关闭折叠）；折叠行可点击展开，工具栏和 `e` 可全部展开/收起。
- 工具栏提供 Prev / Next hunk；`[` / `]` 等价。不提供 side-by-side。
- 配置只放在 `HERDR_PLUGIN_CONFIG_DIR/panel.json` 的 `diff_fold_context`。

## Consequences

- `docs/design.md` §6.2 从 “unified diff 原文” 改为内联叠加。
- 仍使用 `p4 diff -du` 作为改动来源，但 parser 把它投影到文件行上，不再把 patch 文本当 UI。
- binary、无权限、空 diff、截断继续走既有明确状态，不伪装成 0 changes。
