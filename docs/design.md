# Herdr Perforce 侧边栏设计

状态：首版设计基线
目标平台：Windows 优先，保留 macOS/Linux 可移植性
目标宿主：Herdr 0.8.2 或更高版本
暂定仓库名：`herdr-perforce`
暂定二进制名：`herdr-p4`
测试环境：[testing.md](testing.md)

## 1. 产品定义

`herdr-perforce` 是 Herdr 右侧工具区域中的紧凑 P4 审阅面板。它在不遮挡 Agent CLI 主工作区的前提下，提供以下闭环：

1. 浏览当前 P4 client 本地目录树并预览文件（本插件内独立实现，不依赖社区 File Explorer）。
2. 查看当前 P4 workspace 的 changelist。
3. 展开 changelist 并选择其中的文件。
4. 在 Agent CLI 与导航树之间的独立内容 pane 查看文件或 diff。
5. 将选中行或审阅意见发送给当前 Herdr Agent。
6. 使用可配置的 Agent CLI one-shot 命令生成 changelist 描述。
7. 人工检查、编辑并应用生成的描述。
8. 经过预检和二次确认后提交指定的 numbered pending changelist。

它不是一个独占 Workspace 的 P4V 替代品，也不会永久占据三列。CL 文件树 ≠ 工作区 File Explorer，两者都在本插件的最右导航 pane 内，但是两个 view；内容 pane 只在需要阅读 File、Diff 或 CL 文件列表时出现。

## 2. 已确认的产品决策

| 决策 | 结论 | 主要理由/否决方案 |
|---|---|---|
| 宿主形态 | Herdr 右侧可切换 plugin pane | 保留 Agent CLI 主区域；否决独占 Workspace |
| 页面占用 | 不创建独占 Workspace，不替换 Agent CLI 主区域 | P4 是辅助 Agent 的工具，不是 P4V 替代品 |
| 面板布局 | Agent CLI → Content → 最右导航；Content 按需出现 | 长文本与树尺寸解耦；见 [ADR-0006](adr/0006-standalone-content-pane.md) |
| 工作区 Explorer | 最右导航 pane 的独立 view：本地树 + 只读 P4 装饰 | P4 与 Git 宿主互斥，不能依赖社区插件；见 [ADR-0005](adr/0005-in-plugin-file-explorer.md) |
| 默认比例 | 无内容时 80/20；有内容时 Agent 40%、Content 40%、导航 20% | Agent 与正文同宽，树保持窄列 |
| 首版 SCM | 原生 P4 changelist，不依赖 P4 Code Review/Swarm | 避免把可选服务器产品变成基础依赖 |
| 实现语言 | 独立 Rust 项目 | 适合单 binary、Windows TUI 和有界并发；否决清理 Git 耦合的直接 fork |
| P4 接口 | 调用用户现有的 `p4` CLI，不引入 P4API.NET | 复用现有 ticket/trust/config，减少 native SDK 发布依赖 |
| 一致性 | 双 freshness token、request epoch 和有界 cache | 不能用单个时间戳覆盖 pending 本地内容；见 [ADR-0002](adr/0002-consistency-and-async-invalidation.md) |
| 首版写操作 | 更新 pending CL 描述；提交指定 numbered pending CL | 满足日用闭环，其余写操作继续保持非目标 |
| Agent 描述 | 可配置 Agent CLI、argv 和 Prompt 的 one-shot 生成器 | 支持不同 Agent；配置只能来自受信任用户目录，见 [ADR-0004](adr/0004-agent-generator-trust-boundary.md) |
| 自动化边界 | 不允许自动提交；所有写操作均需要显式人工确认 | 单键只能打开确认 UI，不能构成写入授权；见 [ADR-0003](adr/0003-key-ownership-and-destructive-actions.md) |
| Pane 生命周期 | 首次手动打开后按 workspace cwd 记忆，Herdr server startup 时幂等恢复 | 不修改 Herdr 全局快捷键；不在启动阶段扫描目录或探测 P4；缺失 workspace 安全跳过 |

## 3. 宿主布局

没有打开具体内容时：

```text
┌────────────┬────────────────────────────────────────┬──────────────┐
│ Herdr 左栏  │ Agent CLI                              │ P4 Navigation│
│            │ Codex / Claude / Shell                 │ Explorer/CL  │
└────────────┴────────────────────────────────────────┴──────────────┘
```

选择 File、Diff 或 CL 后：

```text
┌────────────┬───────────────────┬───────────────────┬──────────────┐
│ Herdr 左栏  │ Agent CLI         │ P4 Content        │ P4 Navigation│
│            │                   │ File / Diff / CL  │ Explorer/CL  │
└────────────┴───────────────────┴───────────────────┴──────────────┘
```

