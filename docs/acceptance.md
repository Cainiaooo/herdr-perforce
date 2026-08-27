# Herdr Perforce 首版验收计划

状态：首版验收基线
关联设计：[design.md](design.md)
测试环境：[testing.md](testing.md)

## 1. 验收目标

本计划验证 `herdr-perforce` 是否能够作为 Herdr 右侧紧凑工具面板，安全完成以下工作流：

1. 从当前 Herdr workspace 识别正确的 P4 client。
2. 在右侧 pane 内浏览 changelist 和 files。
3. 在同一 pane 的左侧查看可靠的 diff。
4. 将选中 diff 的审阅备注发送给明确的当前 Agent。
5. 使用可配置的 Agent CLI one-shot 生成 CL 描述。
6. 人工预览、编辑并应用描述。
7. 在专用可丢弃 P4 环境中，经过预检和确认提交 numbered pending CL。

“编译通过”或“单元测试通过”不能单独视为产品验收完成。首版完成需要自动化证据、Herdr 运行时证据，以及隔离 P4 环境中的写操作证据。

## 2. 验收环境分层

### 2.1 Level A：离线自动化

使用受控 fake `p4` executable 和 fixture，不连接真实 P4 Server。

覆盖：

- argv 和 cwd。
- JSON/tagged record 解析。
- changelist/domain mapping。
- diff 组装。
- UI state transitions。
- Prompt 构建与 Agent runner。
- preflight 和确认状态机。
- 错误、超时、取消和输出预算。

### 2.2 Level B：只读真实 P4

可以在用户现有 P4 环境中执行，但仅限明确的只读查询：

- client/workspace identity。
- changes/opened/describe/fstat/where/diff 等只读命令。
- 不创建、不编辑、不移动、不 shelve、不 submit、不 revert 任何真实资产。

该层用于确认实际 P4 版本、服务器配置、路径和输出差异。连接失败、ACL 或沙箱限制必须与产品缺陷分别记录。

### 2.3 Level C：隔离写入验收

描述 Apply 和 Submit 只能在以下任一环境执行：

- 临时本地 P4 Server 和临时 depot/client；或
- 经用户明确批准、初始干净、可丢弃的专用测试 depot/client/CL。

默认实现采用 [testing.md](testing.md) 定义的 loopback、每轮独立、进程与路径均可验证的一次性 `p4d`。Level C harness 不得 fallback 到用户默认 `P4PORT`、`.p4config` 或日常 workspace。

禁止将日常开发 CL、共享项目资产、他人 shelved CL 或无法恢复的 workspace 用作写入 fixture。

验收结束后必须证明：

- 没有意外 pending CL。
- 没有遗留 opened files。
- 没有遗留 lock。
- 测试 fixture 已按计划保留或清理。

## 3. 优先级与发布 Gate

P0/P1 表示场景本身的正确性优先级，不再等同于“第一个版本必须满足”。交付采用两道独立 Gate，避免把内部日用版本和公开分发版本混成一个无限扩张的首版。

| 优先级 | 含义 |
|---|---|
| P0 | 所属 Gate 的正确性阻断项；不能靠记录已知限制跳过 |
| P1 | 所属 Gate 的重要质量项；可经明确产品决策延期 |
| P2 | 不属于当前两道 Gate 的后续增强 |

### 3.1 Dogfood Gate：自己可以每天使用

Dogfood Gate 证明核心工作流在开发者自己的 Windows + Herdr + P4 环境中可安全日用。必须满足：

- Windows build、质量门禁和本地 plugin link。
- 真实 Herdr 右侧 pane、两列布局和极窄降级。
- 当前 client 的 CL/File 树、工作区 Explorer（树/预览/只读装饰）和 pending/shelved/submitted diff。
- binary metadata 和锁信息的可用降级。
- Agent 审阅消息闭环。
- Agent CLI one-shot 生成、preview、Apply 的失败矩阵。
- Level B 真实只读 P4 验证。
- Level C 隔离环境中的一次 Description Apply 和一次 numbered CL Submit。
- stale token、异步 epoch、重复提交和写入残留检查。
- 用户级配置边界和基本秘密扫描。

