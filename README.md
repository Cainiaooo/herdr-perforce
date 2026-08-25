# herdr-perforce

`herdr-perforce` 是 Herdr 右侧工具区域中的紧凑 Perforce 审阅面板。产品目标、验收 Gate 和隔离测试边界分别见：

- [设计基线](docs/design.md)
- [首版验收计划](docs/acceptance.md)
- [测试方案](docs/testing.md)
- [架构决策记录](docs/adr/README.md)

## 当前实现阶段

Level A 离线基础已完成，当前推进 Level B 真实只读兼容验证：

- Rust library 与 `herdr-p4` binary 骨架；
- 不依赖 UI 或进程实现的领域模型；
- `p4 -ztag -Mj` 行式 JSON record 解析，包括真实 `-Mj` 无 `code` 记录；
- workspace、pending changelist 和 opened file 的领域映射；
- 结构化只读命令、输出预算、错误分类、确定性 fake P4 transport，以及在执行中强制超时/字节预算的进程 transport；
- `spec_token` 的稳定 BLAKE3 规范化；
- 显式 `--read-only` 门控、命令 allowlist、结果脱敏和不回退配置的 Level B runner。

此阶段没有真实 P4 写能力。Description Apply 和 Submit 在一次性 loopback `p4d` harness 及其负向安全测试完成前不会实现。

Level B 可通过 `cargo run -- level-b --read-only` 执行；若要验证另一个明确的映射目录，追加 `--cwd <workspace-path>`。相对路径会相对当前进程目录拼接成绝对路径，不会 `canonicalize`。该入口最多采样 8 个 pending changelist，只输出脱敏 identity、计数和检查状态。

## 开发检查

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