导航 pane 内用 `1` / `2` 在 **Explorer** 与 **Review** 之间切换。不依赖社区 Files/Git pane。用户关闭本面板后不应终止 Agent CLI，也不应改变当前 P4 workspace。非 P4 workspace 不自动打开本插件。

## 4. P4 面板布局

### 4.1 导航 pane

同一 plugin pane 两个 view，不拆成两个 Herdr 插件：

| View | 布局 | 职责 |
|---|---|---|
| Explorer | 本地目录树 | 浏览 client 内未 opened 的文件；只读 P4 装饰；Enter 打开 File，`d` 打开 opened-file Diff |
| Review | CL 树 | Enter 在 Content pane 打开 CL 文件列表；保留生成描述和 Submit 入口 |

切换 view 不得重置当前 CL/文件选择，也不得重置 Explorer 的展开和滚动。`1` / `2` 只切换 Explorer 与 Review；Content pane 的滚动和返回栈独立于导航。

### 4.2 Content pane

- File：显示行号和按扩展名/首行选择的语法高亮。
- Diff：以当前文件为画布的内联叠加；增/删/行内修改用不同颜色和 gutter 符号；远距未改可折叠。
- CL：显示描述和文件列表，Enter 下钻 Diff，`Esc` 返回。
- File、Diff 和 CL 长行始终按 Content pane 当前宽度自动换行；文件行号位于固定 gutter，续行使用等宽空白 gutter，使正文保持左对齐。`↑` / `↓`、`PageUp` / `PageDown` 和鼠标滚轮按换行后的显示行滚动。

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

### 5.4 与工作区 Explorer 的区别

本节的树是 **changelist → opened/described 文件**，数据来自 `p4 opened` / `describe`，不是磁盘目录。不能把 depot 路径拼成假文件夹来冒充 Explorer。工作区目录树见 §5.5。

### 5.5 Workspace File Explorer

完整决策见 [ADR-0005](adr/0005-in-plugin-file-explorer.md)。

Explorer 根：

- 默认是当前 Herdr workspace cwd。
- 不得列出 Client root 之外、或不在 client view 内的路径。
- cwd 不属于 client view 时不画树，显示与 §11.2 相同的连接说明。

树行为：

- 懒展开本地目录；遵守常见忽略（如 `.git` 目录可显示但默认折叠策略由实现决定，不读取 Git status）。
- 装饰只读，来自对该路径的 `p4 fstat`/`opened`/`have`：unopened、opened（及 action）、out-of-date、not in view、unmapped。查询失败时装饰为空，不假装是 Git。
- 单击文件：在中间 Content pane 预览 **工作区当前内容**（文本 + 行号 + 语法高亮）。binary 用与 §6.3 同类的 metadata card，不解析资产内容。
- 若该文件已 opened：提供在同一 Content pane 查看对应 Diff 的入口，不在 Explorer 里再画一份 submit UI。
- 双击或 “Open with default app” 可交给 OS；首版不在树里执行 `p4 add/edit/delete/sync/revert`。

预览预算与 Review diff 类似：过大/超行数显示截断原因。刷新后尽量保持展开、选中和滚动。

## 6. 中间 Content pane

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

选择文件后显示 **整文件内联叠加**（见 [ADR-0007](adr/0007-inline-overlay-diff.md)），而不是 `p4 diff -du` 原文：

- 基底是工作区当前文件；未改行与 File 预览相同（行号 + 语法高亮）。
- 删除行插在改动位置（红底、gutter `-` 与旧行号）；新增行就地显示（绿底、gutter `+` 与新行号）。
- 配对的删/增行再做词级高亮，只标出真正变化的 token。
- 工具栏：`Prev` / `Next` 跳到上一/下一处改动；有折叠时提供 `Expand all` / `Fold unchanged`。
- `[` / `]` 与工具栏 hunk 按钮等价；`e` 展开或收起全部折叠。
- 远距未改默认折叠，每侧保留 `diff_fold_context` 行（默认 5；`0` 表示不折叠）。隐藏不足 4 行时不折叠。折叠处画 `⋯` 分隔，并提供 `[▼20]` / `[▲20]`；点击按钮再展开 20 行上下文，点击分隔其余部分同时向两侧展开。工作区文本不可用时，分离 hunks 之间只显示不可交互的 omitted separator。
- Diff gutter 同时显示旧行号和新行号（删除只有旧号，新增只有新号），避免折叠块之间行号看起来错位。
- add 为整份绿 `+`；delete 为整份红 `-`。
- 当前 hunk 和增删统计（`+N -M`）显示在标题/上下文行。
- diff 太大时显示截断原因。
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
| `1` / `2` | 在最右导航 pane 切换 Explorer / Review view |
| `j` / `k`、方向键 | 在当前 pane 移动选择或滚动 |
| `Left` / `Right` | 折叠或展开节点 |
| `Enter` | Explorer 打开 File；Review 打开 CL 文件列表；Content 的 CL 列表下钻 Diff |
| `d` | 为 Explorer 中已 opened 的文件打开 Diff |
| `[` / `]` | 上一个/下一个 hunk（与 Diff 工具栏 Prev/Next 相同） |
| `e` | Diff 中展开或收起全部远距未改折叠 |
| `f` / `F` | 下一个/上一个文件 |
| `v` | 开始或结束 diff 行选择 |
| `c` | 为选中行或范围编写审阅备注 |
| `a` | 将待发送备注发送给当前 Agent |
| `g` | 生成 CL 描述 |
| `s` | 打开当前 CL 的 Submit review overlay；不直接提交 |
| `o` | 按编号打开 CL |
| `/` | 搜索 CL 或文件 |
| `r` | 刷新 |
| `PageUp` / `PageDown` | Content pane 按当前可见高度翻页 |
| `?` | 打开帮助 overlay |
| `q` | 关闭 P4 pane，不退出 Herdr |