对应场景以 ACC-BUILD、ACC-UI-001 至 004 及 006/007、ACC-P4、ACC-TREE、ACC-EXPLORER、ACC-DIFF、ACC-BINARY、ACC-REVIEW、ACC-GEN、ACC-SUBMIT、ACC-CONFIG、ACC-STATE、ACC-PRIVACY、ACC-PERF-003 至 005、ACC-STABILITY 和 ACC-RELEASE-001 为主。

Dogfood Gate 不要求他人能够在无 Rust 环境中安装，也不要求完整公开发布/卸载体验。

### 3.2 Distribution Gate：可以交给别人安装

Distribution Gate 包含全部 Dogfood Gate，并额外要求：

- 受支持终端/显示适配矩阵。
- 大型 CL 和慢服务器性能基线。
- 预编译 Windows release artifact。
- 无 Rust toolchain 的干净安装。
- checksum、版本和来源验证。
- 明确且经过验证的升级/卸载语义。
- 发布包、fixture、snapshot 和文档的完整隐私扫描。
- 面向外部用户的安装、配置、安全和故障排查文档。

对应场景包括 ACC-UI-005、ACC-PERF-001/002、ACC-RELEASE-002/003，以及所有在 Dogfood 阶段被明确延期的 P1。

Distribution Gate 未通过时，可以发布内部 dogfood build，但不得宣称插件已具备公开可安装体验。

### 3.3 Gate 变更规则

- 场景可以通过产品决策从一个 Gate 移到另一个 Gate，但不能仅通过降低 P0/P1 标签掩盖正确性缺口。
- 任何写操作安全、stale 检测或残留状态检查不得移出 Dogfood Gate。
- Gate 结论必须引用实际执行证据，而不是只引用本文件。

## 4. 构建与静态质量

### ACC-BUILD-001（P0）Windows 构建

前置条件：受支持 Rust toolchain。
步骤：构建 release binary。
期望：

- 构建成功。
- 不依赖 P4API.NET、Visual Studio extension host 或 Unix-only shell。
- release binary 可在干净 Windows 用户环境启动。

证据：完整构建命令、exit code、binary 路径和版本输出。

### ACC-BUILD-002（P0）质量门禁

依次执行：

1. formatter check。
2. clippy/lint。
3. build。
4. tests。
5. diff whitespace/link check。

期望：全部通过；不得并行运行可能共享生成产物的 build/test 步骤。

### ACC-BUILD-003（P0）Manifest 校验

步骤：通过当前支持版本的 Herdr link 本地插件。
期望：

- manifest 可解析。
- `min_herdr_version`、platforms、action 和 pane entrypoint 正确。
- 没有未知 event 或平台警告。
- unlink 后源码目录保持不变。

## 5. Herdr 宿主与布局

### ACC-UI-001（P0）右侧工具 pane

步骤：在含 Agent CLI 的正常 Herdr workspace 中打开 P4 插件。
期望：

- P4 以右侧 pane 打开。
- Agent CLI 保持在中间主区域并继续运行。
- 不创建新的独占 Workspace。
- 不替换或关闭当前 Agent pane。

证据：打开前后终端截图或结构化 Herdr pane snapshot。

### ACC-UI-002（P0）标准两列布局

步骤：将 P4 pane 调整到标准侧边栏宽度。
期望：

- Diff 在左、CL/File 树在右。
- Diff 明显占主要宽度。
- 两列均无重叠、越界和裁切 chrome。
- footer 始终可见。

### ACC-UI-003（P0）分隔线调整

步骤：用鼠标拖动分隔线并关闭/重新打开 pane。
期望：

- 比例实时变化且保持合法最小宽度。
- 重新打开后恢复上次比例。
- 不写入项目目录。

### ACC-UI-004（P0）极窄降级

步骤：将 pane 缩窄到无法容纳两列。
期望：

- 自动进入单视图模式。
- `Tab` 可在导航和 Diff 间切换。
- 当前 CL、文件、hunk 和滚动状态不丢失。
- 恢复宽度后回到两列布局。

### ACC-UI-005（P1）终端适配矩阵

至少验证：

- Windows Terminal + PowerShell 7。
- 鼠标滚动和点击。
- Unicode box drawing。
- truecolor theme。
- 100%、125% 或常用 DPI 下无关键交互丢失。

该场景属于 Distribution Gate；Dogfood 只要求用户当前主力 Windows Terminal 环境通过。

