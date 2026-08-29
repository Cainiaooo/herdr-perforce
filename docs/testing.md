# Herdr Perforce 扩展测试方案

状态：当前实现验证记录
适用范围：Windows 优先的 Herdr Perforce 侧边栏扩展  
关联文档：[设计目标](design.md) · [验收标准](acceptance.md)

## 当前验收记录

| 层级 | 状态 | 执行日期 | 实际结果 |
|---|---|---|---|
| Level A | 已验收 | 2026-08-27 | `cargo fmt --check`、Clippy、release build 和 `cargo test --workspace --all-targets` 通过；library 177 个、CLI 17 个，共 194 个测试 |
| Level B | 部分完成 | 2026-08-25 | 显式只读 runner 的真实 `info/changes/describe/opened` 通过；当前仓库 cwd 不在 client view，`where` 明确 skip，未回退配置 |
| Level C | 部分完成 | 2026-08-26 | 独立 harness 的 16 个测试和真实 loopback `p4d` 通过；产品 Description Apply 与 Submit 均完成 stale 拒绝和成功写入闭环，另含 shelf、`binary+l`、第二 client 逐字节验证、隐私扫描和精确清理 |

本记录确认 Level A、一次 Level B 部分兼容验证，以及 Level C 的产品 Description Apply/Submit 核心写闭环。Submit 已包含 preflight、双 token freshness、二次确认对象、single-flight 和写后刷新；完整失败矩阵、Level B mapped `where` 和 Herdr pane/UI 集成仍未完成验收。

## 1. 目标

本方案把测试分成三个层级，并明确本机 Perforce 环境的创建、使用和清理边界：

- Level A：不依赖 Perforce Server 的单元测试与 fake `p4` 契约测试。
- Level B：连接开发者已有的 Perforce 环境，只执行只读验证。
- Level C：启动一次性的本机 `p4d`，验证会修改服务端状态的完整流程。

默认测试入口不得读取或修改开发者日常使用的 `P4PORT`、`P4USER`、`P4CLIENT`、注册表 `p4 set`、`.p4config` 或真实 workspace。任何无法证明目标属于本轮测试环境的写操作都必须失败关闭。

## 2. 测试层级

### 2.1 Level A：快速且完全隔离

每次提交和本地开发循环都应运行：

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

覆盖范围：

- tagged output、marshal/JSON（若采用）及文本 diff 的解析。
- pending、shelved、submitted changelist 的领域模型映射。
- add/edit/delete/move/integrate、binary、exclusive-lock 等文件状态。
- diff 加载、取消、超时、乱序返回与 request epoch。
- 缓存命中、版本变化失效、容量上限和驱逐。
- `freshness_token` 生成与 stale 拒绝。
- 键位路由、焦点切换、窄宽度降级和空状态。
- argv 构造：不经过 shell，不拼接用户输入。
- submit provider fail-closed load：非法 JSON、非绝对 command、缺少 `{change}`、`.bat`/`.cmd` 不得回退到 `p4 submit`。
- external launch：只替换 `{change}`，剥离 `HERDR_*`，stdio/console 与 TUI 分离。
- Review overlay 按 provider 判别式显示 Open provider / Submit，不以用户可控 label 推断 native。
- 错误分类：空 diff、无权限、未登录、未映射、binary、超时和解析失败不得混淆。

fake `p4` 必须记录收到的 argv、环境和 stdin，并能按脚本返回 stdout、stderr、退出码、延迟及分块输出。测试不得依赖本机已安装的真实 `p4.exe`。

### 2.2 Level B：已有服务器的只读兼容测试

Level B 用于确认扩展能理解真实服务器、代理、字符集和 workspace 配置，但严格限制为只读命令，例如：

- `p4 info`
- `p4 -ztag changes`
- `p4 -ztag describe -s <change>`
- `p4 describe -du <change>`
- `p4 -ztag opened -c <change>`
- `p4 where <file>`

要求：

