//! FORK: VS Code style layout for the Git panel (`fork.git_panel.layout = "vscode"`).
//!
//! The commit editor moves to the top, the changes list sits in the middle and
//! the commit history is shown below it in a collapsible, resizable section,
//! instead of the upstream Changes / History tabs. Everything lives in this
//! file; `git_panel.rs` only calls in at the hooks listed in `fork/HOOKS.md`.
//!
//! `active_tab` keeps its upstream meaning of "which list owns the keyboard":
//! clicking a section makes it the active one, so the existing keyboard
//! navigation and key contexts keep working unchanged.

use super::*;
use gpui::{DragMoveEvent, MouseUpEvent};
use settings::{ForkGitPanelLayout, RegisterSetting, Settings, SettingsContent, SettingsStore};

/// Height of the history section when the user hasn't resized it, as a share
/// of the panel height.
const DEFAULT_HISTORY_FRACTION: f32 = 0.4;
const MIN_SECTION_HEIGHT: Pixels = px(90.);

#[derive(Clone, Copy, Debug, PartialEq, RegisterSetting)]
pub struct ForkGitPanelLayoutSettings {
    pub layout: ForkGitPanelLayout,
}

impl Settings for ForkGitPanelLayoutSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            layout: content
                .fork
                .as_ref()
                .and_then(|fork| fork.git_panel.as_ref())
                .and_then(|git_panel| git_panel.layout)
                .unwrap_or_default(),
        }
    }
}

pub(super) struct LayoutState {
    history_collapsed: bool,
    /// `None` until the user drags the divider.
    history_height: Option<Pixels>,
    _settings_subscription: Subscription,
}

impl LayoutState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<GitPanel>) -> Self {
        let mut was_layout = ForkGitPanelLayoutSettings::get_global(cx).layout;
        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |panel, window, cx| {
                let layout = ForkGitPanelLayoutSettings::get_global(cx).layout;
                if layout != was_layout {
                    was_layout = layout;
                    panel.fork_layout_changed(window, cx);
                }
            });
        Self {
            history_collapsed: false,
            history_height: None,
            _settings_subscription: settings_subscription,
        }
    }
}

struct DraggedHistoryDivider;

impl Render for DraggedHistoryDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl GitPanel {
    pub(super) fn fork_is_vscode_layout(cx: &App) -> bool {
        ForkGitPanelLayoutSettings::get_global(cx).layout == ForkGitPanelLayout::Vscode
    }

    /// Whether the commit history is on screen, so it should be loaded and kept
    /// up to date. Replaces upstream's `active_tab == GitPanelTab::History`.
    pub(super) fn fork_history_is_shown(&self, cx: &App) -> bool {
        if Self::fork_is_vscode_layout(cx) {
            !self.fork_layout.history_collapsed
        } else {
            self.active_tab == GitPanelTab::History
        }
    }

    /// Hook at the top of `set_active_tab`. Returns `true` when the VS Code
    /// layout handled the switch: both lists stay visible, so switching only
    /// moves the keyboard to the other list.
    pub(super) fn fork_set_active_tab(
        &mut self,
        tab: GitPanelTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !Self::fork_is_vscode_layout(cx) {
            return false;
        }
        if tab == GitPanelTab::History && self.fork_layout.history_collapsed {
            self.fork_set_history_collapsed(false, cx);
        }
        if self.active_tab != tab {
            self.active_tab = tab;
            self.activation_focus_handle(cx).focus(window, cx);
        }
        cx.notify();
        true
    }