### ACC-UI-006（P0）按键所有权

在 Herdr 宿主、P4 浏览态、评论输入框和 Submit overlay 间切换焦点。期望：

- P4 pane 未聚焦时不消费普通字母键。
- Herdr 全局/prefix 快捷键仍由宿主处理。
- 文本输入中的 `s`、`q`、`z` 作为文本输入，不触发背景动作。
- overlay 打开时背景 pane 不响应快捷键。
- `Esc` 只取消最上层输入/overlay。
- 无 modal/输入时 `q` 只关闭 P4 pane，不关闭 Workspace 或 Agent。

### ACC-UI-007（P0）Submit 快捷键不是授权

步骤：在浏览态按 `s`。
期望：仅打开 Submit review overlay，没有 P4 写命令。overlay 默认 Cancel；`Enter` 不提交，只有明确点击 Submit 或 `Ctrl+Enter` 才进入提交执行状态。

### ACC-UI-008（P0）Remembered workspace 启动恢复

步骤：在两个 Herdr workspace 中只对其中一个成功执行一次 Open Perforce review，重启 Herdr server，并再次触发一次 startup/handoff。
期望：

- 只恢复被记住且本次 session 仍存在的 workspace；不扫描其他目录、不运行 `p4`。
- 已有匹配且 process-info 确认运行 `herdr-p4 ... pane` 的健康 pane 时不重复打开。
- 只有 PowerShell/shell prompt 的同 workspace/cwd 同名 pane 视为 stale；新 pane 成功后必须二次确认仍无插件进程，再用 pane close 清理，失败不得静默忽略。
- 缺失 workspace 计为 unavailable 并安全跳过。
- 新恢复 pane 不抢焦点，且不会替换或关闭 Agent pane。
- 连接到同一 server 的新客户端、配置 reload、link/enable 不被误记为 server startup。

## 6. Workspace 识别

### ACC-P4-001（P0）正确识别当前 client

步骤：从 client view 内的嵌套目录打开 Herdr/P4 pane。
期望：

- 使用该 cwd 对应的 server/user/client/root。
- 不选择另一个可用 client。
- 若 cwd 或其祖先有 `p4config.txt` / `.p4config`（或 `P4CONFIG` 指定的文件），overlay 后的 identity 必须与该文件一致，而不是 Herdr 进程默认的 `P4CLIENT`。
- 未设置 `P4CONFIG` 时，不得因为盘符根上的 `p4config.txt` 而选中一个无关 client。
- UI 中的 identity 与只读 P4 查询一致。

### ACC-P4-002（P0）非 P4 目录

步骤：从不属于任何有效 client view 的目录打开 pane。
期望：

- 显示可操作的空状态。
- 不扫描或猜测其他 client。
- 不崩溃，不执行写命令。

### ACC-P4-003（P0）连接与认证错误分类

fixture/隔离环境分别模拟：

- `p4` 不存在。
- server unavailable。
- login expired。
- SSL trust required。
- permission denied。

期望：显示不同错误类别和安全的下一步；不得把它们显示为空 changelist。

### ACC-P4-004（P0）Windows 路径

覆盖：

- drive-letter 路径。
- 路径含空格。
- 大小写差异。
- Unicode 文件名。
- client view remap。
- 文件不在 client view。

期望：depot/client 路径映射正确，argv 不因空格或特殊字符拆分。

## 7. Changelist/File 树

### ACC-TREE-001（P0）当前 pending 列表

期望：

- 显示 default CL。
- 显示当前用户、当前 client 的 numbered pending CL。
- 不混入其他 client 的同一用户 CL。
- 空列表时仍显示 default，而不是报错。

### ACC-TREE-002（P0）展开和缓存

步骤：展开 CL、折叠、再次展开。
期望：

- 第一次异步加载 files。
- UI 加载期间可继续操作。
- 再次展开可使用仍有效的缓存。
- 手动刷新后获取最新文件列表。

### ACC-TREE-003（P0）按编号打开 CL

分别打开：

- owned pending CL。
- visible shelved CL。
- submitted CL。
- 不存在 CL。
- restricted/无权限 CL。

期望：前三种映射到正确状态；后两种给出不同诊断。

### ACC-TREE-004（P0）刷新保持选择

