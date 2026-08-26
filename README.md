# herdr-perforce

`herdr-perforce` 是 Herdr 右侧工具区域中的紧凑 Perforce 审阅面板。产品目标、验收 Gate 和隔离测试边界分别见：

- [设计基线](docs/design.md)
- [首版验收计划](docs/acceptance.md)
- [测试方案](docs/testing.md)
- [架构决策记录](docs/adr/README.md)

## 当前实现阶段

Level A 离线基础和 Level B 只读 runner 已完成，当前把已验证的 Level C 产品写流程接入 Herdr pane：

- Rust library 与 `herdr-p4` binary 骨架；
- 不依赖 UI 或进程实现的领域模型；
- `p4 -ztag -Mj` 行式 JSON record 解析，包括真实 `-Mj` 无 `code` 记录；
- workspace、pending changelist 和 opened file 的领域映射；
- 结构化只读命令、输出预算、错误分类、确定性 fake P4 transport，以及在执行中强制超时/字节预算的进程 transport；
- `spec_token` 的稳定 BLAKE3 规范化；
- 显式 `--read-only` 门控、命令 allowlist、结果脱敏和不回退配置的 Level B runner；
- 产品级 Description Apply：owned/current-client/numbered-pending 门控、完整 change form 只替换 Description、显式确认对象、Apply 前 stale token 重查和写后刷新验证；
- 产品级 Submit：4,096 文件上限、pending/open-file preflight、unresolved/out-of-date/缺失或越界本地文件门控、流式 `content_token`、spec/content 双 stale 检查、显式二次确认、workspace 级 Submit 单飞、单次 `p4 submit -c` 和提交后状态刷新。
- Herdr v1 manifest、打开 action 和 split terminal pane 入口；pane 从 `HERDR_PLUGIN_CONTEXT_JSON` 取得 focused pane/workspace cwd，并把该路径显式传给每个 P4 child process，而不依赖插件进程 cwd。它列出当前 client 的 pending CL，并以 terminal overlay 接入 Submit preflight、确认、运行、结果和只读 reconciliation。
- Submit UI 故障矩阵明确区分“未启动写”“服务器明确拒绝”和“结果未知”；认证、权限、预检超时不会伪装成已提交，write/写后刷新超时或连接中断不会提供直接重试，只允许只读核对服务器状态。

Description Apply 和 Submit 都没有暴露为独立 CLI 写入口；二者只由产品 library 的“预览/preflight → 显式确认 → 再读校验 → 写入 → 刷新”API 进入。一次性 loopback `p4d` 已验证 Description 的 spec stale 拒绝与只改 Description，以及 Submit 的本地内容 stale 拒绝、单次产品 submit、最终 submitted 状态、第二 client 精确字节和无残留清理。当前 Herdr pane 切片已接 Submit；完整 diff/navigator、宿主强制关闭期间的独立写进程监管和真实 Herdr Level C UI 闭环仍是后续工作。

Level B 可通过 `cargo run -- level-b --read-only` 执行；若要验证另一个明确的映射目录，追加 `--cwd <workspace-path>`。相对路径会相对当前进程目录拼接成绝对路径，不会 `canonicalize`。该入口最多采样 8 个 pending changelist，只输出脱敏 identity、计数和检查状态。

本地构建后可用 `herdr plugin link <repo-path>` 链接 `herdr-plugin.toml`，再调用 action `herdr.perforce.open`（Windows 为 `herdr.perforce.open-windows`）。开发期也可直接运行 `cargo run -- pane --cwd <mapped-workspace>`。`pane` 不是写 CLI；最终写授权只会由可见 overlay 中的 Submit 点击或 `Ctrl+Enter` 创建。

## 开发检查

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

