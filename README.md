# herdr-perforce

`herdr-perforce` 是 Herdr 右侧工具区域中的紧凑 Perforce 审阅面板。产品目标、验收 Gate 和隔离测试边界分别见：

- [设计基线](docs/design.md)
- [首版验收计划](docs/acceptance.md)
- [测试方案](docs/testing.md)
- [架构决策记录](docs/adr/README.md)

## 当前实现阶段

Level A 离线基础已完成，Level B 只读 runner 已落地，当前推进 Level C 产品写流程：

- Rust library 与 `herdr-p4` binary 骨架；
- 不依赖 UI 或进程实现的领域模型；
- `p4 -ztag -Mj` 行式 JSON record 解析，包括真实 `-Mj` 无 `code` 记录；
- workspace、pending changelist 和 opened file 的领域映射；
- 结构化只读命令、输出预算、错误分类、确定性 fake P4 transport，以及在执行中强制超时/字节预算的进程 transport；
- `spec_token` 的稳定 BLAKE3 规范化；
- 显式 `--read-only` 门控、命令 allowlist、结果脱敏和不回退配置的 Level B runner；
- 产品级 Description Apply：owned/current-client/numbered-pending 门控、完整 change form 只替换 Description、显式确认对象、Apply 前 stale token 重查和写后刷新验证。

Description Apply 没有暴露为独立 CLI 写入口；它只由产品 library 的“预览 → 显式确认 → 再读校验 → 写入 → 刷新”API 进入，并已在一次性 loopback `p4d` 中验证 stale 拒绝和成功写入。Submit 目前仍只由隔离 harness 直接验证 Perforce 闭环，产品级 preflight、二次确认和 single-flight 尚未实现。

Level B 可通过 `cargo run -- level-b --read-only` 执行；若要验证另一个明确的映射目录，追加 `--cwd <workspace-path>`。相对路径会相对当前进程目录拼接成绝对路径，不会 `canonicalize`。该入口最多采样 8 个 pending changelist，只输出脱敏 identity、计数和检查状态。

## 开发检查

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