步骤：选择 CL/file/hunk 后刷新。
期望：对象仍存在时恢复选择和合理滚动位置；对象消失时安全选择相邻节点并解释变化。

### ACC-TREE-005（P0）文件动作

fixture 至少包含：

- add
- edit
- delete
- branch
- move/add + move/delete
- integrate
- binary

期望：图标、标签、统计和排序稳定；未知动作以 unknown 显示，不当成 edit。

## 7.1 Workspace File Explorer

Explorer 是 Dogfood Gate 能力（ADR-0005）。不替代 ACC-TREE；CL 树与目录树必须分开验收。

### ACC-EXPLORER-001（P0）client 内本地树

前置：cwd 在 client view 内。
期望：

- 显示 workspace cwd 下的目录树，不列出 Client root 之外的路径。
- 懒展开；刷新后尽量保持展开和选中。
- 不属于 client view 时不画树，显示连接说明。

### ACC-EXPLORER-002（P0）文本预览

步骤：单击文本文件。
期望：左侧显示工作区当前内容、行号；过大/超行数截断并说明原因；不得把读取失败显示成空文件。

### ACC-EXPLORER-003（P0）只读 P4 装饰

fixture 覆盖：unopened、opened edit/add/delete、out-of-date、not in view。
期望：装饰来自 P4 只读查询；查询失败时无装饰，不显示 Git status。树上不能发起 add/edit/delete/sync/revert。

### ACC-EXPLORER-004（P0）与 Review view 切换

步骤：在 Explorer 选中已 opened 文件，切到 Review（`2` 或等价 UI），再切回 Explorer（`1`）。
期望：两边选择不丢；opened 文件提供跳转到对应 CL/文件 diff 的入口。Submit overlay 只存在于 Review。

## 8. Diff

### ACC-DIFF-001（P0）Pending edit

期望：本地文件与正确 have revision 比较；unified hunk、行号和增删统计正确。

### ACC-DIFF-002（P0）Pending add/delete

期望：

- add 显示完整新增内容或明确的大小截断。
- delete 显示正确 base 内容为删除。
- 空文件不会被误判为读取失败。

### ACC-DIFF-003（P0）Shelved diff

期望：显示 shelved 内容与正确 depot revision 的差异，不错误读取当前本地 workspace 内容。

### ACC-DIFF-004（P0）Submitted diff

期望：显示该 submitted CL 引入的差异，不混入后续 depot revisions。

### ACC-DIFF-005（P0）Move

期望：可确定时关联 move/add 和 move/delete；不能确定时保留原始动作并避免虚构 rename 相似度。

### ACC-DIFF-006（P0）文本边界

golden fixtures 覆盖：

- CRLF 与 LF。
- Unicode 内容。
- 无末尾换行。
- 超长行。
- 空文件。
- 路径含空格。

期望：渲染和行号稳定；不会因 terminal width 修改真实 diff 内容。

### ACC-DIFF-007（P0）Binary 与超限

期望：binary、超过单文件预算、超过 CL 总预算分别显示明确原因；均不得伪装成“0 changes”。

### ACC-BINARY-001（P0）Binary metadata

fixture 和 Level B 只读环境至少覆盖普通 binary 与 `+l` binary。期望在权限允许时显示：

- depot/have/head revision。
- 本地和 depot/base 文件大小及变化量。
- 完整 P4 file type modifiers。
- 当前 client 的 opened/locked 状态。
- 其他 open/lock 持有者。
- move 来源/目标。

字段不可获得时必须显示 unknown/permission-limited，不能显示虚假的 revision 0、size 0 或 unlocked。

### ACC-BINARY-002（P0）Binary 内容有界

对大型 binary 计算 Submit freshness。期望使用流式 hash，内存不随完整文件大小增长；完整 binary bytes 不进入 diff cache、state 文件、日志或 snapshot。

### ACC-DIFF-008（P1）Diff 导航

期望：`[`/`]` 和 `f`/`F` 在首尾边界行为稳定；刷新后尽量保留当前 hunk。

## 9. Agent 审阅反馈

### ACC-REVIEW-001（P0）行范围选择

步骤：用键盘和鼠标分别选择单行、多行和跨可视区域范围。
期望：选择范围与发送的文件/行号/diff context 一致。