- 必须由开发者显式启用；默认测试命令不运行 Level B。
- 测试前打印脱敏后的 server、user、client 和 charset 摘要，并再次声明“只读”。
- 禁止 `edit`、`add`、`revert`、`shelve`、`submit`、client/spec 修改等写命令。
- 服务器不可达、ticket 过期或路径未映射时应跳过或给出明确诊断，不得回退到其他配置。

当前入口：

```text
cargo run -- level-b --read-only
cargo run -- level-b --read-only --cwd <mapped-workspace-path>
```

`--read-only` 是必需的显式确认。相对 `--cwd` 会相对进程目录拼接为绝对路径，不 `canonicalize`。runner 固定限制 pending 查询最多返回 8 条，只对其中一条执行 `describe -s` 与 `opened -c`，并对 `<cwd>/...` 执行 `where`（探测 client view 内的路径，而不是 workspace 根目录本身）；输出中的 server、user 和 client 仅显示域分离的短指纹，不输出 changelist 编号、描述、文件名或本地路径。任何连接、认证、权限或解析错误都会停止，不尝试其他 P4 配置；cwd 未映射或 `where` 没有 Stat mapping record 则记录为 `completed-with-skip`。

### 2.3 Level C：一次性 loopback `p4d`

Level C 是写流程和端到端验收的权威环境。测试 harness 直接启动临时 `p4d` 进程，不安装 Windows Service，不要求 Docker，也不触碰用户的全局 Perforce 配置。

建议入口：

```text
cargo xtask p4d doctor
cargo xtask p4d test
cargo xtask p4d test --keep-on-fail
cargo xtask p4d inspect <run-id>
cargo xtask p4d cleanup <run-id>
```

在 `xtask` 尚未实现前，可用等价脚本，但其安全约束和产物格式必须相同。

当前机器使用仓库同级、无版本管理的 `../herdr-perforce-test-harness` 作为等价入口：

```powershell
cd ../herdr-perforce-test-harness
$env:HERDR_P4D_TEST_BIN = '<absolute-path-to-p4d.exe>'
cargo run -- doctor
cargo run -- level-c
```

该 harness 通过产品 `P4WriteService` 执行 Description Apply 与 numbered Submit；其 transport 对产品命令使用 fail-closed allowlist，并在每个 `change -i` 或 `submit -c` 前执行本节定义的写授权检查。Submit 先证明 spec/content freshness，再由显式确认对象执行一次写命令并刷新 submitted 状态。

## 3. 本机 `p4d` 拓扑

每轮测试使用独立目录：

```text
%TEMP%\herdr-p4-tests\<run-id>\
├─ run.json
├─ server-root\
├─ client-a\
├─ client-b\
├─ logs\
└─ artifacts\
```

- `run-id` 使用随机不可预测标识，不能复用固定目录。
- `P4PORT` 只绑定 `127.0.0.1:<random-port>`；端口冲突时创建新 run，而非接管已有进程。
- 所有 `p4` 调用显式传递 `-p`、`-u`、`-c`，并设置独立工作目录。
- 不执行 `p4 set`，不创建系统服务，不写项目目录外的持久配置。
- `run.json` 是本轮环境的所有权标记，至少记录 schema version、run id、规范化根目录、端口、PID、进程启动时间、`p4d` 路径和测试命名空间。

`p4d` 来源由用户级配置 `HERDR_P4D_TEST_BIN` 指向绝对路径。仓库不得提交服务器二进制。`doctor` 应验证文件存在、可执行且 `p4d -V` 可识别；版本不满足支持矩阵时给出明确错误。

## 4. 生命周期与安全边界

### 4.1 启动

1. 创建独立 run 目录并解析其规范化路径。
2. 选择空闲 loopback 端口。
3. 以显式 server root 和端口启动隐藏的子进程，保存进程句柄。
4. 记录 PID、启动时间和可执行文件规范化路径。
5. 轮询显式端口上的 `p4 info`，直到 ready 或超时。
6. 创建 depot、typemap、用户、clients 和 fixtures。

