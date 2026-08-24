# Herdr Perforce 侧边栏设计

状态：首版设计基线
目标平台：Windows 优先，保留 macOS/Linux 可移植性
目标宿主：Herdr 0.8.2 或更高版本
暂定仓库名：`herdr-perforce`
暂定二进制名：`herdr-p4`

## 1. 产品定义

`herdr-perforce` 是 Herdr 右侧工具区域中的紧凑 P4 审阅面板。它在不遮挡 Agent CLI 主工作区的前提下，提供以下闭环：

1. 查看当前 P4 workspace 的 changelist。
2. 展开 changelist 并选择其中的文件。
3. 在同一侧边栏中查看文件 diff。
4. 将选中行或审阅意见发送给当前 Herdr Agent。
5. 使用可配置的 Agent CLI one-shot 命令生成 changelist 描述。
6. 人工检查、编辑并应用生成的描述。
7. 经过预检和二次确认后提交指定的 numbered pending changelist。

它不是一个独占 Workspace 的 P4V 替代品，也不是一个占据全屏的三列工作台。

## 2. 已确认的产品决策

| 决策 | 结论 | 主要理由/否决方案 |
|---|---|---|
| 宿主形态 | Herdr 右侧可切换 plugin pane | 保留 Agent CLI 主区域；否决独占 Workspace |
| 页面占用 | 不创建独占 Workspace，不替换 Agent CLI 主区域 | P4 是辅助 Agent 的工具，不是 P4V 替代品 |
| 面板布局 | 左侧 Diff，右侧 Changelist/File 树 | Diff 靠近 Agent CLI；完整理由见 [ADR-0001](adr/0001-right-sidebar-layout.md) |
| 默认比例 | Diff 约 70%，导航约 30%，允许拖动 | 审阅内容优先；极窄时改为单视图而非继续压缩 |
| 首版 SCM | 原生 P4 changelist，不依赖 P4 Code Review/Swarm | 避免把可选服务器产品变成基础依赖 |
| 实现语言 | 独立 Rust 项目 | 适合单 binary、Windows TUI 和有界并发；否决清理 Git 耦合的直接 fork |
| P4 接口 | 调用用户现有的 `p4` CLI，不引入 P4API.NET | 复用现有 ticket/trust/config，减少 native SDK 发布依赖 |
| 一致性 | 双 freshness token、request epoch 和有界 cache | 不能用单个时间戳覆盖 pending 本地内容；见 [ADR-0002](adr/0002-consistency-and-async-invalidation.md) |
| 首版写操作 | 更新 pending CL 描述；提交指定 numbered pending CL | 满足日用闭环，其余写操作继续保持非目标 |
| Agent 描述 | 可配置 Agent CLI、argv 和 Prompt 的 one-shot 生成器 | 支持不同 Agent；配置只能来自受信任用户目录，见 [ADR-0004](adr/0004-agent-generator-trust-boundary.md) |
| 自动化边界 | 不允许自动提交；所有写操作均需要显式人工确认 | 单键只能打开确认 UI，不能构成写入授权；见 [ADR-0003](adr/0003-key-ownership-and-destructive-actions.md) |

## 3. 宿主布局

Herdr 的整体布局保持不变：

```text
┌────────────┬──────────────────────────┬──────────────────────────┐
│ Herdr 左栏  │ Agent CLI                │ 右侧工具面板              │
│            │                          │                          │
│ Spaces     │ Codex / Claude / Shell   │ P4 Changelist            │
│ Workspaces │                          │ Files / Browser / ...     │
│ Agents     │                          │                          │
│ Threads    │                          │                          │
└────────────┴──────────────────────────┴──────────────────────────┘
```

P4 是右侧工具面板中的一种工具。用户可以打开、关闭或切换该面板，面板关闭后不应终止 Agent CLI，也不应改变当前 P4 workspace。

## 4. P4 面板布局

### 4.1 标准宽度