### ACC-REVIEW-002（P0）发送目标

前置：Workspace 内有多个 Agent pane。
期望：

- UI 明确显示目标 Agent。
- 发送到当前 Herdr 上下文解析出的目标。
- 不按进程名或最近输出猜测。
- 没有有效目标时禁止发送。

### ACC-REVIEW-003（P0）消息内容

期望消息包含：CL、文件、动作、行范围、用户备注和有限 diff context。特殊字符和多行内容不破坏字段边界。

### ACC-REVIEW-004（P0）失败与重试

模拟 Herdr API 拒绝/断开。
期望：备注保持 unsent；用户可明确重试；不得出现发送两次但 UI 只记录一次的静默状态。

## 10. Agent CLI 描述生成

### ACC-GEN-001（P0）可配置 argv

使用 fake Agent CLI 捕获 argv、cwd 和 stdin。
期望：

- argv 与配置逐项一致。
- 不经过 shell。
- cwd 是当前 Herdr/P4 workspace。
- Prompt 通过 stdin 完整传入。

### ACC-GEN-002（P0）Prompt 内容

期望包含：

- CL identity/status/current description。
- 完整文件动作列表。
- 预算范围内的 diff。
- 已提供的验证信息。
- 未提供验证信息时的明确声明。
- 被截断文件和原因。

不得包含 ticket、password、未授权环境变量转储或其他 workspace 的数据。

### ACC-GEN-003（P0）输出预览

Agent CLI 返回有效文本。
期望：生成文本先进入可编辑 preview overlay；关闭或 Cancel 不改变 P4 CL。

### ACC-GEN-004（P0）失败保持原描述

分别模拟：

- executable missing。
- non-zero exit。
- timeout。
- empty stdout。
- stdout 超限。
- invalid UTF-8 或不支持输出。

期望：现有描述不改变；错误分类正确；stderr 不写入 Description。

### ACC-GEN-005（P0）上下文预算稳定性

同一 CL 和同一配置重复生成 Prompt。
期望：字节内容稳定；大文件不会独占全部预算；截断清单可复现。

### ACC-GEN-006（P0）Apply 描述

仅在 Level C 环境执行。
步骤：生成、手工编辑、Apply。
期望：

- 仅 owned numbered pending CL 可用。
- Apply 前重新检查目标。
- 仅 Description 发生预期变化。
- Files、Jobs、Type、Client、User 等其他 spec 字段保持。
- P4 拒绝时显示失败且不宣称成功。

### ACC-GEN-007（P0）Stale CL

在 preview 打开后从隔离的第二客户端修改 CL。
期望：重新计算的 `spec_token` 不同，Apply 检测到 stale 状态并要求刷新，不覆盖新描述。

### ACC-GEN-008（P0）Generator 配置来源

在 workspace 根和父目录放置包含不同 generator command 的诱饵配置，包括类似 `.herdr-perforce.toml` 的文件。期望：

- 实际 executable/argv 只来自 `HERDR_PLUGIN_CONFIG_DIR`。
- 项目文件不会被读取或合并。
- 无用户级配置时 fail closed，不自动执行 PATH 中猜测的 Agent。

### ACC-GEN-009（P0）Spec token 规范化

对相同 CL 使用字段顺序不同但语义相同的 structured fixture。期望 `spec_token` 相同。改变 description、owner/client、status、保留 spec 字段或文件动作后，token 必须改变。

### ACC-GEN-010（P0）Generator 环境边界

fake Agent CLI 记录环境变量名称和去标识化值类型。期望：

- 正常用户环境仍足以启动已配置 Agent CLI。
- Herdr socket/binary path、plugin context、workspace/tab/pane/action/entrypoint identifiers 不传给子进程。
- 明文 `P4PASSWD` 不传给子进程。
- 不读取 workspace 或父目录中的 `.env`。
- 配置不能新增 secret environment values。
- 日志/UI 不输出完整 environment 或 Agent credential values。

## 11. Submit

以下场景仅允许在 Level C 隔离环境执行。

### ACC-SUBMIT-001（P0）能力门控

Submit 仅对 owned、current-client、numbered pending CL 启用。以下对象必须禁用：

- default CL。
- shelved/submitted CL。
- 其他用户 CL。
- 其他 client CL。
- restricted/不可见 CL。

