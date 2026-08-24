# Herdr Perforce 扩展测试方案

状态：开发前基线  
适用范围：Windows 优先的 Herdr Perforce 侧边栏扩展  
关联文档：[设计目标](design.md) · [验收标准](acceptance.md)

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

### 6.4 UI 与宿主集成

- 右侧 pane 内保持“两列”：左侧主区域为 diff，右侧窄列为 changelist/files 树。
- 宽度不足时按设计定义降级，不把 pane 扩成全屏工作台。
- 验证 Herdr 与插件的键位所有权：焦点进入/离开、`Esc`、关闭、搜索、滚动及文本输入。
- 破坏性命令使用明确动作或组合键，并经过确认层；普通字符不应被无条件截获。
- Windows Terminal 下覆盖常用字号、缩放、窗口宽度和高 DPI。

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