```text
+--------------------------------------+----------------------+
| Diff                                 | Changelists          |
+--------------------------------------+----------------------+
| Foo.cpp                              | v CL 123456 pending  |
| @@ -42,7 +42,9 @@                    |   M Foo.cpp          |
|                                      |   A Bar.cpp          |
|  42  fn update() {                   |   D Old.cpp          |
|- 43      old();                      | > CL 123450 shelved  |
|+ 43      new();                      | > default            |
|  44  }                               |                      |
+--------------------------------------+----------------------+
| CL 123456 / Foo.cpp / edit / +2 -1 / ?                      |
+-------------------------------------------------------------+
```

- Diff 位于左侧，占主要宽度。
- Changelist/File 树位于右侧。
- 分隔线可通过鼠标拖动。
- 面板尺寸变化后，比例在合法范围内保持。
- 用户调整后的比例保存在插件状态目录，而不是项目目录。

### 4.2 中等宽度

- 导航列缩窄。
- 文件路径优先显示 basename；完整路径通过 tooltip、状态栏或详情 overlay 查看。
- 隐藏非关键统计，不隐藏文件动作和选中状态。
- Diff 默认软换行；用户可以切换为水平滚动。

### 4.3 极窄宽度

当两列均无法保持可用最小宽度时：

- 一次只显示 Diff 或 Changelist/File 树。
- `Tab` 在两者之间切换。
- `z` 隐藏或恢复导航，让 Diff 临时占满插件 pane。
- 当前 CL、文件和模式必须保留，切换不能重置选择。

具体宽度阈值由实现期通过终端快照测试确定，不把某个固定列数写入产品契约。

## 5. Changelist/File 树

### 5.1 树结构

```text
▾ 123456  pending
  M Source/Foo.cpp
  A Source/Bar.cpp
  D Source/Old.cpp

▸ 123450  shelved
▸ 123420  submitted
▸ default
```

首版自动列出：

- 当前 client 的 default changelist。
- 当前用户、当前 client 的 numbered pending changelist。
- 当前会话中通过 CL 号直接打开过的 shelved 或 submitted changelist。

首版不自动扫描所有用户或所有 client 的全局 changelist。用户可通过“打开 CL”命令输入编号，按权限读取该 CL。

### 5.2 节点行为

- 单击 CL：选择 CL，并在左侧显示紧凑概览。
- 展开 CL：异步加载文件列表。
- 折叠 CL：不丢弃已加载数据；下次刷新发现版本变化时再失效。
- 单击文件：在左侧显示该文件 diff。
- 刷新后尽量维持 CL、文件、hunk 和滚动位置。
- 被删除、无权限或已经提交的选中 CL 应显示明确状态，并安全选择相邻节点。

### 5.3 文件状态

首版至少识别：

- `add`
- `edit`
- `delete`
- `branch`
- `move/add`
- `move/delete`
- `integrate`
- binary/非文本类型

移动文件应尽量成对呈现；无法确定配对时保留两个原始动作，不猜测关系。

## 6. 左侧内容区

### 6.1 CL 概览

选择 CL、尚未选择文件时显示：

```text
CL 123456
Pending · 5 files · +120 -34

Description
Fix pooled entity lifecycle during reuse

Files
3 modified · 1 added · 1 deleted

[Generate Description]   [Submit…]
```

概览包含：

- CL 号或 `default`
- pending、shelved、submitted 状态
- owner 和 client（受权限和 UI 宽度限制）
- 描述
- 文件动作统计
- 文本 diff 增删统计；无法计算时显示 unknown，而不是显示 0
- 当前可用操作

### 6.2 文件 Diff

选择文件后显示 unified diff：

- 行号、hunk header、增加/删除样式。
- 语法高亮。
- 当前 hunk 和当前选中行范围。
- add/delete/move/binary 的显式状态。
- diff 太大时显示截断原因和继续加载入口。
- P4 无权限、文件不在 client view 或本地文件缺失时，显示对应诊断。

不得把以下情况混为“空 diff”：

- 文件内容没有变化。
- P4 命令失败。
- 没有权限。
- binary 文件不支持文本 diff。
- diff 被大小预算截断。
- pending 文件不属于当前 client，无法读取本地内容。

### 6.3 Binary 文件详情

Binary 文件不伪装成文本 diff，也不能只显示一句“binary”。左侧内容区改为紧凑 metadata card，至少显示权限允许获得的：

