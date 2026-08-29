# Perforce Review for Herdr

`herdr-perforce` 是运行在 [Herdr](https://herdr.dev/) 右侧 terminal pane 中的紧凑 Perforce 插件。它让你留在当前 Herdr workspace 内查看 pending changelist，并通过可见、可取消、显式授权的 Submit overlay 提交 numbered changelist。

> 当前版本是开发预览。Submit 的产品写流程和隔离 `p4d` 闭环已经验证；独立内容 pane 的真实 Herdr 宿主 UI 验收仍在推进。

## 功能

- 从 `HERDR_PLUGIN_CONTEXT_JSON` 读取打开插件前的 pane/workspace 路径，不依赖插件安装目录推断 P4 workspace。
- 首次手动打开成功后记住该 Herdr workspace；Herdr server 下次启动恢复 session 时幂等恢复 pane，不重复打开，也不抢走当前焦点。
- 使用当前用户的 `p4` 配置列出 current-client pending changelist。
- 在 Submit 前重新读取 workspace、CL spec、opened files 和本地内容，不使用可能过期的 UI 缓存。
- 检查 owner/client、描述、文件数量、unresolved、out-of-date、本地文件映射和内容 freshness。
- 显示 CL 号、描述、文件数量和 add/edit/delete/move 等动作统计。
- Submit overlay 默认选择 Cancel；只有点击 **Submit** 或按 `Ctrl+Enter` 才会创建写授权。
- 同一进程内 Submit 单飞；重复按键和双击不会创建第二个 `p4 submit`。
- Submit 后重新读取服务器状态；超时或结果不确定时禁止再次提交，只允许只读 reconciliation。

插件不会自动执行 `resolve`、`unlock`、`revert`、`reopen`、`shelve`、`sync` 或重试 Submit，也没有独立的 CLI submit 命令。

## 环境要求

- Herdr `0.8.2` 或更高版本。
- Helix Command-Line Client `p4` 可从 `PATH` 启动。
- 当前 workspace 已配置有效的 `P4PORT`、`P4USER`、`P4CLIENT`/config，并处于 client view 内。
- 需要提交时，当前用户必须已经 `p4 login` 且具有目标 CL 的读取和提交权限。
- 从源码 link 时需要 Rust `1.85` 或更高版本和 Cargo。

Herdr local link 不会自动执行 manifest 中的 build command，因此首次 link 前必须先构建 release binary。插件安装、运行环境和信任边界见 [Herdr Plugins](https://herdr.dev/docs/plugins/)。

## 从本地源码安装

克隆或进入仓库后先构建：

```powershell
cd D:\Projects\herdr-perforce
cargo build --release
herdr plugin link .
```

链接是当前用户全局生效的，但源码和构建产物仍保留在这个 checkout 中。可以用以下命令确认 manifest 已被 Herdr 接受：

```powershell
herdr plugin list --plugin herdr.perforce --json
herdr plugin action list --plugin herdr.perforce
```

### Windows 打开插件

```powershell
herdr plugin action invoke open-windows --plugin herdr.perforce
```

### Linux / macOS 打开插件

```bash
herdr plugin action invoke open --plugin herdr.perforce
```

也可以从 Herdr 的 plugin action 列表中选择 **Open Perforce review**。该 action 会在当前 pane 右侧打开约 20% 宽的导航 pane，并把打开前的 workspace/pane context 交给插件。导航 pane 默认打开 **File Explorer**；按 `2` 或点击 **P4 Review** 查看 changelist。Review 上方内嵌当前 CL 的可编辑描述框与 **Review & Submit** 入口，下方是 Changelists，并预留与其同级的 File History / Workspace History。首次查看 File、Diff 或 CL 文件列表时，会在 Agent CLI 与最右导航之间按需创建内容 pane。

### Panel 加载与自动恢复

`herdr plugin link` 持久注册插件，但不会让 terminal pane 永久驻留。首次仍需在目标 workspace 手动执行一次 **Open Perforce review**；打开成功后，插件把该 workspace 记录到 Herdr 提供的 `HERDR_PLUGIN_STATE_DIR`。

之后 Herdr server 启动并恢复 session 时，manifest 的 startup hook 会：

1. 读取已记住的 workspace，不扫描其他目录，也不在启动阶段运行 `p4`。
2. 只处理本次 session 中仍存在且 cwd 匹配的 Herdr workspace。
3. 对已有 `Perforce` pane 调用 `pane process-info`；前台是 `herdr-p4 ... pane`，或 Windows 上由 PowerShell 包装启动的同一命令，都视为健康并保持现状。同一 Herdr workspace 只保留一个导航 pane，多出来的重复 pane 会关掉。
4. 标题是 `Perforce` 但里面只是 `PS ...>` 或其他默认 shell 的 pane 是 corpse：先关掉空壳和残留 Content pane，再从剩余 pane 打开真正的插件进程。Content 本来由普通 split 创建，重启后 token 可能丢失；只有标题匹配、前台仍是 viewer 或默认 shell、并且与已确认的导航候选水平相邻时才允许按 plugin-first、plain-fallback 路径清理。不能把同名 Agent 当成 Content，也不能把空壳当成已经恢复。新 pane 使用 `--no-focus`。
5. 导航比例、Explorer/Review 视图以及最后打开的 File/Diff/CL 请求按 workspace 恢复；有 Content 时重新建立 `Agent | Content | Navigation`，且不抢焦点。

关闭并重新打开一个连接到同一 Herdr server 的客户端，不会重复运行 startup hook；要验证恢复行为，需要真正重启 Herdr server。仅关闭当前 Perforce pane 不会删除 workspace 记录，因此下次 server 启动仍会恢复它。

默认模式是 `remembered`。如果希望所有 workspace 始终手动打开，在插件配置目录创建 `panel.json`：

```powershell
$config = herdr plugin config-dir herdr.perforce
Copy-Item .\examples\panel.manual.json (Join-Path $config 'panel.json')
```

对应内容为：

```json
{
  "open_mode": "manual",
  "diff_fold_context": 5
}
```

`diff_fold_context` 是 Diff 折叠时每侧保留的未改行数，默认 `5`，允许 `0`–`200`；`0` 表示不折叠。省略该字段时使用默认值。

删除该文件，或改为 `{ "open_mode": "remembered" }`，即可恢复默认行为。配置损坏、包含未知字段、未知 mode 或非法 `diff_fold_context` 时会失败关闭，不自动恢复 pane；已保存的 workspace 记录不会被覆盖。

## 使用方法

主界面快捷键：

| 按键 | 行为 |
|---|---|
| `j` / `k`、方向键 | 选择目录、文件或 changelist |
| `1` / `2` | 切换 Explorer / P4 Review 导航 |
| `Enter` | Explorer 中打开 File；Review 中打开 CL 文件列表 |
| `m` / 右键 | 打开当前行的上下文菜单（Rename、Copy Path、Reveal 等） |
| 滚轮 / 拖动 | 树与 CL 列表纵向滚动；横向拖动查看被截断的名称及 Explorer 状态图例 |
| `d` | Explorer 中为 opened file 打开 Diff |
| `o` | 使用系统默认应用打开 Explorer 选中路径 |
| `e` / 点击描述框 | 读取完整 change form 后，编辑当前 owned numbered pending CL 的 Description |
| `s` / 点击 **Review & Submit** | 为当前 numbered pending CL 打开 Submit review；不会直接提交 |
| `r` | Explorer 中重新读取磁盘目录与已展开 folder 的 P4 状态；Review 中刷新 workspace/CL |
| `q` | 在没有阻塞 overlay 时关闭插件 pane |

Description 编辑器：

| 输入 | 行为 |
|---|---|
| 普通输入 / 粘贴 | 在 pane 内直接修改多行 Description；显示真实终端光标 |
| `Enter` / `Tab` | 插入换行 / 缩进 |
| 方向键、`Home` / `End`、`Backspace` / `Delete` | 移动光标或编辑文本 |
| `Ctrl+Enter` / 点击 **Apply Description** | 显式确认本次修改；重新校验 freshness 后写入并回读验证 |
| `Esc` / 点击 **Cancel** | 丢弃本次编辑；不写入 P4 |

列表使用 `p4 changes -l` 读取完整描述，进入编辑时还会从完整 change form 再次读取，避免长 Description 被摘要截断。加载、编辑、freshness 检查和 Apply 进行中，背景 CL 列表不会响应选择或 Submit 快捷键。`Ctrl+Enter` 或显式点击 **Apply Description** 会创建一次写授权；Apply 会重新校验 freshness、只替换 Description，并在写后重新读取验证，不需要第二次确认。

Submit overlay：

| 输入 | 行为 |
|---|---|
| `Enter` / `Esc` | Cancel；不提交 |
| 点击 **Cancel** | 关闭 review；不提交 |
| 点击 **Submit** | 使用当前 overlay 的 freshness receipt 执行一次 Submit |
| `Ctrl+Enter` | 与明确点击 **Submit** 相同 |
| `r` | 失败后重新 preflight；结果未知时仅运行只读 reconciliation |

overlay 打开时，背景列表不会响应 Submit 或导航快捷键。SubmitRunning 期间无法从插件内取消、关闭或启动第二次提交。

内容 pane：长行始终根据 pane 当前宽度自动换成多行；文件行号使用固定 gutter，续行保留空白 gutter 并与上一行正文对齐。`↑` / `↓`、`PageUp` / `PageDown` 或鼠标滚轮滚动，`q` 关闭；CL 文件列表支持鼠标单击选择，`Enter` 打开 Diff，进入 Diff 后 `Esc` 返回列表。文本文件按类型高亮，二进制文件显示有界 metadata card。Explorer 使用按文件类型区分的可爱图标，例如 `🦀` Rust、`🔩` C/C++、`📝` Markdown、`📷` 图片，目录仍使用 `📂` / `📁`。

Explorer 的 P4 状态采用与 IDE Source Control 类似的颜色和短标记：

| 标记 | 含义 |
|---|---|
| `A` | 已在 Perforce 中打开为新增或 branch |
| `M` | 已打开为修改、integrate 或 import |
| `D` | 已打开为删除、purge 或 archive |
| `R` | `move/add` 或 `move/delete` |
| `U` | 本地存在但当前 depot head 未管理；包括 depot 已删除后在同一路径重新创建的本地文件 |
| `↓` | 已被 Perforce 管理，但 `haveRev < headRev`，工作区版本落后 |
| `⊘` / `?` | 不在 client view / 无法建立映射 |

状态查询严格按目录懒加载：初始只查询 Explorer 根目录的直接条目，展开 folder 后才批量查询该 folder 的 `where` / `fstat` / `opened`。折叠目录不会递归探测，也不会显示潜在的子孙状态。当前目录中已打开为 delete 或 move/delete、因而已从磁盘消失的文件，会以只读虚拟行显示。

Diff 以当前文件为画布：未改行就是文件本身，删除为红底 `-`，新增为绿底 `+`，行内替换会把变化的词标得更亮。远距未改默认折叠；修改区域上方和下方各有一条分割行，点击后朝该方向再展开 20 行上下文。`e` / **Expand all** 展开全部；**Prev** / **Next** 或 `[` / `]` 跳到上一/下一处改动。

## Submit 安全模型

Submit 只对当前 P4 用户、当前 client 的 numbered pending changelist 启用。每次确认都绑定以下事实：

- workspace identity、owner、client 和 CL 状态；
- 规范化 change spec 与文件动作元数据的 `spec_token`；
- 文本和 binary 本地内容的流式 `content_token`；
- 最多 4,096 个 opened files；
- 文件映射、类型、revision、unresolved 和 out-of-date 状态。

用户确认后，插件会再次读取并比较这些事实。任何变化都会使旧确认失效，而且不会运行 `p4 submit`。

故障结果分为三类：

| 结果 | 含义 | 下一步 |
|---|---|---|
| `not-started` | 认证、权限、网络、超时或 preflight 阻止了写命令 | 修复原因后重新 preflight 和确认 |
| `rejected` | Perforce 明确拒绝了 Submit | 修复服务器报告的问题，再创建新确认；不会自动重试 |
| `unknown` | write 可能已到达服务器，但最终状态未能验证 | 不得再次 Submit；恢复只读能力后执行 reconciliation |

reconciliation 只运行当前 workspace 下的 `p4 info` 和 `p4 describe -s`。确认 submitted 才显示成功；确认仍为 pending 时旧授权作废；环境或结果无法匹配时继续保持 `unknown`。

### 外部提交工具

部分 Perforce 项目会禁止直接执行 `p4 submit`，要求通过独立的预检查、Review 或提交应用完成最终提交。插件支持为这类 workspace 配置 external submit provider。

先查询插件配置目录：

```powershell
herdr plugin config-dir herdr.perforce
```

在该目录创建 `submit-provider.json`：

```json
{
  "mode": "external",
  "label": "External submit tool",
  "command": "C:\\absolute\\path\\to\\submit-tool.exe",
  "args": ["--changelist", "{change}"]
}
```

配置约束：

- `command` 必须是已存在的绝对可执行文件路径，不能是 `.bat` / `.cmd`。
- `args` 是直接传给进程的 argv，不经过 PowerShell、`cmd.exe` 或其他 shell。
- `args` 必须包含 `{change}`；启动时只会替换为已确认的 numbered CL。
- external provider 启用后，Herdr 仍执行完整只读 preflight 和提交前 freshness 校验，但不会运行 `p4 submit`。
- 外部工具启动成功只表示“已交接”，不表示提交成功。Overlay 会阻止再次提交，并只提供 read-only reconciliation，直到 Perforce 明确显示 submitted 或 pending。
- 删除 `submit-provider.json`，或写入 `{ "mode": "native" }`，即可恢复原生 `p4 submit`。

仓库提供了可复制修改的示例：[examples/submit-provider.external.json](examples/submit-provider.external.json)。打开 Submit overlay、刷新 preflight 或确认提交时会重新读取该配置，无需重启 Herdr 或重新打开 pane。

## 故障排查

### 插件没有出现在 Herdr 中

```powershell
herdr plugin list --plugin herdr.perforce --json
herdr plugin action list --plugin herdr.perforce
herdr plugin log list --plugin herdr.perforce
```

确认已经运行 `cargo build --release`，并检查 `target/release/herdr-p4.exe`（Windows）或 `target/release/herdr-p4`（Linux/macOS）是否存在。

### Panel 可以手动打开，但重启后没有恢复

先确认该 workspace 至少成功手动打开过一次，并检查 startup log：

```powershell
herdr plugin log list --plugin herdr.perforce
```

`no remembered workspaces` 表示还没有成功记录；`manual mode` 表示 `panel.json` 禁用了恢复；`unavailable` 表示记录的 cwd 在本次 Herdr session 中没有匹配 workspace。Startup log 只输出数量和分类，不输出 workspace 绝对路径。

如果 pane 边框显示 `Perforce`，内容却只是 `PS ...>` 或其他 shell prompt，表示 Herdr 恢复了 pane 布局但插件进程没有存活。当前版本会把它识别为 corpse：先关掉这个空壳，再打开真正的插件 pane。Windows 上 pane 入口通过 PowerShell 启动 `herdr-p4.exe pane`，这不算 stale。日志中的 `stale-closed` 是成功清理的空壳数量，`duplicates-closed` 是同一 workspace 里关掉的重复健康 pane 数量。有完整 token 的 Content 可直接识别；重启后 token 丢失的 Content 还必须同时满足 viewer/默认-shell 进程身份和与导航候选水平相邻，之后才走 plugin-first、plain-fallback 关闭。仅标题相似的 Agent 不会被关闭。任一清理失败会计入 `failed`，并阻止继续 split。workspace 专属的 `layout-<hash>.json` 保存导航宽度、当前 Explorer/Review 视图和最后 Content 请求；旧 `layout.json` 只用于迁移，项目之间不会互相覆盖。

### `p4` 找不到或 workspace 不可用

在目标 workspace 的普通 terminal 中先验证：

```powershell
p4 info
p4 login -s
p4 opened
```

插件不会回退到其他 P4 配置，也不会自动选择另一个 client。

### 显示认证或权限错误

在目标 workspace 中完成 `p4 login`，或让管理员授予目标 CL 所需权限，然后回到 overlay 按 `r` 重新 preflight。插件不会读取、保存或显示 ticket 内容。

### 显示 “Submission result unknown”

不要再次提交。先恢复网络、登录或读取权限，然后在原 overlay 中选择 **Read-only reconcile**。如果仍无法确认结果，应直接在 Perforce 管理工具中检查该 CL。

## 更新与移除

修改源码后通常只需重新构建 release binary：

```powershell
cargo build --release
```

移除 local link 不会删除 checkout：

```powershell
herdr plugin unlink herdr.perforce
```

## 当前限制

- 当前版本已接入本地目录树、独立 File/Diff/CL 内容 pane、内联叠加 Diff（折叠/hunk 跳转/行内高亮）、滚动/高亮以及内联 Description Apply；review comment 与 Agent 描述生成 UI 尚未接完。
- 自动打开只覆盖成功手动打开过的 remembered workspaces；当前版本不会扫描全部 Herdr workspace 并运行 `p4 info` 自动判定。
- 不支持 default changelist、部分文件、其他用户或其他 client 的 Submit。
- 不支持自动 resolve、lock 修复或任何自主提交。
- 插件内会阻止 SubmitRunning/unknown 状态下关闭或再次提交，但 Herdr 宿主强制终止 pane 时的独立 worker 监管仍待实现。
- 真实 Herdr pane 内的隔离 Level C 点击/焦点/DPI 完整验收仍是发布前 Gate。

## 开发与验证

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
cargo test --workspace
```

不经过 Herdr 时可以直接启动 pane 调试：

```powershell
cargo run -- pane --cwd <mapped-workspace>
```

真实写测试只能在仓库同级的隔离 harness 中运行，不能对生产 P4 Server 执行：

```powershell
cd ..\herdr-perforce-test-harness
$env:HERDR_P4D_TEST_BIN = 'D:\Perforce\p4d.exe'
cargo run -- doctor
cargo run -- level-c
```

更多设计和验收资料：

- [设计基线](docs/design.md)
- [首版验收计划](docs/acceptance.md)
- [测试方案](docs/testing.md)
- [架构决策记录](docs/adr/README.md)
