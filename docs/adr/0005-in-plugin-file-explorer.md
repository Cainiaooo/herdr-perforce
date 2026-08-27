# ADR-0005：工作区 File Explorer 在本插件内独立实现

状态：Accepted
日期：2026-08-27

## Context

首版 [design.md](../design.md) 把导航定义成 **Changelist / opened files 树**，不是工作区目录树。宿主示意图里右侧还有 `Files / Browser / ...`，暗示文件浏览可以由其他 Herdr 插件承担。

后续产品决策是：一个 Herdr workspace 只认一种 SCM 宿主。P4 client view 内的路径走 `herdr-perforce`，非 P4 走社区 Git 插件（如 `herdr-sidebar`）。Herdr v1 没有插件依赖，P4 不能运行时引用社区 File Explorer。

因此 P4 workspace 若仍没有自己的目录树和预览，用户会失去浏览未 opened 文件的能力，或被迫打开 Git sidebar，破坏互斥。

## Decision

- 工作区 File Explorer（本地目录树、只读 P4 装饰）在 `herdr-perforce` 的**导航 pane** 内实现，作为与 Review 并列的内部 view；文件内容在 [ADR-0006](0006-standalone-content-pane.md) 定义的独立内容 pane 中显示。
- 不安装、不调用、不 fork 社区 File/Git 插件来提供这棵树。
- Explorer 根默认为当前 Herdr workspace **cwd**（不是 Client root，也不是 depot 根）。列举不得走出 Client root / client view。
- 首版 Explorer 只读：不从树上执行 `add` / `edit` / `delete` / `sync` / `revert`。选择文件可在内容 pane 查看 File；opened 文件还可查看 Diff。
- Explorer 与 Review 共用最右侧窄导航 pane；File、Diff 和 CL 文件列表共用中间内容 pane。
- 树控件和预览可以借鉴 `herdr-sidebar` / `herdr-reviewr` 的交互，代码在本仓库重写，不抽跨插件 crate 作为首版完成条件。

## Rejected alternatives

### 运行时依赖社区 File Explorer 插件

否决原因：Herdr v1 没有 `depends_on` 或跨插件 UI SDK。

### 把 Git sidebar 改成 P4 provider

否决原因：sidebar 绑死 Git index/stage；互斥策略要求 P4 与 Git 宿主分开。

### 首版继续只做 CL 树，Explorer 留到以后

否决原因：P4 workspace 关闭 Git sidebar 之后，没有第二棵树可用。Explorer 是互斥策略的配套能力，不是 P4V 替代。

## Consequences

- `design.md` 必须区分 CL 文件树和工作区 Explorer，并更新宿主示意图。
- 验收需要覆盖树浏览、预览、装饰和 view 切换；Explorer 写操作仍保持非目标。
- 实现者不得把 depot 浏览器、资产内容预览或 `p4 add` 从树上做进首版。
- Review 的「当前 client」守卫比较的是 Client **root**（含 junction canonicalize），不是完整 `p4 where` view；Explorer 落地时再用 view 过滤目录项。
