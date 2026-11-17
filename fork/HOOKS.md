# 上游挂钩清单

fork 对上游已有文件的全部改动。每处挂钩上方都有 `// FORK: <原因>`（TOML 里是 `# FORK:`），`rg "FORK:"` 能列出全部挂钩。

改动上游文件前先确认没有别的办法；确实要加挂钩时，在同一个提交里更新本文件和 `fork/check-hooks.sh` 的清单。

## 清单

| # | 文件 | 改动 | 原因 |
|---|---|---|---|
| H1 | `Cargo.toml` | `members` 和 `[workspace.dependencies]` 各加 4 个 `fork_` crate | 让自有 crate 进入 workspace |
| H2 | `crates/zed/Cargo.toml` | 加 `fork_ext.workspace = true` | `zed` 是唯一依赖 `fork_ext` 的上游 crate |
| H3 | `crates/zed/src/main.rs` | `edit_prediction_registry::init` 之后调用 `fork_ext::init(app_state.clone(), cx)` | fork 功能的总入口 |
| H4 | `crates/zed/src/zed/edit_prediction_registry.rs` | `assign_edit_prediction_provider` 开头：`fork_ext::assign_edit_prediction_provider` 返回 `true` 时提前返回 | fork 的编辑预测 provider 优先于官方设置，见设计文档 5.1 |
| H5 | `crates/settings_content/src/settings_content.rs` | `mod fork;`、`pub use fork::*;`、`SettingsContent` 加字段 `fork`、`flattened_deserialize!` 的 `options` 加 `fork` | 设置只能从 `SettingsContent` 读取；`SettingsContent` 的反序列化由宏按字段列表生成，漏掉列表会导致 `fork` 键被忽略 |
| H6 | `crates/settings_content/src/fork.rs` | **新文件**，`ForkSettingsContent` 及子结构 | 设置结构放在新文件里，不和上游冲突 |
| H7 | `crates/editor/src/hover_popover.rs` | `open_markdown_url` 前加全局处理函数槽 `ForkHoverLinkHandler(pub fn(&str, &mut Window, &mut App) -> bool)`；函数开头遇到 `zed-fork://` 链接时先交给它，返回 `true` 即结束，否则走原逻辑 | 悬浮窗（`InfoPopover`）的链接点击都经 `on_url_click` 调到 `open_markdown_url`，没有给外部 crate 处理点击的接口；`editor` 不能依赖 `fork_` crate，所以用全局槽，由 `fork_ext::init` 注册并按链接 host 分发。补全文档、签名帮助里的链接也走这个函数，同样只拦截 `zed-fork://` |
| H8 | `crates/zed/src/zed.rs` | `load_default_keymap` 末尾调用 `fork_ext::load_default_keymap(cx)` | `reload_keymaps` 每次先 `clear_key_bindings()`，只在 init 时绑定一次的键位会在用户改 keymap 后丢失；放在这里，fork 键位排在官方默认之后、用户键位之前 |

| H9 | `crates/settings/src/vscode_import.rs` | `settings_content()` 的 `SettingsContent { … }` 末尾加 `fork: None` | 这里用结构体字面量列出了全部字段，加字段后不补这一行编译不过 |
| H11 | `crates/git_ui/src/git_panel.rs` | `mod fork_vscode_layout;`；`GitPanel` 加字段 `fork_layout` 并在 `new` 里初始化；`set_active_tab` 开头先调 `fork_set_active_tab`，返回 `true` 即结束；`load_commit_history` 的订阅和 `set_active_repository` 里两处 `active_tab == GitPanelTab::History` 换成 `fork_history_is_shown(cx)`；`Render` 开头算出 `fork_body`，有值时不画标签栏，并在 `match self.active_tab` 前加一个分支直接放入 `fork_body` | Git 面板的 VS Code 式布局（设计文档 9）。标签页和历史加载逻辑都在 `GitPanel` 的私有方法里，只能用子模块访问，子模块必须在这个文件里声明 |
| H12 | `crates/git_ui/src/git_panel/fork_vscode_layout.rs` | **新文件**，布局设置、状态、渲染和测试 | 作为 `git_panel` 的子模块才能用 `GitPanel` 的私有字段和方法；新文件不会冲突 |

计划中的挂钩（还没落地）：

| # | 文件 | 改动 | 时机 |
|---|---|---|---|
| H10 | `crates/zed/RELEASE_CHANNEL` | 改为 `dev` | 改为基于上游稳定版 tag 时。tag 上的值是 `stable` / `preview`，不改的话编出来的应用会和官方 `Zed.app` 共用标识和数据目录，并检查更新 |

## 冲突时怎么重放

| 挂钩 | 处理 |
|---|---|
| H1、H2 | 接受上游版本，重新加上 fork 的几行 |
| H3 | 接受上游版本，在 `edit_prediction_registry::init` 之后重新加一行 |
| H4 | 接受上游版本，在 `assign_edit_prediction_provider` 函数体开头重新加挂钩；函数签名变了就同步调整 `fork_ext::assign_edit_prediction_provider` |
| H5 | 接受上游版本，重新加 4 处：`mod`、`pub use`、字段、宏的 `options` 列表 |
| H6 | 新文件，不会冲突；派生宏或属性变化时按编译错误修改 |
| H7 | 接受上游版本，在 `open_markdown_url` 开头重新加挂钩；函数被改名或拆分时，找到悬浮窗 `on_url_click` 最终调用的函数 |
| `Cargo.lock` | 不算挂钩（`check-hooks.sh` 不检查它）。冲突时接受上游版本，再执行一次 `cargo check -p zed` 重新生成 |
| H8 | 接受上游版本，在 `load_default_keymap` 末尾重新加一行；如果上游改了默认键位的加载流程，保证 fork 键位仍在每次重新加载时、用户键位之前绑定 |
| H9 | 接受上游版本，在 `SettingsContent { … }` 字面量末尾重新加 `fork: None`。上游给 `SettingsContent` 加字段时这里最容易冲突，编译器会报 `missing field` |
| H10 | 接受上游版本（上游 tag 之间这个文件一般不变），重新改成 `dev` |
| H11 | 接受上游版本，重新加上各处 `// FORK:` 标注的行。上游新增了判断 `active_tab == GitPanelTab::History` 来决定是否加载或刷新历史的地方，也要换成 `fork_history_is_shown(cx)`；`Render` 的结构变了时，保证 VS Code 布局下不画标签栏和原来的正文 |
| H12 | 新文件，不会冲突；按编译错误跟进 `GitPanel` 私有方法的改名或签名变化 |
