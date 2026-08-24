# ADR-0002：一致性、异步失效与有界缓存

状态：Accepted
日期：2026-08-24

## Context

P4 pending changelist 同时包含服务器上的 spec/文件动作和 client workspace 中尚未提交的内容。单一服务器时间戳无法证明用户正在确认的本地内容没有变化。

侧边栏还会并发加载 CL、files 和 diff。用户快速切换选择时，较早请求可能晚于新请求完成。如果没有显式代际机制，旧结果可能覆盖当前选择。大型游戏项目的 changelist 也要求缓存有明确上限。

## Decision

### 双 freshness token

`spec_token` 是以下规范化数据的 BLAKE3：

- workspace identity 中与写操作相关的 server/client/user。
- CL id、status、owner、client、description 和其他必须保留的 spec 字段。
- 按稳定顺序排列的 depot path、action、file type、base revision 和 move 关系。

`content_token` 是当前可提交内容的规范化指纹：

- 文本文件使用规范化 diff bytes。
- add/delete 无普通 diff 时使用生成该 diff 的内容 bytes。
- binary 使用流式 BLAKE3，不把完整内容保存在内存中。
- 每个文件的指纹按 canonical depot path 稳定排序后，再生成 CL 级 token。

Description Apply 前重新查询并比较 `spec_token`。Submit confirmation 前同时比较 `spec_token` 和 `content_token`。任一变化都使原确认失效并要求刷新。

Token 不是 P4 Server 事务锁。最终 `p4 submit` 的原子性和拒绝结果仍由服务器决定。

### 请求代际

- `repository_generation`：workspace identity 改变或手动全局刷新时递增。
- `selection_epoch`：当前 CL/file 选择改变时递增。
- 每个异步请求携带唯一 request ID、generation、epoch 和资源 key。
- 结果只有在 generation、epoch 和资源 key 仍匹配时才能写入可见 UI state。
- 取消旧请求用于节省资源；即使取消失败，代际校验仍保证正确性。

### 有界缓存

- metadata cache 默认最多 4,096 entries。
- diff cache 按实际 bytes 使用 LRU，默认总上限 64 MiB、单 entry 上限 8 MiB。
- binary hash cache 默认最多 4,096 个只含 path/file identity/hash 的 records。
- binary 内容不进入 cache，只缓存小型 metadata 和 content hash。
- workspace identity/generation 是 cache key 的一部分，禁止跨 client 复用。
- 手动刷新提升 generation；过期结果可被回收但永不重新进入当前 UI。
- 预算可在用户级配置中调低或调高；任何配置都必须有有限正数上限。

### 并发上限

- 只读 P4 查询默认最多 4 个并发子进程。
- Agent generator 同时最多 1 个；新生成请求必须先取消或等待旧请求结束。
- Submit 同时最多 1 个，并进入排他的 `SubmitRunning` 状态。
- binary hashing 使用有界 worker，不与 UI 线程同步读取完整文件。

## Rejected alternatives

### 只使用 `p4 change -o` 时间或 spec hash

否决原因：无法识别本地 pending 文件在 CL spec 不变时发生的内容变化。

### 只依赖进程取消

否决原因：子进程可能已经完成或无法及时取消，仍会产生竞态结果。

### 无上限会话缓存

否决原因：大型 CL 和长时间 Herdr session 会造成不可控内存增长。

## Consequences

- hashing binary 可能增加 Submit confirmation 延迟，但使用流式读取，内存保持有界。
- Apply Description 不需要重复 hash 所有本地内容。
- fake P4、慢请求和 stale CL 都必须有确定性测试。
- 实现必须将“选择状态”“请求状态”“缓存状态”分开建模。