所有键位最终都应可配置。鼠标应支持节点选择、展开、滚动和宿主 split 分隔线拖动。

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
- submit 期间显示进行中状态并进入排他的 `SubmitRunning`；同一时刻最多一个 submit 进程。
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

每次 pane 打开时，从 Herdr plugin context 得到 workspace cwd。身份解析分两步，**不扫描**本机其他 client 名称：

1. **目录配置 overlay。** 从 cwd 向上读取 p4config，把其中的 `P4CLIENT` / `P4PORT` / `P4USER` 等 `P4*` 变量叠到每次 `p4` 子进程上（不清空继承环境里的 ticket / trust）。
   - 若进程设置了 `P4CONFIG`：按官方 `p4` 行为搜索该文件名，可以走到卷根（`D:\`、`/`）。
   - 若未设置 `P4CONFIG`：为兼容游戏/Helix 树，仍搜索 `p4config.txt`、`.p4config`、`.p4config.txt`。这与官方 `p4`（未设置则不搜索）不同；兼容搜索**不包含卷根**，避免 `D:\p4config.txt` 绑到同盘所有目录。
2. **运行 `p4 info`，再做 Client root 守卫。** cwd 必须落在该 client 的 Root 下（Windows 大小写不敏感；路径存在时会 canonicalize，以覆盖 junction / subst）。这是「选错了另一个 client」的防护，**不是**完整的 `p4 where` view 测试：Root 下但未映射的路径仍显示 Review（pending CL）。Explorer 列举目录项时再用 view 过滤。

若 root 守卫失败，或 `p4 info` 不能给出有效 client，面板显示连接说明，不回退到任意其他 client。

解析结果包括：

- server identity
- user
- client
- client root
- stream（如果存在）
- case handling
- client view/path mapping（Explorer 与 `p4 where` 使用；Review 的错误 client 守卫只用 root）

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
- 一个恢复 remembered workspaces 的 startup hook，默认启用。

运行时通过 Herdr 提供的 plugin environment 获取：

- plugin root
- config directory
- state directory
- workspace/tab/pane context
- Herdr binary/socket path

插件应优先通过 `HERDR_BIN_PATH` 或 socket API 与正在运行的 Herdr 通信，兼容 Unix socket 和 Windows named pipe。

首版不依赖 Herdr 的 Git worktree provenance；P4 workspace identity 完全由当前 cwd 和 P4 查询决定。

Link/install 持久注册 manifest；terminal pane 是 Herdr session 的运行时对象，不因插件已注册就自动出现在每个 workspace。默认 `open_mode = remembered`：一次成功的 `open-pane` action 把 workspace cwd、Herdr workspace id hint 和 pane id hint 写入插件 state 目录。同一 Herdr workspace id 只保留一条记忆记录。

Herdr server 恢复 session 并暴露 API 后，startup hook 执行 `restore-panes`。恢复流程先读取 Herdr workspace/pane snapshot，再按 workspace id（其次 cwd）匹配记忆记录；id hint 只用于优先匹配，不能覆盖 cwd 边界。同一 workspace 中 label 为 `Perforce` 的 pane 都是候选，插件还必须通过 `pane process-info` 确认前台存在 `herdr-p4 ... pane`（Windows 上包括 PowerShell 包装启动）才视为健康。同一 workspace 若已有健康导航 pane，不再打开第二个；多余的健康重复 pane 关掉并保留最右侧那个。标题仍是 `Perforce` 但前台只剩默认 shell 的 pane 是 corpse：Herdr 只恢复了槽位，进程已经死掉。恢复必须先清理这些空壳和残留 Content。Content 由普通 `pane split` 创建，重启后可能没有 plugin token；此时只有标题匹配、`process-info` 确认前台仍是 viewer 或默认 shell、并且布局确认它与导航候选水平相邻，才允许采用 plugin-first、plain-fallback 关闭。全部清理成功后，才从剩余的非插件 pane（通常是 Agent）右侧打开真正的导航 pane，并根据按 workspace 保存的最后 Content 请求重建中间 pane。任一关闭失败则不能继续 split。新 pane 和恢复的 Content 都使用 `--no-focus`。每个 workspace 使用独立的 `layout-<hash>.json` 保存导航比例、Explorer/Review 视图和最后 Content 请求，避免多个 pane 进程并发覆盖；旧版全局 `layout.json` 只作为迁移 fallback。

关闭当前 pane 只改变当前 session，不表示忘记 workspace。首版不实现 `detected` 模式，不在 startup 中对所有 workspace 执行 `p4 info`。

## 13. 配置与状态

### 13.1 配置目录

用户可编辑配置只放在 `HERDR_PLUGIN_CONFIG_DIR`：

- `panel.json` 的 `open_mode = manual|remembered`。
- `panel.json` 的 `diff_fold_context`（整数 0–200，默认 5）：Diff 折叠时每侧保留的未改行数；`0` 关闭折叠。
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

- remembered workspace cwd 和短期 Herdr id hint。
- workspace 专属 `layout-<hash>.json` 中保存的导航宽度、Explorer/Review 视图和最后 Content 请求。
- 展开的 CL。
- 最近打开的 CL 编号。
- 当前布局比例。
- 未发送的本地审阅备注。
- 非敏感 UI 偏好。

`remembered-workspaces.json` 有 64 KiB/128 workspace 上限。绝对路径属于本机私有状态，不进入项目目录、日志、fixture 或发布包。状态损坏、超限、相对路径或版本不支持时失败关闭，不能用空状态覆盖原文件。`panel.json` 缺失时使用 remembered 默认值；配置包含未知字段或 mode 时不恢复、不覆盖状态。

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
- submit outcome uncertain：write timeout/连接中断，或 write 返回后刷新、认证、权限、解析、匹配验证失败。

错误信息必须给出下一步，但不能建议自动执行有破坏性的修复。

Submit UI 还必须附带结果确定性：

- `not-started`：preflight/重新校验失败，未运行 `p4 submit`；修复原因后只能重新 preflight。
- `rejected`：write 获得认证、权限或服务器明确拒绝；仍需重新 preflight 和新的显式确认，不能自动重试。
- `unknown`：write 可能已经到达服务器；UI 禁止再次 Submit，只允许用原确认保存的 workspace/CL receipt 运行 `info` 和 `describe -s` 只读 reconciliation。只有确认 submitted 才显示成功；确认 pending 时旧确认作废；查询仍失败或环境不匹配时保持 unknown。

项目可以选择 external submit provider 代替原生 `p4 submit`。该模式仍复用完整 preflight、双 token freshness 和显式确认，但确认后的动作是以无 shell 的 argv 启动配置工具。成功启动只记为 handoff，不记为 submit success；插件保留 receipt 并仅允许只读 reconciliation。配置缺失时默认使用 native provider，配置存在但无效时 fail closed，不能静默回退到原生 Submit。

## 16. 首版非目标

- P4 Code Review/Swarm 评论、投票、review state。
- P4V 的完整替代；depot 浏览器；从 Explorer 树上执行 write 命令。
- sync、reconcile、edit、add、delete、reopen。工作区 File Explorer（只读树 + 独立内容预览）属于目标，见 §5.5。
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
  tui/          rendering, input, overlays, explorer tree/preview
  domain/       changelist, file, diff, review, explorer decoration models
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
- 本机 fake/P4D 测试环境：[testing.md](testing.md)
- `herdr-sidebar`（树/预览交互可借鉴，不作为依赖）：<https://github.com/alexarthurs/herdr-sidebar>
- `herdr-reviewr`（审阅备注交互可借鉴，不作为依赖）：<https://github.com/persiyanov/herdr-reviewr>
- `herdr-co-review`：<https://github.com/elKei24/herdr-co-review>
- `p4-diff`：<https://github.com/JonParr/p4-diff>
- Perforce P4VS：<https://github.com/perforce/P4VS>
- Perforce `p4 describe`：<https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/p4_describe.html>
- Perforce `p4 change`：<https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/p4_change.html>
- Perforce `p4 submit`：<https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/p4_submit.html>
