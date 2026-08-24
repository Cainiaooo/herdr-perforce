# ADR-0004：Agent 描述生成器的配置与信任边界

状态：Accepted
日期：2026-08-24

## Context

描述生成器允许配置任意 executable 和 argv。若插件从项目或仓库目录自动读取这种配置，仅仅打开一个不受信任的 checkout 就可能导致执行仓库提供的命令。

`.p4config` 是 P4 自己的连接配置来源，但它不能成为插件 Agent generator、Prompt 或 keybinding 的配置扩展点。

## Decision

- 可执行生成器配置只从 `HERDR_PLUGIN_CONFIG_DIR` 中的用户级配置读取。
- 绝不从 workspace、仓库、depot、项目目录、当前 cwd、`.p4config` 相邻文件或插件 state 目录读取/合并 generator command。
- 不提供项目级覆盖、向上搜索或自动发现 Agent command。
- `command` 必须是 argv 数组并直接 spawn，不经过 shell。
- 安装源码和 manifest 可以提供不可执行的示例配置，但不能在未复制到用户配置目录时生效。
- UI 显示解析后的 executable、argv、cwd、Prompt 大小和 timeout；不显示 secret 环境值。
- Agent CLI 作为当前用户运行，不宣称受到 Herdr 或插件沙箱保护。
- 子进程默认继承启动 Herdr 的用户环境，使受信任 Agent CLI 能使用其正常认证和用户配置。
- spawn 前移除 Herdr 控制能力变量，包括 socket/binary path、plugin context、workspace/tab/pane/action/entrypoint identifiers。
- spawn 前移除明文 `P4PASSWD`；生成器不需要自行查询 P4。插件不读取或复制 ticket 内容。
- 首版不读取 `.env`，也不支持在 generator 配置中新增 secret environment values。
- 日志和 UI 不得转储完整环境或 Agent credential values。
- 配置无效时 fail closed，不回退执行项目中的同名程序或脚本。

## Rejected alternatives

### 仓库中的 `.herdr-perforce.toml`

否决原因：配置包含任意代码执行能力，不应由不受信任仓库授予。

### 自动查找 PATH 中所有 Agent CLI 并任选一个

否决原因：行为不确定，用户无法审查实际调用目标。

### shell command 字符串

否决原因：Windows/Unix 转义不同，并扩大注入和秘密泄露风险。

## Consequences

- 团队不能通过仓库文件自动共享 generator command；可以共享文档中的建议配置。
- 用户必须显式选择和配置信任的 Agent CLI。
- 配置测试必须证明恶意仓库文件不会改变执行命令。
- Agent CLI 仍以当前用户身份运行并可访问该用户可访问的文件；环境清理不是进程沙箱。