如果 ready 检查失败，必须停止本轮子进程、保留诊断日志并返回失败；不得连接用户默认服务器继续测试。

### 4.2 写操作授权

在执行任何 `configure`、`depot`、`client`、`change`、`edit`、`add`、`delete`、`move`、`reopen`、`shelve`、`revert`、`unlock` 或 `submit` 前，必须同时证明：

- server 地址是 loopback 且端口与 `run.json` 一致；
- server root 位于当前 run 目录内；
- PID、启动时间和可执行文件路径与启动记录一致；
- 测试 user、client、depot 名称位于保留命名空间；
- 当前路径及所有待清理子路径规范化后仍位于当前 run 目录内。

任一条件不满足即终止。禁止回退到环境变量、注册表、`.p4config` 或另一个已运行的服务器。

### 4.3 停止与清理

- 优先通过已保存的进程句柄停止本轮 `p4d`，超时后才终止该精确进程。
- 禁止使用 `taskkill /IM p4d.exe` 或按进程名批量终止。
- 删除前再次核对 marker、规范化根路径和进程身份。
- 拒绝清理包含越界 junction/symlink/reparse point 的目录；不得跟随链接删除外部内容。
- 清理后确认端口关闭，且没有本轮 client/server 进程残留。
- `--keep-on-fail` 只保留本轮目录，并输出脱敏的 inspect/cleanup 命令。

## 5. 固定测试身份与数据

使用明显不属于真实团队的固定逻辑名称，并添加 run-id 后缀以避免碰撞：

```text
Users:   ExampleAdmin, ExampleAuthor, ExampleOther
Clients: ExampleClientA, ExampleClientB
Depot:   SampleDepot
```

若测试版本要求密码，密码在进程内随机生成，只通过 stdin/临时 ticket 传递，不写日志、不进入命令行和测试快照。

### 5.1 基础文件 fixture

- 普通 UTF-8 文本：新增、修改、删除。
- move/rename，并保留 move/add 与 move/delete 关系。
- CRLF、LF、Unicode 文件名与内容。
- 文件末尾无换行。
- 大文本文件，用于分页、取消和缓存上限。
- binary 文件（例如最小伪资产），用于无文本 diff 状态。
- `binary+l` / exclusive-open 文件，用于锁持有者展示。

### 5.2 Changelist 场景

fixture builder 应返回逻辑别名到动态 changelist number 的映射，测试不得硬编码编号。

| 别名 | 场景 | 主要断言 |
|---|---|---|
| `pending_mixed` | 多种文本操作混合 | 树、计数、选择与 diff 正确 |
| `pending_binary` | binary 与 `+l` | revision、大小、锁状态可见 |
| `pending_description` | 修改描述 | apply 成功且刷新 token |
| `shelved_review` | shelved 文件 | shelved diff 与状态正确 |
| `submitted_history` | 已提交 CL | 只读历史可浏览 |
| `stale_spec` | 外部修改 change spec | apply/submit 拒绝 stale |
| `stale_text` | 外部修改文本内容 | 旧 diff/操作不可覆盖新状态 |
| `stale_binary` | 外部修改 binary/锁 | 锁与 revision 重新加载 |
| `unresolved` | 未解决 integrate | submit 被阻止且原因明确 |
| `out_of_date` | 非 head revision | submit 预检给出明确诊断 |
| `locked_other` | 第二用户持有 `+l` | 显示持有者且禁止错误写入 |
| `submit_success` | 可提交的隔离 CL | 确认后提交且 UI 刷新 |
| `auth_preflight` | preflight 返回登录过期 | not-started、提示登录、无 submit |
| `permission_preflight` | restricted/permission denied | not-started、不泄漏内容、无 submit |
| `timeout_preflight` | preflight transport timeout | not-started、可显式刷新、无 submit |
| `reject_write` | submit 明确返回认证/权限/服务器拒绝 | rejected、不自动修复或重试 |
| `unknown_write` | submit timeout/断网/输出不完整 | unknown、仅显示只读 reconciliation |
| `unknown_post_write` | submit 后 refresh 的认证/权限/timeout/mismatch | unknown、禁止再次 submit |
| `reconcile_submitted` | receipt 对应 CL 已 submitted | 只读确认成功并刷新 UI |
| `reconcile_pending` | receipt 对应 CL 仍 pending | 旧确认失效，要求新 preflight |
| `external_provider_invalid` | 配置缺失字段、非绝对 command 或缺少 `{change}` | fail closed、无外部进程、无 submit |
| `external_provider_stale` | 确认后 CL 或内容变化 | 不启动外部工具、无 submit |
| `external_provider_handoff` | 外部工具成功启动 | 显示 handoff/unknown，只允许 reconciliation，传输记录中无 submit |

