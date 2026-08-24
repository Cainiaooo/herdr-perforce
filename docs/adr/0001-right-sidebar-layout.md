# ADR-0001：右侧紧凑 pane 与左 Diff、右导航

状态：Accepted
日期：2026-08-24

## Context

Herdr 的主要工作区已经有三层职责：左侧是 Spaces、Workspaces 和 Agents，中间是 Agent CLI，右侧是 Files、Browser、Terminal 等可切换工具。

P4 changelist 能力的目标是辅助 Agent 工作，而不是建立一个占据整个 Workspace 的 P4V 替代界面。最初的三列独立工作台会挤压 Agent CLI，也会让用户在审阅时离开主要对话上下文。

常见 SCM 工具通常把导航放左、diff 放右。这个布局在独立全屏应用中合理，但本插件贴在 Agent CLI 的右边。

## Decision

- P4 作为 Herdr 右侧可切换 plugin pane，不创建独占 Workspace。
- pane 内部使用两列：左侧 Diff 约 70%，右侧 Changelist/File 树约 30%。
- Diff 靠近中间 Agent CLI，使代码、审阅备注和 Agent 对话在视觉上相邻。
- 较窄的导航贴屏幕右边缘，鼠标选择可以利用边缘的过冲容错。
- 分隔线可拖动；极窄时降级为 Diff/导航单视图切换。

## Rejected alternatives

### 独占三列 Workspace

否决原因：占用过大，割裂 Agent CLI，与 Herdr 右侧工具生态不一致。

### 左导航、右 Diff

否决原因：Diff 与 Agent CLI 被导航隔开，用户在“读 diff—问 Agent—回到 diff”之间需要更长的视线和鼠标移动。它仍可作为未来用户配置，但不是首版默认值。

### 仅显示文件列表，另开 pane 查看 diff

否决原因：增加 pane 生命周期和焦点切换，窄宽场景更难理解。

## Consequences

- 实现必须优先优化紧凑宽度，而不是从全屏 TUI 缩小。
- 路径、统计和按钮必须支持渐进隐藏。
- 终端快照测试必须覆盖标准、中等和极窄三种宽度。
- 实现者不得把默认方向“修正”为传统 SCM 布局，除非由新的 ADR 取代本决策。
