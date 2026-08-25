# herdr-perforce

`herdr-perforce` 是 Herdr 右侧工具区域中的紧凑 Perforce 审阅面板。产品目标、验收 Gate 和隔离测试边界分别见：

- [设计基线](docs/design.md)
- [首版验收计划](docs/acceptance.md)
- [测试方案](docs/testing.md)
- [架构决策记录](docs/adr/README.md)

## 当前实现阶段

当前从测试方案规定的 Level A 基础开始：

- Rust library 与 `herdr-p4` binary 骨架；
- 不依赖 UI 或进程实现的领域模型；
- `p4 -ztag -Mj` 行式 JSON record 解析；
- workspace、pending changelist 和 opened file 的领域映射；
- 结构化只读命令、输出预算、错误分类、确定性 fake P4 transport，以及在执行中强制超时/字节预算的进程 transport；
- `spec_token` 的稳定 BLAKE3 规范化。

此阶段没有真实 P4 写能力。Description Apply 和 Submit 在一次性 loopback `p4d` harness 及其负向安全测试完成前不会实现。

## 开发检查

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