- depot、have 和 head revision。
- 本地文件大小，以及可获得时的 base/head 文件大小和变化量。
- P4 file type modifiers，包括游戏项目常见的 `+l` 独占锁定类型。
- 当前文件是否由本 client opened/locked。
- 权限允许时的其他 open/lock 持有者。
- move 来源/目标和 changelist 状态。

无法获得锁持有者、大小或 revision 时显示 unknown/permission-limited，不显示虚假的未锁定或 0。首版仍不解析 `.uasset`、`.umap`、贴图或其他资产内容。

## 7. 输入与操作

默认快捷键候选如下。它们只在 P4 pane 获得焦点且没有更高优先级的输入/overlay 时生效；完整所有权和破坏性操作协议见 [ADR-0003](adr/0003-key-ownership-and-destructive-actions.md)。

| 键 | 行为 |
|---|---|
| `j` / `k`、方向键 | 移动选择或滚动 |
| `Left` / `Right` | 折叠或展开 CL |
| `Enter` | 选择节点或文件 |
| `[` / `]` | 上一个/下一个 hunk |
| `f` / `F` | 下一个/上一个文件 |
| `v` | 开始或结束 diff 行选择 |
| `c` | 为选中行或范围编写审阅备注 |
| `a` | 将待发送备注发送给当前 Agent |
| `g` | 生成 CL 描述 |
| `s` | 打开当前 CL 的 Submit review overlay；不直接提交 |
| `o` | 按编号打开 CL |
| `/` | 搜索 CL 或文件 |
| `r` | 刷新 |
| `w` | 切换软换行 |
| `z` | 隐藏或恢复导航 |
| `Tab` | 切换 Diff/导航焦点；极窄模式下切换视图 |
| `?` | 打开帮助 overlay |
| `q` | 关闭 P4 pane，不退出 Herdr |

所有键位最终都应可配置。鼠标应支持节点选择、展开、滚动、diff 行选择和分隔线拖动。

按键路由优先级为 Herdr 宿主保留键、当前 overlay/文本输入、获得焦点的 P4 pane。Submit overlay 默认 Cancel，最终提交只能通过明确点击 Submit 或 `Ctrl+Enter`；`Enter` 不构成最终提交授权。

## 8. Agent 审阅反馈

用户可选择 diff 行或范围并创建一条本地审阅备注。备注在发送前只存放于插件状态中。

发送给当前 Herdr Agent 的内容应包含：

```text
Perforce review note
CL: 123456
File: Source/Foo.cpp
Action: edit
Lines: 42-48

Comment:
这里在 pooled reuse 后可能保留旧 owner。

Diff context:
@@ -40,8 +40,10 @@
...
```

要求：

- 发送目标必须由当前 Herdr 上下文解析，不凭进程名猜测。
- 发送前展示目标 Agent；没有可用 Agent 时禁用发送并说明原因。
- 一个备注只在 Herdr API 确认接收后标记为 sent。
- 插件不自动把备注发布到 P4 Code Review，也不修改文件。
- 路径和 diff 片段按需要转义，不能让结构边界含糊。

## 9. Agent CLI One-shot 描述生成

### 9.1 数据流

```text
P4 CLI
  │
  ▼
CL metadata + file actions + normalized diff
  │
  ▼
Prompt template
  │
  ▼
Configured Agent CLI one-shot process
  │
  ▼
Preview/Edit overlay
  │
  ▼ explicit Apply
p4 change -i
```

插件负责先获取 CL 的权威数据。Agent CLI 的主要职责是根据已提供上下文生成描述，不要求它自行查询 P4。

### 9.2 配置草案

配置文件只允许位于 `HERDR_PLUGIN_CONFIG_DIR`。插件绝不从 workspace、仓库、depot、当前 cwd、`.p4config` 相邻文件或项目目录读取/合并 generator command、Prompt 或 keybindings；完整信任边界见 [ADR-0004](adr/0004-agent-generator-trust-boundary.md)。

```toml
[description_generator]
command = ["codex", "exec", "-"]
input = "stdin"
output = "text"
timeout_seconds = 120
max_prompt_bytes = 262144
max_output_bytes = 32768

prompt = """
为以下 Perforce changelist 生成简洁、具体的提交描述。
说明目的、主要改动和明确提供的验证信息。
不要虚构测试结果，不要输出解释或 Markdown 代码块。

{change_context}
"""
```