## 6. 核心测试矩阵

### 6.1 数据与解析

- 每类 `p4` 输出保留匿名化 golden fixture，同时以真实临时服务器生成结果校验 parser。
- 未知 tagged 字段应向前兼容；必需字段缺失应返回结构化错误。
- stdout 和 stderr 的本地化、空输出、部分输出及非零退出码不得被误判为空 diff。
- Unicode 路径、空格、`#`、`@` 等 revision 特殊字符必须通过 argv 安全传递。

### 6.2 并发、取消与缓存

- 连续快速选择 A→B→C 时，只有最新 epoch 的 C 可更新视图。
- 被取消或超时的子进程必须回收，结果不得写入缓存。
- 缓存 key 至少包含 server/client/change/file/revision 或内容身份；refresh 发现版本变化时失效。
- 以 1000 文件 CL 验证懒加载、可取消、内存上限和交互延迟。

### 6.3 写操作与 freshness

`freshness_token` 应来自写操作相关事实的规范化摘要，而非仅使用本地选择时间。至少涵盖：

- changelist spec 中会被覆盖的字段；
- opened files 的 depot path、action、type、revision/digest 等可获得身份；
- shelved 或待提交内容中会影响操作结果的版本事实。

测试必须在预览与确认之间由第二 client 修改其中一项，并证明 Apply/Submit 拒绝 stale、刷新展示差异、不会静默覆盖。

Submit 测试还应覆盖：

- 默认焦点为 Cancel；破坏性操作不能由一次意外单键直接完成。
- 展示实际 server/user/client/change 摘要后才允许确认。
- preflight、真实 submit、失败恢复和成功后的状态刷新。
- dry-run 或 parser 测试不能替代真实 loopback submit。
- 故障注入必须分别记录 write 是否已经启动；write 可能启动后的任何 timeout/断网/解析或验证失败都归为 unknown。
- unknown 状态只能携带无 credential 的 opaque reconciliation receipt；恢复动作只允许运行当前 cwd 下的 `info` 和 `describe -s`，不得调用 submit 或切换到其他 P4 配置。
- reconciliation 的 pending 结果必须使旧授权失效；submitted 结果必须与 receipt 的 workspace、CL spec 和文件投影匹配后才显示成功。

### 6.4 UI 与宿主集成