    fn fork_layout_changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.fork_history_is_shown(cx) {
            self.load_commit_history(cx);
        } else {
            self.fork_unload_commit_history(cx);
        }
        cx.notify();
    }

    fn fork_unload_commit_history(&mut self, cx: &mut Context<Self>) {
        self.set_commit_history(CommitHistory::Loading, cx);
        self._repo_subscriptions.clear();
    }

    fn fork_set_history_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.fork_layout.history_collapsed == collapsed {
            return;
        }
        self.fork_layout.history_collapsed = collapsed;
        if collapsed {
            if self.active_tab == GitPanelTab::History {
                self.active_tab = GitPanelTab::Changes;
            }
            self.fork_unload_commit_history(cx);
        } else {
            self.load_commit_history(cx);
        }
        cx.notify();
    }

    /// Only moves the keyboard to the clicked list; focus itself is left to
    /// whatever was clicked.
    fn fork_focus_section(&mut self, tab: GitPanelTab, cx: &mut Context<Self>) {
        if self.active_tab == tab {
            return;
        }
        self.active_tab = tab;
        if tab == GitPanelTab::Changes {
            self.history_keyboard_nav = false;
        }
        cx.notify();
    }

    /// Replaces the body of the panel (everything inside the action handlers)
    /// when the VS Code layout is active.
    pub(super) fn fork_render_vscode_body(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_write_access = self.has_write_access(cx);
        let history_collapsed = self.fork_layout.history_collapsed;
        let commit_editor_expanded = self.commit_editor_expanded;

        let commit_area = v_flex()
            .debug_selector(|| "fork-git-commit-area".into())
            .w_full()
            .when(commit_editor_expanded, |this| this.flex_1().min_h_0())
            .when(!commit_editor_expanded, |this| this.flex_none())
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .children(self.render_footer(window, cx))
            .when(self.amend_pending, |this| {
                this.child(self.render_pending_amend(cx))
            })
            // The history section already shows the latest commit.
            .when(!self.amend_pending && history_collapsed, |this| {
                this.children(self.render_previous_commit(window, cx))
            });

        v_flex()
            .id("fork-git-panel-vscode-body")
            .size_full()
            .child(commit_area)
            .when(!commit_editor_expanded, |this| {
                this.child(self.fork_render_changes_section(has_write_access, window, cx))
                    .child(self.fork_render_history_section(window, cx))
            })
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedHistoryDivider>, _window, cx| {
                    let bounds = event.bounds;
                    let max = (bounds.size.height - MIN_SECTION_HEIGHT).max(MIN_SECTION_HEIGHT);
                    let height = (bounds.bottom() - event.event.position.y)
                        .max(MIN_SECTION_HEIGHT)
                        .min(max);
                    if this.fork_layout.history_height != Some(height) {
                        this.fork_layout.history_height = Some(height);
                        cx.notify();
                    }
                },
            ))
            .into_any_element()
    }

    fn fork_render_changes_section(
        &mut self,
        has_write_access: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_entries = !self.entries.is_empty();
        v_flex()
            .id("fork-git-panel-changes-section")
            .debug_selector(|| "fork-git-changes-section".into())
            .flex_1()
            .min_h(MIN_SECTION_HEIGHT)
            .overflow_hidden()
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.fork_focus_section(GitPanelTab::Changes, cx);
            }))
            .children(self.render_changes_header(window, cx))
            .map(|this| {
                if let Some(repo) = self.active_repository.clone()
                    && has_entries
                {
                    this.child(self.render_entries(has_write_access, repo, window, cx))
                } else {
                    this.child(self.render_empty_state(cx).into_any_element())
                }
            })
    }

    fn fork_render_history_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.fork_layout.history_collapsed;
        let history_height = self.fork_layout.history_height;
        let is_active = self.active_tab == GitPanelTab::History;
        let colors = cx.theme().colors();

        let header = h_flex()
            .id("fork-git-panel-history-header")
            .h(Tab::container_height(cx))
            .w_full()
            .flex_none()
            .pl_1()
            .pr_2()
            .gap_1()
            .justify_between()
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.fork_set_history_collapsed(!collapsed, cx);
            }))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new("History").size(LabelSize::Small).color(
                        if is_active && !collapsed {
                            Color::Default
                        } else {
                            Color::Muted
                        },
                    )),
            )
            .child(
                IconButton::new("fork-history-git-graph", IconName::GitGraph)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Open Git Graph", &crate::git_graph::Open, cx)
                    })
                    .on_click(|_, window, cx| {
                        cx.stop_propagation();
                        window.dispatch_action(crate::git_graph::Open.boxed_clone(), cx)
                    }),
            );

        let divider = div()
            .id("fork-git-panel-history-divider")
            .absolute()
            .top(px(-2.))
            .left_0()
            .w_full()
            .h(px(4.))
            .cursor_row_resize()
            .on_drag(DraggedHistoryDivider, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DraggedHistoryDivider)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.fork_layout.history_height = None;
                        cx.notify();
                    }
                }),
            );

        v_flex()
            .id("fork-git-panel-history-section")
            .debug_selector(|| "fork-git-history-section".into())
            .relative()
            .w_full()
            .border_t_1()
            .border_color(colors.border)
            // When the panel gets too short, the history shrinks first: the
            // changes list keeps its minimum height.
            .map(|this| match (collapsed, history_height) {
                (true, _) => this.flex_none(),
                (false, Some(height)) => this.h(height),
                (false, None) => this.h(relative(DEFAULT_HISTORY_FRACTION)),
            })
            .min_h(Tab::container_height(cx))
            .child(header)
            .when(!collapsed, |this| {
                this.child(
                    v_flex()
                        .id("fork-git-panel-history-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.fork_focus_section(GitPanelTab::History, cx);
                        }))
                        .child(self.render_history_tab(window, cx)),
                )
                .child(divider)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::ForkGitPanelSettingsContent;
    use std::path::Path;
    use theme::LoadThemes;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn set_layout(layout: ForkGitPanelLayout, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.fork.get_or_insert_default().git_panel =
                        Some(ForkGitPanelSettingsContent {
                            layout: Some(layout),
                        });
                });
            });
        });
        cx.run_until_parked();
    }

    async fn panel_with_one_commit(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<GitPanel>, VisualTestContext) {
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree("/root", json!({ "project": { ".git": {} } }))
            .await;
        let dot_git = Path::new(path!("/root/project/.git"));
        let sha: Oid = "0123456789012345678901234567890123456789".parse().unwrap();
        fs.with_git_state(dot_git, false, |state| {
            state.current_branch_name = None;
            state.refs.insert("HEAD".into(), sha.to_string());
            state.graph_commits = vec![Arc::new(git::repository::InitialGraphCommitData {
                sha,
                parents: SmallVec::new(),
                ref_names: Vec::new(),
            })];
        })
        .unwrap();

        let project = Project::test(fs, [Path::new(path!("/root/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.read(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .as_local()
                .unwrap()
                .scan_complete()
        })
        .await;
        cx.run_until_parked();

        set_layout(ForkGitPanelLayout::Vscode, &mut cx);
        let panel = workspace.update_in(&mut cx, GitPanel::new);
        cx.run_until_parked();
        (workspace, panel, cx)
    }

    fn history_len(panel: &GitPanel) -> Option<usize> {
        match &panel.commit_history {
            CommitHistory::Loaded(entries) => Some(entries.len()),
            CommitHistory::Loading | CommitHistory::Error(_) => None,
        }
    }

    #[gpui::test]
    async fn test_vscode_layout_loads_history_alongside_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, panel, cx) = panel_with_one_commit(cx).await;

        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.active_tab, GitPanelTab::Changes);
            assert_eq!(history_len(panel), Some(1));
        });
    }

    #[gpui::test]
    async fn test_vscode_layout_collapse_and_activate_history(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, panel, mut cx) = panel_with_one_commit(cx).await;

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.set_active_tab(GitPanelTab::History, window, cx);
            panel.fork_set_history_collapsed(true, cx);
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.active_tab, GitPanelTab::Changes);
            assert_eq!(history_len(panel), None);
        });

        // Activating the history from the keyboard expands the section again.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate_history_tab(&ActivateHistoryTab, window, cx);
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.active_tab, GitPanelTab::History);
            assert!(!panel.fork_layout.history_collapsed);
            assert_eq!(history_len(panel), Some(1));
        });

        // Moving back to the changes keeps the history on screen.
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.activate_changes_tab(&ActivateChangesTab, window, cx);
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.active_tab, GitPanelTab::Changes);
            assert_eq!(history_len(panel), Some(1));
        });
    }

    #[gpui::test]
    async fn test_switching_back_to_tabs_unloads_hidden_history(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, panel, mut cx) = panel_with_one_commit(cx).await;
        panel.read_with(&cx, |panel, _| assert_eq!(history_len(panel), Some(1)));

        set_layout(ForkGitPanelLayout::Tabs, &mut cx);
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.active_tab, GitPanelTab::Changes);
            assert_eq!(history_len(panel), None);
        });

        set_layout(ForkGitPanelLayout::Vscode, &mut cx);
        panel.read_with(&cx, |panel, _| assert_eq!(history_len(panel), Some(1)));
    }

    #[gpui::test]
    async fn test_vscode_layout_stacks_commit_changes_and_history(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, panel, mut cx) = panel_with_one_commit(cx).await;
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.open_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();

        let commit = cx.debug_bounds("fork-git-commit-area").unwrap();
        let changes = cx.debug_bounds("fork-git-changes-section").unwrap();
        let history = cx.debug_bounds("fork-git-history-section").unwrap();
        assert!(commit.bottom() <= changes.top(), "{commit:?} {changes:?}");
        assert!(changes.bottom() <= history.top(), "{changes:?} {history:?}");
        assert!(history.size.height > px(60.), "{history:?}");
        assert!(changes.size.height >= MIN_SECTION_HEIGHT, "{changes:?}");

        // Collapsing leaves only the header and gives the space to the changes.
        panel.update(&mut cx, |panel, cx| panel.fork_set_history_collapsed(true, cx));
        cx.run_until_parked();
        let collapsed_history = cx.debug_bounds("fork-git-history-section").unwrap();
        let grown_changes = cx.debug_bounds("fork-git-changes-section").unwrap();
        assert!(collapsed_history.size.height < history.size.height);
        assert!(grown_changes.size.height > changes.size.height);
    }
}