首版配置契约：

- `command` 是 argv 数组，不通过 shell。
- 工作目录是 P4 client 中当前 Herdr workspace 的目录。
- `input = "stdin"` 是首选输入方式。
- `output = "text"` 读取 stdout 并去除首尾空白。
- 非零退出、超时、空输出和超限输出均为失败，不能覆盖现有描述。
- stderr 可用于诊断，但不混入生成描述。
- Prompt 仅支持定义好的占位符；未知占位符使配置整体无效。
- 插件显示实际将调用的可执行文件和参数，但不得显示凭据。

未来可增加 JSON 输出和多个 generator profile，但不属于首版完成条件。

### 9.3 上下文预算

`change_context` 包含：

- CL 号、状态和现有描述。
- 文件动作和 depot/client 相对路径。
- 规范化文本 diff。
- 已知的验证信息；没有则明确写“未提供验证信息”。
- 截断清单，包括未包含的文件和原因。

预算策略必须稳定且可测试。优先保留 CL 元数据、文件列表和每个文件的开头 diff，避免单个大文件吞掉全部上下文。

### 9.4 预览与应用

- 生成结果在 overlay 中预览并可编辑。
- 用户选择 Apply 后，再次确认目标 CL 仍为同一个 pending CL。
- 使用 `p4 change -o` 读取完整 spec，仅替换 Description，再通过 `p4 change -i` 写回。
- 其他 spec 字段必须逐字保留，除 P4 自己的规范化外不得改变。
- 生成成功不等于 Apply 成功；两者分别显示结果。
- submitted CL、shelved-only CL、default CL 的 Apply 能力按 P4 规则显式禁用或单独设计；首版仅保证 owned numbered pending CL。

## 10. 提交 numbered pending CL

### 10.1 前置条件

首版 Submit 仅对以下对象启用：

- 编号 changelist。
- 状态为 pending。
- owner 是当前 P4 用户。
- client 是当前 P4 client。
- 包含至少一个可提交文件或可提交 stream spec。
- 描述非空且不是默认占位文本。

### 10.2 Preflight

提交前必须重新查询服务器，不使用可能过期的 UI 缓存。Preflight 至少检查：

- CL 仍存在且仍为 pending。
- owner/client 未改变。
- 文件列表和动作统计。
- unresolved 或需要 resolve 的状态。
- 本地文件缺失、无权限、登录过期和连接错误。
- 当前选中 CL 与即将执行的 CL 完全一致。

插件不宣称能够提前发现 P4 Server 在 submit 时才会报告的所有问题。

### 10.3 二次确认

```text
┌──────── Submit CL 123456 ────────┐
│                                  │
│ 5 files · +120 -34               │
│                                  │
│ Fix pooled entity lifecycle...   │
│                                  │
│ Preflight                        │
│ ✓ Description                    │
│ ✓ Ownership                      │
│ ✓ No unresolved files            │
│                                  │
│       [Cancel]  [Submit]         │
└──────────────────────────────────┘
```

- 默认焦点是 Cancel。
- 确认框明确显示 CL 号、描述摘要、文件数量和 preflight 结果。
- 用户必须在当前确认框中显式选择 Submit。
- Agent 消息、启动 hook、刷新和快捷键重复事件都不能绕过确认。

### 10.4 执行与结果

- 执行 `p4 submit -c <change>`，不经过 shell。
- submit 期间显示进行中状态并阻止重复提交同一个 CL。
- 用户关闭 pane 不应悄悄启动第二次提交。
- 成功后重新查询 CL，因为 P4 可能返回不同的最终编号或状态。
- 失败时显示分类后的诊断和经过清理的命令输出。
- 不自动执行 resolve、revert、unlock、reopen 或重试 submit。

## 11. P4 数据层

### 11.1 命令执行

`P4Executor` 统一负责：

- argv 调用。
- cwd 和环境。
- 超时与取消。
- stdout/stderr 字节预算。
- exit code。
- P4 structured error records。
- 敏感字段清理。

结构化查询优先使用 `p4 -ztag -Mj`，每行按独立 JSON record 解析。Diff 使用独立的文本命令和 parser，避免 structured record 与 patch 文本混合。