### ACC-SUBMIT-002（P0）Preflight

分别验证：

- 有效可提交 CL。
- 空描述。
- 无文件。
- unresolved 文件。
- 本地文件缺失。
- 登录过期。
- CL 在 UI 读取后已改变。

期望：只有有效 CL 进入最终确认；其他情况不运行 submit。

### ACC-SUBMIT-002A（P0）Content token

打开 Submit confirmation 后分别修改一个文本文件和一个 binary 文件，但不修改 CL spec。期望：

- 文本 diff 改变使 `content_token` 改变。
- binary 内容改变使流式 hash 和 `content_token` 改变。
- 原确认失效并要求刷新。
- 不运行 `p4 submit`。

### ACC-SUBMIT-003（P0）二次确认

期望：

- 显示准确 CL 号、描述摘要、文件数量和动作统计。
- 默认焦点为 Cancel。
- Esc/关闭 pane/失焦均不提交。
- 只有在当前 overlay 显式选择 Submit 才能继续。

### ACC-SUBMIT-004（P0）成功提交

步骤：在隔离 CL 中修改已知 fixture，通过 UI 确认提交。
期望：

- 实际只调用一次目标 CL 的 submit。
- P4 Server 中出现预期 submitted change 和文件内容。
- UI 刷新为最终状态/编号。
- 没有遗留 opened file、pending CL 或 lock。

证据：执行前后只读 P4 查询、submit 输出、最终文件 revision 和清理检查。

### ACC-SUBMIT-005（P0）服务器拒绝

模拟/构造 unresolved、out-of-date 或 lock 冲突。
期望：

- UI 不显示成功。
- 不自动 resolve、unlock、revert 或重试。
- 错误信息可操作。
- 隔离环境状态与 P4 Server 实际结果一致。

### ACC-SUBMIT-006（P0）重复事件

在提交进行中重复按键、双击按钮并切换 pane。
期望：同一时刻最多一个 submit process（跨 CL 全局排他）；UI 不产生第二次提交。

### ACC-SUBMIT-007（P0）取消

在确认前取消。
期望：没有任何 P4 写命令，CL 保持不变。

### ACC-SUBMIT-008（P0）认证、权限、超时与不确定结果矩阵

用确定性 fake transport 覆盖全部单元格；标记为 write 后的场景还需在 Level C 隔离服务器复验：

| 注入点 | 故障 | UI 结果确定性 | 允许的下一步 | 禁止行为 |
|---|---|---|---|---|
| preflight/确认后重读 | authentication expired | `not-started` | `p4 login` 后显式刷新 preflight | 显示成功、自动登录、自动 submit |
| preflight/确认后重读 | permission denied/restricted CL | `not-started` | 申请权限后显式刷新 | 泄漏受限 CL 内容、自动 submit |
| preflight/确认后重读 | timeout/network unavailable | `not-started` | 恢复连接后显式刷新 | 把超时当空结果、自动 submit |
| `p4 submit -c` 返回明确错误 | authentication/permission/server rejection | `rejected` | 修复条件后重新 preflight 和确认 | resolve/unlock/revert 或自动重试 |
| `p4 submit -c` | timeout、连接中断、输出截断/无法解析 | `unknown` | 只读 reconciliation | 再次 submit、显示成功或显示确定失败 |
| write 已返回后 refresh/verify | authentication、permission、timeout、network、mapping/mismatch | `unknown` | 恢复只读能力后 reconciliation | 再次 submit、覆盖旧 receipt |
| reconciliation | 匹配的 submitted CL | confirmed success | 刷新 pane | 再次 submit |
| reconciliation | pending CL | confirmed pending；旧确认失效 | 新 preflight + 新显式确认 | 复用旧确认 |
| reconciliation | 查询失败/环境或内容不匹配 | 保持 `unknown` | 人工检查 P4 server | 再次 submit |

所有故障信息只显示分类后的可操作提示，不显示原始 server/user/client/path/ticket。重复 `r` 只能重复只读 reconciliation，不能隐式创建新的写授权。

### ACC-SUBMIT-009（P0）外部提交 provider

为要求预检查或 Review 应用的 workspace 配置 external submit provider。期望：

