# Architecture Decision Records

本目录记录 `herdr-perforce` 中会影响产品形态、安全或长期实现边界的决策。

ADR 只描述一项已接受决策、理由和后果。产品设计见 [design.md](../design.md)，可执行验收见 [acceptance.md](../acceptance.md)。

| ADR | 状态 | 决策 |
|---|---|---|
| [ADR-0001](0001-right-sidebar-layout.md) | Accepted | 使用 Herdr 右侧 pane，内部左 Diff、右导航 |
| [ADR-0002](0002-consistency-and-async-invalidation.md) | Accepted | 使用双 freshness token、request epoch 和有界缓存 |
| [ADR-0003](0003-key-ownership-and-destructive-actions.md) | Accepted | 由焦点决定普通键所有权，破坏性操作必须二阶段确认 |
| [ADR-0004](0004-agent-generator-trust-boundary.md) | Accepted | 可执行生成器配置仅来自 Herdr 用户级插件配置目录 |

新 ADR 使用递增四位编号。已接受 ADR 不静默改写结论；如结论改变，应新增 superseding ADR 并更新索引。