### 11.2 Workspace 解析

每次 pane 打开时，从 Herdr plugin context 得到 workspace cwd，再使用 P4 查询确定：

- server identity
- user
- client
- client root
- stream（如果存在）
- case handling
- client view/path mapping

若 cwd 不属于有效 P4 client view，面板显示连接说明，不回退到任意其他 client。

### 11.3 Diff 来源

| CL 类型 | 数据策略 |
|---|---|
| default/numbered pending | `opened`/`fstat` 元数据，加本地 workspace 与 have revision 的 diff 组装 |
| shelved | shelved describe/diff，与 depot revision 比较 |
| submitted | submitted describe/diff |

Diff assembler 必须覆盖 add、delete、move、binary、空文件、无末尾换行、CRLF 和 Unicode。任何 fallback 都必须在模型中保留其来源和不确定性。

### 11.4 领域模型草案

```text
WorkspaceIdentity
  server_id
  user
  client
  root
  stream?
  case_handling

Changelist
  id: Default | Numbered(u64)
  status: Pending | Shelved | Submitted
  owner
  client
  description
  files[]
  spec_token
  content_token?

ChangedFile
  depot_path
  client_path?
  action
  file_type
  base_revision?
  moved_from?
  moved_to?

FileDiff
  source
  hunks[]
  stats?
  truncation?
  binary
  diagnostic?
```

实现中将 `freshness_token` 明确拆成：

- `spec_token`：规范化 workspace/CL spec/文件动作元数据的 BLAKE3。
- `content_token`：文本 diff/add/delete 内容以及 binary 流式内容 hash 组成的 BLAKE3。

Description Apply 前重新查询并比较 `spec_token`；Submit confirmation 前同时比较两者。任一变化都使现有确认失效并要求刷新。Token 不是服务器事务锁，最终 submit 仍以 P4 Server 的原子结果为准。规范化输入和 cache 行为见 [ADR-0002](adr/0002-consistency-and-async-invalidation.md)。

### 11.5 异步请求与缓存失效

- workspace identity 改变或全局刷新时递增 `repository_generation`。
- CL/file 选择改变时递增 `selection_epoch`。
- 每个异步请求携带 request ID、generation、epoch 和资源 key。
- 只有仍与当前状态全部匹配的结果才能写入可见 UI。
- 取消旧进程用于节省资源；即使取消失败，epoch 校验也会丢弃其结果。
- metadata cache 按 entry 数限制，diff cache 按 bytes 使用 LRU。
- binary 内容不缓存，只缓存 metadata 和 content hash。
- workspace generation 是 cache key 的一部分，禁止跨 client 复用。

首版默认 metadata 上限为 4,096 entries，diff cache 总上限为 64 MiB、单 entry 上限为 8 MiB，binary hash records 上限为 4,096。只读 P4 查询默认最多 4 个并发子进程，generator 和 submit 各自单飞；Submit 进入排他状态。预算可以由用户配置调整，但不允许无上限值。实现期必须将选择状态、请求状态和缓存状态分开，不能用单个全局 loading flag 表示所有并发工作。

## 12. Herdr 集成

插件包含：

- 一个打开/切换 P4 pane 的 action。
- 一个右侧 terminal pane entrypoint。
- 可选的新 workspace 自动打开行为，默认关闭。

运行时通过 Herdr 提供的 plugin environment 获取：

- plugin root
- config directory
- state directory
- workspace/tab/pane context
- Herdr binary/socket path

插件应优先通过 `HERDR_BIN_PATH` 或 socket API 与正在运行的 Herdr 通信，兼容 Unix socket 和 Windows named pipe。

首版不依赖 Herdr 的 Git worktree provenance；P4 workspace identity 完全由当前 cwd 和 P4 查询决定。

## 13. 配置与状态

### 13.1 配置目录

用户可编辑配置只放在 `HERDR_PLUGIN_CONFIG_DIR`：

- Agent description generator。
- keybindings。
- theme。
- diff wrap。
- refresh interval。
- navigator width。
- diff 和 Prompt 大小预算。

无效配置整体拒绝，并在 pane 内显示可恢复错误；修复文件后无需重启 Herdr 即可恢复。

