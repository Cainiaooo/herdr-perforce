# ADR-0003：键位所有权与破坏性操作

状态：Accepted
日期：2026-08-24

## Context

P4 TUI 运行在 Herdr 管理的 terminal pane 中。Herdr、插件、输入框和 overlay 都可能对同一按键有意义。若所有状态都直接处理单字母键，用户在输入描述、评论或关闭确认框时可能意外触发导航或提交。

Submit 是不可轻易恢复的外部写操作。单个 `s` 不能直接构成提交授权。

## Decision

按键按以下优先级路由：

1. Herdr 宿主保留的全局/prefix 快捷键。
2. 当前 plugin overlay 或文本输入控件。
3. 获得焦点的 P4 pane 普通导航键。
4. 未处理输入交回 terminal 的默认行为。

具体规则：

- P4 pane 未获得焦点时，不消费普通单字母键。
- 文本输入激活时，字母、删除、光标移动优先属于输入控件；只有明确的 Cancel/Accept 键保留特殊含义。
- overlay 打开时，背景 pane 不处理快捷键。
- `Esc` 逐层取消输入或 overlay，不提交、不 Apply、不关闭整个 Herdr workspace。
- `q` 只在没有 modal/输入状态时关闭 P4 pane。
- `s` 只打开 Submit review overlay，绝不执行 `p4 submit`。
- Submit overlay 默认焦点为 Cancel；最终提交只能通过明确点击 Submit 或 `Ctrl+Enter`。
- `Enter` 不作为 Submit overlay 的最终提交快捷键。
- Description Apply 使用独立 overlay 和确认，不复用 Submit 授权。
- 重复键、双击和按键自动重复必须被单飞状态拦截。

配置键位时：

- 插件只允许重绑定 pane 内动作，不覆盖 Herdr 宿主保留键。
- 同一上下文中的冲突使配置整体无效。
- 破坏性动作可以更换“打开确认框”的键，但不能配置成绕过确认。

## Rejected alternatives

### `s` 直接提交

否决原因：误触成本过高，且与显式确认产品原则冲突。

### 插件优先处理所有输入

否决原因：会破坏 Herdr 宿主快捷键和文本编辑体验。

### 仅依赖“再按一次 s”确认

否决原因：按键自动重复和肌肉记忆容易产生误提交；可见 overlay 更容易核对目标 CL。

## Consequences

- TUI state machine 必须显式区分 Browse、TextInput、ReviewOverlay、SubmitRunning 等输入模式。
- Herdr 集成验收必须验证焦点切换和宿主 prefix 不被吞掉。
- 文档中的快捷键表描述的是“打开操作”，不是写操作授权本身。
