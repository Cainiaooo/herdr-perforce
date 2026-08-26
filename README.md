# Perforce Review for Herdr

`herdr-perforce` 是运行在 [Herdr](https://herdr.dev/) 右侧 terminal pane 中的紧凑 Perforce 插件。它让你留在当前 Herdr workspace 内查看 pending changelist，并通过可见、可取消、显式授权的 Submit overlay 提交 numbered changelist。

> 当前版本是开发预览。Submit 的产品写流程和隔离 `p4d` 闭环已经验证；完整 diff/navigator 与真实 Herdr 宿主内的最终 UI 验收仍在推进。

## 功能

- 从 `HERDR_PLUGIN_CONTEXT_JSON` 读取打开插件前的 pane/workspace 路径，不依赖插件安装目录推断 P4 workspace。
- 使用当前用户的 `p4` 配置列出 current-client pending changelist。
- 在 Submit 前重新读取 workspace、CL spec、opened files 和本地内容，不使用可能过期的 UI 缓存。
- 检查 owner/client、描述、文件数量、unresolved、out-of-date、本地文件映射和内容 freshness。
- 显示 CL 号、描述、文件数量和 add/edit/delete/move 等动作统计。
- Submit overlay 默认选择 Cancel；只有点击 **Submit** 或按 `Ctrl+Enter` 才会创建写授权。
- 同一进程内 Submit 单飞；重复按键和双击不会创建第二个 `p4 submit`。
- Submit 后重新读取服务器状态；超时或结果不确定时禁止再次提交，只允许只读 reconciliation。

插件不会自动执行 `resolve`、`unlock`、`revert`、`reopen`、`shelve`、`sync` 或重试 Submit，也没有独立的 CLI submit 命令。

## 环境要求

- Herdr `0.7.0` 或更高版本；本地开发环境当前使用 Herdr `0.8.2`。
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

也可以从 Herdr 的 plugin action 列表中选择 **Open Perforce review**。该 action 会在当前 pane 右侧打开 split pane，并把打开前的 workspace/pane context 交给插件。

## 使用方法

主界面快捷键：

| 按键 | 行为 |
|---|---|
| `j` / `k`、方向键 | 选择 changelist |
| `s` | 为当前 numbered pending CL 打开 Submit review；不会直接提交 |
| `r` | 重新执行只读 workspace/CL 刷新 |
| `q` | 在没有阻塞 overlay 时关闭插件 pane |

Submit overlay：

| 输入 | 行为 |
|---|---|
| `Enter` / `Esc` | Cancel；不提交 |
| 点击 **Cancel** | 关闭 review；不提交 |
| 点击 **Submit** | 使用当前 overlay 的 freshness receipt 执行一次 Submit |
| `Ctrl+Enter` | 与明确点击 **Submit** 相同 |
| `r` | 失败后重新 preflight；结果未知时仅运行只读 reconciliation |

overlay 打开时，背景列表不会响应 Submit 或导航快捷键。SubmitRunning 期间无法从插件内取消、关闭或启动第二次提交。

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

## 故障排查

### 插件没有出现在 Herdr 中

```powershell
herdr plugin list --plugin herdr.perforce --json
herdr plugin action list --plugin herdr.perforce
herdr plugin log list --plugin herdr.perforce
```

确认已经运行 `cargo build --release`，并检查 `target/release/herdr-p4.exe`（Windows）或 `target/release/herdr-p4`（Linux/macOS）是否存在。

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

- 当前 pane 重点覆盖 pending CL 列表和 Submit overlay；完整文件树、diff、review comment 和 Description Apply UI 尚未接完。
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