项目目录级配置被明确禁止，尤其不能允许仓库文件覆盖 `description_generator.command`。仓库可以提供文档中的配置示例，但用户必须主动复制到自己的 Herdr plugin config directory 后才会生效。

### 13.2 状态目录

本地运行状态放在 `HERDR_PLUGIN_STATE_DIR`：

- 展开的 CL。
- 最近打开的 CL 编号。
- 当前布局比例。
- 未发送的本地审阅备注。
- 非敏感 UI 偏好。

不得保存：

- P4 password。
- ticket 内容。
- Agent API key。
- 完整的长期 diff 缓存。
- 未经用户同意的 CLI transcript。

缓存还必须遵守 [ADR-0002](adr/0002-consistency-and-async-invalidation.md) 的 entry/byte 上限；“不长期保存”不能替代运行时内存上限。

## 14. 安全边界

- 插件与 Agent CLI 都以当前用户权限运行，Herdr 不提供插件沙箱。
- 安装预览应清楚展示 build 和 runtime 命令。
- 所有子进程使用 argv，不拼接 shell 命令。
- 日志清理 P4PORT host、用户名、client 名、绝对路径、ticket 和环境秘密；UI 可以按需显示当前用户自己的上下文，但测试 fixture 和提交到仓库的证据必须去标识化。
- 描述生成器是用户信任的本地可执行程序；插件只能限制输入、超时和输出，不能把它描述为安全沙箱。
- Apply Description 和 Submit 是两个独立写操作，各自需要显式确认。
- 首版绝不自动执行 submit、resolve、revert、sync、shelve 或 unshelve。
- generator 默认继承用户环境以支持 Agent 认证，但 spawn 前移除 Herdr socket/control context 和明文 `P4PASSWD`；首版不读取 `.env`，不允许 generator 配置注入新的 secret environment values。该清理不构成进程沙箱。

## 15. 错误分类

至少区分：

- P4 executable missing。
- cwd 不属于 P4 client view。
- network/server unavailable。
- authentication expired。
- trust required。
- permission denied/restricted CL。
- malformed or unsupported P4 output。
- local file missing。
- diff unavailable/binary/truncated。
- Agent CLI missing、timeout、non-zero、invalid output。
- Herdr context/target Agent unavailable。
- stale CL before Apply/Submit。
- submit rejected by server。

错误信息必须给出下一步，但不能建议自动执行有破坏性的修复。

## 16. 首版非目标

- P4 Code Review/Swarm 评论、投票、review state。
- P4V 的完整替代。
- sync、reconcile、edit、add、delete、reopen。
- shelve、unshelve、revert、resolve。
- stream graph 或 integration 工作台。
- default changelist submit。
- 选择部分文件 submit。
- Agent 自主提交。
- 二进制资产内容预览。
- 多 server 聚合视图。
- 移动端专用 UI。

## 17. 推荐模块边界

```text
src/
  app/          UI state and commands
  tui/          rendering, input, overlays
  domain/       changelist, file, diff, review models
  p4/           executor, structured parser, repository, diff assembler
  herdr/        context and agent messaging
  generator/    one-shot Agent CLI runner and prompt building
  submit/       preflight, confirmation model, submit result
  config/       config loading and validation
  state/        non-sensitive persisted UI state
```

领域层不依赖 Ratatui、进程实现或 Herdr socket，使 parser、preflight 和 diff assembler 可以通过 fixture 完整测试。

## 18. 参考项目与文档

- Herdr plugin contract：<https://herdr.dev/docs/plugins/>
- Herdr socket API：<https://herdr.dev/docs/socket-api/>
- Architecture Decision Records：[adr/README.md](adr/README.md)
- `herdr-reviewr`：<https://github.com/persiyanov/herdr-reviewr>
- `herdr-co-review`：<https://github.com/elKei24/herdr-co-review>
- `p4-diff`：<https://github.com/JonParr/p4-diff>
- Perforce P4VS：<https://github.com/perforce/P4VS>
- Perforce `p4 describe`：<https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/p4_describe.html>
- Perforce `p4 change`：<https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/p4_change.html>
- Perforce `p4 submit`：<https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/p4_submit.html>
