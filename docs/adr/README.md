# Architecture Decision Records

本目录记录 `herdr-perforce` 中会影响产品形态、安全或长期实现边界的决策。

ADR 只描述一项已接受决策、理由和后果。产品设计见 [design.md](../design.md)，可执行验收见 [acceptance.md](../acceptance.md)。

| ADR | 状态 | 决策 |
|---|---|---|
| [ADR-0001](0001-right-sidebar-layout.md) | Superseded | 旧的单 pane 内左 Diff、右导航布局 |
| [ADR-0002](0002-consistency-and-async-invalidation.md) | Accepted | 使用双 freshness token、request epoch 和有界缓存 |
| [ADR-0003](0003-key-ownership-and-destructive-actions.md) | Accepted | 由焦点决定普通键所有权，破坏性操作必须二阶段确认 |
| [ADR-0004](0004-agent-generator-trust-boundary.md) | Accepted | 可执行生成器配置仅来自 Herdr 用户级插件配置目录 |
| [ADR-0005](0005-in-plugin-file-explorer.md) | Accepted | 工作区 File Explorer 在本插件内独立实现，不依赖社区插件 |
| [ADR-0006](0006-standalone-content-pane.md) | Accepted | Agent 与最右导航之间按需创建可复用的独立 Content pane |
| [ADR-0007](0007-inline-overlay-diff.md) | Accepted | Diff 以当前文件为画布做内联叠加，而不是 unified patch 原文 |

新 ADR 使用递增四位编号。已接受 ADR 不静默改写结论；如结论改变，应新增 superseding ADR 并更新索引。