- 默认保持 `Agent CLI | 最右导航`，导航约占 20%；打开内容后保持 `Agent CLI | Content | 最右导航`，前两栏近似等宽。
- File/Diff/CL 复用同一个 Content pane；关闭 Content 后 split 正常折叠，不替换或关闭 Agent pane。
- 验证 Content 按 pane 宽度自动换行、文件续行保留空白行号 gutter 并与正文对齐、换行后完整纵向滚动、语法高亮、CL 文件鼠标单击选择→Enter 打开 Diff→返回，以及长行不会改变导航树宽度。
- Explorer fixture 分目录验证 `A/M/D/R/U/↓/⊘/?`、delete/move-delete 虚拟行和查询失败无装饰；用 fake transport 断言未展开目录不会产生 `where/fstat/opened` 请求。Review 验证内嵌描述/Review & Submit 仅进入 preflight，并显示 Changelists / File History / Workspace History 同级 section。
- 验证 Herdr 与插件的键位所有权：焦点进入/离开、`Esc`、关闭、搜索、滚动及文本输入。
- 破坏性命令使用明确动作或组合键，并经过确认层；普通字符不应被无条件截获。
- Windows Terminal 下覆盖常用字号、缩放、窗口宽度和高 DPI。
- 首次成功手动打开后检查状态只写入 `HERDR_PLUGIN_STATE_DIR`；重启 Herdr server 后恢复 remembered workspace，重复 startup 不创建第二个 Perforce pane。同一 workspace 因不同 cwd 产生的重复记忆记录必须合并；Windows 上 PowerShell 包装的 `herdr-p4 pane` 视为健康进程。标题仍是 `Perforce` 但进程已是默认 shell 的 pane 必须先关闭再打开。按 workspace 验证导航比例、Explorer/Review 视图和最后 Content 请求；恢复后重建正确的两栏或三栏顺序，不得停在 50/50，也不得被另一 workspace 的比例覆盖。
- 同一 server 上仅重新连接客户端不应重复执行 startup；缺失 workspace 安全 skip，恢复 pane 使用 `--no-focus`。
- startup 期间用 fake `herdr` 捕获 argv，确认只执行 workspace/pane/process-info/layout 查询、plugin pane open 和对确认 stale pane 的 pane close；带完整 token 的 Content 可正常清理，token 丢失的标题候选必须同时通过 viewer/默认-shell 进程身份和与导航候选水平相邻的布局检查，之后按 plugin-first、plain-fallback 清理。任一检查或关闭失败后不得继续 open。不得关闭仅标题相似的 Agent pane；不运行 `p4`，也不同时传 `--workspace` 与 `--target-pane`。

### 6.5 故障注入

- `p4`/`p4d` 不存在、版本不支持。
- 端口冲突、server 启动超时、server 中途退出。
- ticket 失效、权限拒绝、client 未知、文件不在 view。
- 命令超时、输出过大、编码异常、parser 收到截断数据。
- 扩展重载或 Herdr 退出时仍有在途请求。
- 测试进程被强制结束后，`inspect`/`cleanup` 能识别并安全处理孤立 run。

## 7. CI 分层

- PR 必跑：format、clippy、Level A、文档链接与隐私扫描。
- Windows CI 推荐跑：下载或预置受支持的 `p4`/`p4d`，执行 Level C 全套。
- Level B 不进入公共 CI，因为它依赖组织服务器并可能暴露内部元数据。
- 发布候选：Windows 安装/卸载、真实 Herdr pane、Level C submit、1000 文件性能和 DPI 矩阵。

失败产物可包含：匿名化命令类别、退出码、耗时、run-id、loopback 端口、server log 与 UI 截图。不得包含密码、ticket、真实服务器地址、真实用户/client/depot、宿主机绝对路径或内部文件内容。

## 8. 完成判据

开发测试基础设施按以下顺序落地：

1. Level A fake `p4`、领域 fixture 与 parser tests。
2. `xtask p4d doctor` 及安全生命周期。
3. fixture builder 与 Level C 只读场景。
4. stale、锁、shelve、submit 等写场景。
5. Herdr pane 集成、性能、安装与发布矩阵。

在开始实现写能力前，至少必须具备：

- 可重复创建和清理的一次性 server；
- 写操作五项授权检查及对应负向测试；
- stale spec/content 的双 client 测试；
- 精确进程终止和越界路径拒绝测试；
- 日志隐私扫描。

## 9. 官方参考

- [启动临时 Helix Core Server](https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/P4Guide/tutorial.start-server.html)
- [`p4d` 命令参数](https://help.perforce.com/helix-core/server-apps/p4sag/2024.1/Content/P4SAG/appendix.p4d.html)
- [Windows 上手动运行或管理 Server](https://help.perforce.com/helix-core/server-apps/p4sag/current/Content/P4SAG/install-windows-manage.html)
- [`P4ROOT` 定义](https://help.perforce.com/helix-core/server-apps/cmdref/current/Content/CmdRef/P4ROOT.html)