- Review overlay 明确显示 provider 名称，确认控件显示 Open provider，而不是宣称直接 Submit。
- 确认后仍重新验证 spec/content freshness；stale 时不启动外部工具。
- 外部工具通过直接 argv 启动，只接收配置模板替换后的 numbered CL；不得经过 shell，也不得继承 `HERDR_*` 控制变量。
- provider 启用时插件绝不运行 `p4 submit`、`p4 unlock` 或自动重试。
- 启动失败为 `not-started`；启动成功为 external handoff，不等于提交成功，并保留只读 reconciliation receipt。
- handoff 后禁止再次 Submit；只有 reconciliation 明确确认 submitted 才显示成功，确认 pending 时旧授权失效。

## 12. 配置、状态和隐私

### ACC-CONFIG-001（P0）配置热重载

步骤：运行时修改有效配置。
期望：下次刷新或操作使用新配置，无需重启 Herdr。

### ACC-CONFIG-002（P0）配置 fail-closed

分别加入未知字段、错误类型、未知 Prompt 占位符和非法 keybinding。
期望：整个新配置不生效；旧有效配置或安全默认值继续使用；修复后自动恢复。

### ACC-CONFIG-003（P0）禁止项目级可执行配置

期望：workspace、仓库、depot、当前 cwd 和 `.p4config` 相邻文件都不能覆盖 generator command、Prompt 或 keybindings。测试记录实际打开过的配置路径，除 `HERDR_PLUGIN_CONFIG_DIR` 外不得出现候选项目配置。

### ACC-STATE-001（P0）状态位置

期望：布局、展开节点和本地备注仅写入 `HERDR_PLUGIN_STATE_DIR`；项目目录无运行时杂项。

### ACC-STATE-002（P0）Pane 恢复状态

期望：

- 成功手动打开才 upsert workspace；失败打开不写状态。
- 同一 cwd 大小写差异和 Windows `\\?\` 前缀不会产生重复记录。
- 状态最多 128 个 workspace/64 KiB，只包含 cwd 和短期 Herdr id hint。
- 损坏、超限、相对路径或未知版本失败关闭，原文件不被空状态覆盖。
- `panel.json` 的 `manual` 禁止记录和恢复；缺失配置默认 `remembered`；未知字段/mode 不生效。

### ACC-PRIVACY-001（P0）秘密扫描

检查日志、fixture、snapshot、失败输出和发布包。
期望不存在：

- P4 password/ticket。
- API key。
- 真实 hostname/server address。
- 真实用户名/client/depot/project 名。
- 开发机绝对路径。

仓库 fixture 使用 `SampleProject`、`ExampleWorkspace`、`ExampleUser` 和抽象 depot path。

### ACC-PRIVACY-002（P0）命令诊断清理

构造包含敏感环境或路径的失败输出。
期望：UI 给出足够诊断，但持久日志和测试证据经过清理。

## 13. 性能与稳定性

### ACC-PERF-001（P1）大型 CL

fixture：至少 1,000 个 changed files，其中包含大文本和 binary。
期望：

- pane 首次显示不等待所有 diff 完成。
- 展开和滚动保持响应。
- diff 按需加载并受预算控制。
- 内存使用有界。

具体时间/内存阈值在性能基线建立后固化。

该场景属于 Distribution Gate。Dogfood Gate 仍要求缓存有明确配置上限和对应单元测试，但不以 1,000 文件性能数据阻塞首次日用。

### ACC-PERF-002（P1）慢 P4 Server

fixture 对命令注入延迟。
期望：有加载状态、可取消、旧结果不会覆盖较新的选择。

### ACC-PERF-003（P0）Request epoch

使用可控 fake P4，使旧 CL/file 请求晚于新选择返回。期望：

- 每个请求携带 request ID、repository generation、selection epoch 和资源 key。
- 旧结果被丢弃，不覆盖当前 selection、diff 或错误状态。
- 即使进程取消失败，结果仍不会进入 UI。

该确定性竞态测试属于 Dogfood Gate；ACC-PERF-001/002 的规模和时延基线属于 Distribution Gate。

### ACC-PERF-004（P0）有界 cache

向 metadata/diff cache 连续注入超过配置上限的数据。期望：

- metadata 按 entry 上限驱逐。
- diff 按实际 bytes 使用 LRU 驱逐。
- binary 内容不缓存。
- workspace generation 改变后不复用旧 client 数据。
- 进程内存不会因会话持续刷新而无界增长。

使用默认配置时还要验证 metadata 4,096 entries、diff 总计 64 MiB/单 entry 8 MiB、binary hash 4,096 records 的边界。使用较小测试配置验证驱逐顺序，不要求测试 fixture 实际分配完整默认预算。

### ACC-PERF-005（P0）并发上限

fake executables 阻塞并记录同时运行数量。期望：只读 P4 最多 4 个并发进程，generator 最多 1 个，submit 最多 1 个；SubmitRunning 期间重复事件不能创建第二个写进程。

### ACC-STABILITY-001（P0）Pane 生命周期

重复执行 20 次打开、关闭、调整宽度和刷新。
期望：无 orphan child process、terminal mode 损坏、鼠标模式泄漏或 Agent pane 终止。

### ACC-STABILITY-002（P0）进程取消

关闭 pane 时存在进行中的只读 P4 查询或 Agent generation。
期望：安全取消或回收进程；不得因为 pane 关闭而启动写操作。

Submit 已经获得显式确认并开始后的关闭策略必须在实现时单独定义和测试，不能简单 kill 后宣称未提交。

## 14. 发布验收

### ACC-RELEASE-001（P0）本地开发安装

步骤：从 checkout 构建并 `herdr plugin link`。
期望：action 可发现，pane 可打开，重新构建后重开 pane 使用新 binary。

### ACC-RELEASE-002（P0）干净安装

步骤：从发布产物或 GitHub plugin install 在无 Rust toolchain 的干净 Windows 环境安装。
期望：

- 安装预览准确。
- 使用匹配平台的预编译 binary。
- checksum/版本验证成功。
- 不要求 Bash。

该场景属于 Distribution Gate，不阻塞 Dogfood Gate；未通过时不得宣称已具备公开安装体验。

### ACC-RELEASE-003（P0）卸载

期望：插件注册和受管 binary 被移除；用户配置/状态的保留策略与文档一致；不删除项目或 P4 workspace 文件。

## 15. Gate 完成定义

### 15.1 Dogfood 完成

只有同时满足以下条件，版本才可标记为 dogfood-ready：

- Dogfood Gate 所列 P0 的离线自动化通过。
- Windows release build、lint、test 和 manifest 校验通过。
- Herdr 中的真实右侧 pane、两列布局、极窄降级和 Agent 消息闭环通过。
- 至少一次 Level B 只读真实 P4 验证通过，或明确记录为发布阻断缺口。
- Agent description generation 和失败矩阵通过。
- 在 Level C 隔离 P4 环境完成一次 Description Apply 和一次 numbered CL submit。
- 写入验收后证明无意外 CL、opened file 或 lock 残留。
- 隐私扫描通过。
- 内部用户文档说明本地安装、配置、快捷键、写操作确认和已知限制。

### 15.2 Distribution 完成

只有同时满足以下条件，版本才可标记为 distribution-ready：

- 同一提交上的 Dogfood Gate 完整通过。
- ACC-UI-005 和 ACC-PERF Distribution 场景通过并记录基线。
- ACC-RELEASE-002/003 通过。
- 在无 Rust toolchain 的干净 Windows 环境完成安装、升级和卸载闭环。
- 发布 artifact、fixture、snapshot、日志和公共文档的隐私扫描通过。
- 外部用户文档说明安装、升级、卸载、信任边界、Agent 配置和故障排查。

以下证据不能替代缺失的真实验收：

- parser 单测不能替代真实 P4 输出验证。
- Herdr 外单独运行 TUI 不能替代 Herdr pane 验收。
- dry-run/fake submit 不能替代隔离 P4 Server 的 submit closure。
- 较早版本的成功结果不能替代最终代码的重新验证。

## 16. 首版之后的候选验收域

以下内容不属于当前首版：

- P4 Code Review/Swarm。
- shelve/unshelve。
- resolve 工作流。
- reopen/move between CL。
- sync/reconcile/edit/add/delete/revert。
- stream/integration 可视化。
- binary asset preview。
- 多 server/client 聚合。

新增这些能力时必须各自增加写入门控、隔离验收和残留状态检查，不能复用 Submit 的一次确认作为通用授权。
