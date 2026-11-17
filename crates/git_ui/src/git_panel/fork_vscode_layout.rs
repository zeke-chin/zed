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

mod graph;

use super::*;
use gpui::{DragMoveEvent, FontWeight, Hsla, MouseUpEvent};
use settings::{
    ForkGitPanelLayout, ForkGitPanelSettingsContent, RegisterSetting, Settings, SettingsContent,
    SettingsStore,
};

/// Height of the history section when the user hasn't resized it, as a share
/// of the panel height.
const DEFAULT_HISTORY_FRACTION: f32 = 0.4;
const MIN_SECTION_HEIGHT: Pixels = px(90.);
const SECTION_HEADER_HEIGHT: Pixels = px(24.);
const HISTORY_ROW_HEIGHT: Pixels = px(24.);
const COMMIT_EDITOR_MAX_LINES: usize = 10;
/// Wider graphs are clipped so the commit subjects stay readable.
const MAX_GRAPH_LANES: usize = 6;
const MAX_REF_BADGES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, RegisterSetting)]
pub struct ForkGitPanelLayoutSettings {
    pub layout: ForkGitPanelLayout,
    pub show_tags: bool,
    pub show_remote_branches: bool,
}

impl Settings for ForkGitPanelLayoutSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let git_panel = content
            .fork
            .as_ref()
            .and_then(|fork| fork.git_panel.as_ref());
        Self {
            layout: git_panel
                .and_then(|git_panel| git_panel.layout)
                .unwrap_or_default(),
            show_tags: git_panel
                .and_then(|git_panel| git_panel.show_tags)
                .unwrap_or(true),
            show_remote_branches: git_panel
                .and_then(|git_panel| git_panel.show_remote_branches)
                .unwrap_or(true),
        }
    }
}

pub(super) struct LayoutState {
    changes_collapsed: bool,
    history_collapsed: bool,
    /// `None` until the user drags the divider.
    history_height: Option<Pixels>,
    graph: Option<GraphCache>,
    _settings_subscription: Subscription,
}

#[derive(Clone)]
struct GraphCache {
    /// Identity of the history list the graph was computed for.
    key: usize,
    rows: Rc<[graph::GraphRow]>,
    refs: Rc<[Vec<SharedString>]>,
    max_lanes: usize,
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
            changes_collapsed: false,
            history_collapsed: false,
            history_height: None,
            graph: None,
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
        if !Self::fork_is_vscode_layout(cx) {
            self.fork_restore_commit_editor_mode(cx);
        }
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
        self.fork_sync_commit_editor_mode(cx);
        self.fork_sync_graph(cx);
        let commit_editor_expanded = self.commit_editor_expanded;

        v_flex()
            .id("fork-git-panel-vscode-body")
            .size_full()
            .child(self.fork_render_changes_section(has_write_access, window, cx))
            .when(!commit_editor_expanded, |this| {
                this.child(self.fork_render_history_section(window, cx))
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

    /// The panel shares one commit editor between both layouts; this one wants
    /// it to start at a single line and grow, like VS Code's input box.
    fn fork_sync_commit_editor_mode(&mut self, cx: &mut Context<Self>) {
        if self.commit_editor_expanded {
            return;
        }
        let mode = EditorMode::AutoHeight {
            min_lines: 1,
            max_lines: Some(COMMIT_EDITOR_MAX_LINES),
        };
        self.commit_editor.update(cx, |editor, _| {
            if editor.mode() != &mode {
                editor.set_mode(mode);
            }
        });
    }

    /// Restores upstream's fixed-height commit editor when leaving this layout.
    fn fork_restore_commit_editor_mode(&mut self, cx: &mut Context<Self>) {
        if self.commit_editor_expanded {
            return;
        }
        self.commit_editor.update(cx, |editor, _| {
            editor.set_mode(EditorMode::AutoHeight {
                min_lines: MAX_PANEL_EDITOR_LINES,
                max_lines: Some(MAX_PANEL_EDITOR_LINES),
            });
        });
    }

    /// Recomputes the graph lanes whenever the history list is reloaded. The
    /// history entries only keep SHAs, so parents and refs come from the same
    /// cached `git log` the entries were built from.
    fn fork_sync_graph(&mut self, cx: &mut Context<Self>) {
        let CommitHistory::Loaded(entries) = &self.commit_history else {
            self.fork_layout.graph = None;
            return;
        };
        let key = Rc::as_ptr(entries) as *const () as usize;
        if self
            .fork_layout
            .graph
            .as_ref()
            .is_some_and(|graph| graph.key == key)
        {
            return;
        }
        let entries = entries.clone();
        let graph = maybe!({
            let repository = self.active_repository.clone()?;
            let log_source = Self::commit_history_log_source(&repository, cx)?;
            let color_count = cx.theme().accents().0.len();
            repository.update(cx, |repository, cx| {
                let response =
                    repository.graph_data(log_source, LogOrder::DateOrder, 0..usize::MAX, cx);
                let commits = response.commits;
                let matches = commits.len() == entries.len()
                    && commits
                        .iter()
                        .zip(entries.iter())
                        .all(|(c, e)| c.sha == e.sha);
                matches.then(|| {
                    let rows = graph::compute_rows(commits, color_count);
                    let max_lanes = rows.iter().map(|row| row.width).max().unwrap_or(1);
                    let refs = commits.iter().map(|c| c.ref_names.clone()).collect();
                    GraphCache {
                        key,
                        rows: rows.into(),
                        refs,
                        max_lanes,
                    }
                })
            })
        });
        self.fork_layout.graph = graph;
    }

    fn fork_render_section_header(
        &self,
        id: &'static str,
        title: &'static str,
        collapsed: bool,
        badge: Option<usize>,
        on_toggle: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        actions: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        h_flex()
            .id(id)
            .group(id)
            .h(SECTION_HEADER_HEIGHT)
            .w_full()
            .flex_none()
            .pl_1()
            .pr_1()
            .gap_1()
            .justify_between()
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .id(SharedString::from(format!("{id}-toggle")))
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_toggle(this, cx)))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Label::new(title)
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::BOLD)
                            .single_line(),
                    )
                    .when_some(badge.filter(|count| *count > 0), |this, count| {
                        this.child(
                            div()
                                .px_1p5()
                                .rounded_full()
                                .bg(colors.element_background)
                                .child(
                                    Label::new(count.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    }),
            )
            .when_some(actions.filter(|_| !collapsed), |this, actions| {
                this.child(h_flex().flex_none().gap_0p5().child(actions))
            })
            .into_any_element()
    }

    fn fork_render_changes_section(
        &mut self,
        has_write_access: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.fork_layout.changes_collapsed;
        let commit_editor_expanded = self.commit_editor_expanded;
        let has_entries = !self.entries.is_empty();
        let show_commit_area = self.active_repository.is_some()
            && !matches!(self.git_access, Some(GitAccess::No))
            && !self.is_branch_mode();

        let header = self.fork_render_section_header(
            "fork-git-changes-header",
            "CHANGES",
            collapsed,
            Some(self.changes_count),
            |this, cx| {
                this.fork_layout.changes_collapsed = !this.fork_layout.changes_collapsed;
                cx.notify();
            },
            self.fork_render_changes_actions(cx),
            cx,
        );

        v_flex()
            .id("fork-git-panel-changes-section")
            .debug_selector(|| "fork-git-changes-section".into())
            .w_full()
            .map(|this| {
                if collapsed {
                    this.flex_none()
                } else {
                    this.flex_1().min_h(MIN_SECTION_HEIGHT)
                }
            })
            .overflow_hidden()
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.fork_focus_section(GitPanelTab::Changes, cx);
            }))
            .child(header)
            .when(!collapsed, |this| {
                this.when(show_commit_area, |this| {
                    this.child(self.fork_render_commit_area(window, cx))
                })
                .when(!commit_editor_expanded, |this| {
                    this.map(|this| {
                        if let Some(repo) = self.active_repository.clone()
                            && has_entries
                        {
                            this.child(self.render_entries(has_write_access, repo, window, cx))
                        } else if show_commit_area {
                            // VS Code shows nothing under the commit button
                            // when the working tree is clean.
                            this.child(div().flex_1())
                        } else {
                            this.child(self.render_empty_state(cx).into_any_element())
                        }
                    })
                })
            })
    }

    fn fork_render_changes_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if matches!(self.git_access, Some(GitAccess::No)) {
            return None;
        }
        self.active_repository.as_ref()?;
        let is_branch_mode = self.is_branch_mode();
        let focus_handle = self.focus_handle.clone();

        Some(
            h_flex()
                .gap_0p5()
                .child(self.render_changes_mode_menu(cx))
                .child(
                    IconButton::new("fork-view-diff", IconName::Diff)
                        .icon_size(IconSize::Small)
                        .tooltip(move |_window, cx| {
                            if is_branch_mode {
                                Tooltip::simple("View Branch Diff", cx)
                            } else {
                                Tooltip::for_action_in("View Diff", &Diff, &focus_handle, cx)
                            }
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.is_branch_mode() {
                                this.open_branch_diff(None, window, cx);
                            } else {
                                cx.defer(|cx| cx.dispatch_action(&Diff));
                            }
                        })),
                )
                .child(self.render_view_options_menu("fork-view-options-menu"))
                .when(!is_branch_mode, |this| {
                    this.child(self.render_git_changes_actions_button(cx))
                })
                .into_any_element(),
        )
    }

    fn fork_render_commit_area(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let expanded = self.commit_editor_expanded;
        let editor_focused = self.commit_editor.focus_handle(cx).is_focused(window);

        let theme_settings = ThemeSettings::get_global(cx);
        let font_size = theme_settings.ui_font_size(cx);
        let editor_style = EditorStyle {
            background: colors.editor_background,
            local_player: cx.theme().players().local(),
            text: TextStyle {
                color: colors.text,
                font_family: theme_settings.ui_font.family.clone(),
                font_fallbacks: theme_settings.ui_font.fallbacks.clone(),
                font_features: theme_settings.ui_font.features.clone(),
                font_size: AbsoluteLength::from(font_size),
                font_weight: theme_settings.ui_font.weight,
                line_height: (font_size * 1.4).into(),
                ..Default::default()
            },
            syntax: cx.theme().syntax().clone(),
            ..Default::default()
        };

        let input = h_flex()
            .id("fork-commit-input")
            .w_full()
            .items_start()
            .gap_1()
            .pl_2()
            .pr_0p5()
            .py_0p5()
            .rounded_sm()
            .border_1()
            .border_color(if editor_focused {
                colors.border_focused
            } else {
                colors.border
            })
            .bg(colors.editor_background)
            .cursor_text()
            .when(expanded, |this| this.flex_1().min_h_0())
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                window.focus(&this.commit_editor.focus_handle(cx), cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .py_0p5()
                    .when(expanded, |this| this.h_full())
                    .on_action(|&zed_actions::editor::MoveUp, _, cx| cx.stop_propagation())
                    .on_action(|&zed_actions::editor::MoveDown, _, cx| cx.stop_propagation())
                    .child(EditorElement::new(&self.commit_editor, editor_style)),
            )
            // As tall as the first line of text, so the buttons stay centered
            // on it and don't move when the message grows.
            .child(
                h_flex()
                    .flex_none()
                    .h(font_size * 1.4 + px(4.))
                    .items_center()
                    .gap_0p5()
                    .children(self.render_co_authors(cx))
                    .children(self.render_generate_commit_message_button(cx)),
            );

        v_flex()
            .debug_selector(|| "fork-git-commit-area".into())
            .w_full()
            .px_2()
            .pt_0p5()
            .pb_2()
            .gap_1p5()
            .when(expanded, |this| this.flex_1().min_h_0())
            .when(!expanded, |this| this.flex_none())
            .child(input)
            .when(self.amend_pending, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .justify_between()
                        .child(
                            Label::new("Amending the last commit")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Button::new("fork-cancel-amend", "Cancel")
                                .label_size(LabelSize::Small)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.set_amend_pending(false, cx)),
                                ),
                        ),
                )
            })
            .child(self.fork_render_commit_button(cx))
    }

    fn fork_render_commit_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (can_commit, tooltip) = self.configure_commit_button(cx);
        let title = self.commit_button_title();
        let focus_handle = self.commit_editor.focus_handle(cx);
        let options = self.commit_options();
        let status = cx.theme().status();
        let disabled = !can_commit || self.modal_open;

        let commit =
            ButtonLike::new_rounded_left("fork-commit-button")
                .full_width()
                .size(ButtonSize::Large)
                .disabled(disabled)
                .child(
                    h_flex()
                        .w_full()
                        .justify_center()
                        .gap_1()
                        .child(Icon::new(IconName::Check).size(IconSize::Small).color(
                            if disabled {
                                Color::Disabled
                            } else {
                                Color::Default
                            },
                        ))
                        .child(
                            Label::new(title)
                                .size(LabelSize::Default)
                                .color(if disabled {
                                    Color::Disabled
                                } else {
                                    Color::Default
                                }),
                        ),
                )
                .tooltip({
                    let focus_handle = focus_handle.clone();
                    move |_window, cx| {
                        if can_commit {
                            Tooltip::with_meta_in(
                                tooltip,
                                Some(&git::Commit),
                                format!(
                                    "git commit{}{}{}",
                                    if options.amend { " --amend" } else { "" },
                                    if options.signoff { " --signoff" } else { "" },
                                    if options.no_verify {
                                        " --no-verify"
                                    } else {
                                        ""
                                    }
                                ),
                                &focus_handle,
                                cx,
                            )
                        } else {
                            Tooltip::simple(tooltip, cx)
                        }
                    }
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    telemetry::event!("Git Committed", source = "Git Panel");
                    let options = this.commit_options();
                    this.commit_changes(options, window, cx);
                }));

        h_flex()
            .id("fork-commit-button-row")
            .w_full()
            .rounded_sm()
            .border_1()
            .border_color(status.info_border.opacity(if disabled { 0.5 } else { 1. }))
            .bg(status
                .info_background
                .opacity(if disabled { 0.5 } else { 1. }))
            .child(div().flex_1().min_w_0().child(commit))
            .child(div().w_px().h(px(16.)).bg(status.info_border))
            .child(self.render_git_commit_menu(
                "fork-commit-menu",
                Some(focus_handle),
                self.generate_commit_message_task.is_some(),
                cx,
            ))
    }

    fn fork_render_history_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.fork_layout.history_collapsed;
        let changes_collapsed = self.fork_layout.changes_collapsed;
        let history_height = self.fork_layout.history_height;
        let border_color = cx.theme().colors().border;

        let actions = h_flex()
            .gap_0p5()
            .children(self.render_remote_button(cx))
            .child(self.fork_render_graph_filter_menu())
            .child(
                IconButton::new("fork-history-git-graph", IconName::GitGraph)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| {
                        Tooltip::for_action("Open Git Graph", &crate::git_graph::Open, cx)
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(crate::git_graph::Open.boxed_clone(), cx)
                    }),
            )
            .into_any_element();

        let header = self.fork_render_section_header(
            "fork-git-history-header",
            "GRAPH",
            collapsed,
            None,
            move |this, cx| this.fork_set_history_collapsed(!collapsed, cx),
            Some(actions),
            cx,
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
            .border_color(border_color)
            // When the panel gets too short, the history shrinks first: the
            // changes list keeps its minimum height.
            .map(
                |this| match (collapsed, changes_collapsed, history_height) {
                    (true, _, _) => this.flex_none(),
                    (false, true, _) => this.flex_1(),
                    (false, false, Some(height)) => this.h(height),
                    (false, false, None) => this.h(relative(DEFAULT_HISTORY_FRACTION)),
                },
            )
            .min_h(SECTION_HEADER_HEIGHT)
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
                        .child(self.fork_render_history_list(window, cx)),
                )
                .when(!changes_collapsed, |this| this.child(divider))
            })
    }

    fn fork_render_graph_filter_menu(&self) -> impl IntoElement {
        let fs = self.fs.clone();
        PopoverMenu::new("fork-graph-filter-menu")
            .trigger_with_tooltip(
                IconButton::new("fork-graph-filter-trigger", IconName::Filter)
                    .icon_size(IconSize::Small),
                Tooltip::text("Graph Filters"),
            )
            .menu(move |window, cx| {
                let settings = ForkGitPanelLayoutSettings::get_global(cx);
                let show_tags = settings.show_tags;
                let show_remote_branches = settings.show_remote_branches;
                let fs = fs.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let set = |update: fn(&mut ForkGitPanelSettingsContent, bool), value: bool| {
                        let fs = fs.clone();
                        move |_: &mut Window, cx: &mut App| {
                            update_settings_file(fs.clone(), cx, move |settings, _| {
                                let git_panel = settings
                                    .fork
                                    .get_or_insert_default()
                                    .git_panel
                                    .get_or_insert_default();
                                update(git_panel, value);
                            });
                        }
                    };
                    menu.header("Show in Graph")
                        .toggleable_entry(
                            "Tags",
                            show_tags,
                            IconPosition::Start,
                            None,
                            set(|s, v| s.show_tags = Some(v), !show_tags),
                        )
                        .toggleable_entry(
                            "Remote Branches",
                            show_remote_branches,
                            IconPosition::Start,
                            None,
                            set(
                                |s, v| s.show_remote_branches = Some(v),
                                !show_remote_branches,
                            ),
                        )
                }))
            })
            .anchor(Anchor::TopRight)
    }

    fn fork_render_history_list(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let placeholder = |message: &'static str| {
            h_flex()
                .flex_1()
                .justify_center()
                .child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        };
        let Some(active_repository) = self.active_repository.clone() else {
            return placeholder("No repository found");
        };
        let entries = match &self.commit_history {
            CommitHistory::Loading => return placeholder("Loading commit history…"),
            CommitHistory::Error(_) => return placeholder("Failed to load commit history"),
            CommitHistory::Loaded(entries) if entries.is_empty() => {
                return placeholder("No commits yet");
            }
            CommitHistory::Loaded(entries) => entries.clone(),
        };

        let filter = *ForkGitPanelLayoutSettings::get_global(cx);
        let graph = self.fork_layout.graph.clone();
        let graph_lanes = graph
            .as_ref()
            .map_or(1, |graph| graph.max_lanes.min(MAX_GRAPH_LANES));
        let repository = active_repository.read(cx);
        let head_branch: Option<SharedString> = repository
            .branch
            .as_ref()
            .map(|branch| branch.name().to_string().into());
        let mut remote_names: Vec<SharedString> = vec!["origin".into(), "upstream".into()];
        if let Some(remote) = repository
            .branch
            .as_ref()
            .and_then(|branch| branch.upstream.as_ref())
            .and_then(|upstream| upstream.remote_name())
        {
            remote_names.push(remote.to_string().into());
        }
        let ahead_count = repository
            .branch
            .as_ref()
            .and_then(|branch| branch.upstream.as_ref())
            .and_then(|upstream| upstream.tracking.status())
            .map_or(0, |status| status.ahead as usize);

        let repo_weak = active_repository.downgrade();
        let workspace = self.workspace.clone();
        let git_panel = cx.weak_entity();
        let scroll_handle = self.commit_history_scroll_handle.clone();
        let focused_entry = self.focused_history_entry;
        let active_sha = self.active_history_sha;
        let is_panel_focused = self.focus_handle.is_focused(window);
        let show_focus_border = self.history_keyboard_nav;
        let has_context_menu = self.context_menu.is_some();
        let context_menu_target = self
            .context_menu
            .as_ref()
            .and_then(|context_menu| context_menu.target_entry_index);

        v_flex()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .child(
                uniform_list(
                    "fork-commit-history",
                    entries.len(),
                    move |range, _window, cx| {
                        let date_style = GitPanelSettings::get_global(cx).date_style;
                        let theme = cx.theme().clone();
                        let colors = theme.colors();
                        let accents = theme.accents().clone();
                        let commit_data: Vec<Option<Arc<CommitData>>> = repo_weak
                            .update(cx, |repository, cx| {
                                entries[range.clone()]
                                    .iter()
                                    .map(|entry| {
                                        match repository.fetch_commit_data(entry.sha, false, cx) {
                                            CommitDataState::Loaded(data) => Some(data.clone()),
                                            CommitDataState::Loading(_) => None,
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        range
                            .clone()
                            .zip(commit_data)
                            .map(|(index, data)| {
                                let entry = &entries[index];
                                let sha: SharedString = entry.sha.to_string().into();
                                let short_sha: SharedString =
                                    sha[..7.min(sha.len())].to_string().into();
                                let (subject, author, timestamp) = match &data {
                                    Some(data) => (
                                        data.subject.clone(),
                                        data.author_name.clone(),
                                        Some(data.commit_timestamp),
                                    ),
                                    None => ("Loading…".into(), SharedString::default(), None),
                                };
                                let time: SharedString = timestamp
                                    .and_then(|ts| {
                                        time::OffsetDateTime::from_unix_timestamp(ts).ok()
                                    })
                                    .map(|dt| {
                                        crate::git_timestamp::format_git_date(dt, date_style, cx)
                                            .into()
                                    })
                                    .unwrap_or_default();

                                let is_head = index == 0;
                                let is_focused = focused_entry == Some(index);
                                let is_active = active_sha == Some(entry.sha);
                                let is_unpushed = index < ahead_count;
                                let row_graph = graph
                                    .as_ref()
                                    .and_then(|graph| graph.rows.get(index).cloned());
                                let refs = graph
                                    .as_ref()
                                    .and_then(|graph| graph.refs.get(index).cloned())
                                    .unwrap_or_default();
                                let badge_color =
                                    row_graph.as_ref().map_or(colors.text_accent, |row| {
                                        accents.color_for_index(row.color as u32)
                                    });

                                h_flex()
                                    .id(("fork-commit-history-item", index))
                                    .h(HISTORY_ROW_HEIGHT)
                                    .w_full()
                                    .pr_2()
                                    .gap_1()
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(gpui::transparent_black())
                                    .when(
                                        is_focused && is_panel_focused && show_focus_border,
                                        |this| this.border_color(colors.panel_focused_border),
                                    )
                                    .map(|this| {
                                        if is_active {
                                            this.bg(colors.element_selected)
                                        } else if context_menu_target == Some(index) {
                                            this.bg(colors.element_hover)
                                        } else {
                                            this.hover(|style| style.bg(colors.element_hover))
                                        }
                                    })
                                    .map(|this| match row_graph {
                                        Some(row) => this.child(graph::render_row_graph(
                                            row,
                                            graph_lanes,
                                            is_head,
                                            accents.clone(),
                                            colors.panel_background,
                                        )),
                                        None => {
                                            this.child(div().flex_none().w(graph::graph_width(1)))
                                        }
                                    })
                                    .child(
                                        h_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap_1p5()
                                            .child(
                                                div().min_w_0().child(
                                                    Label::new(subject.clone())
                                                        .size(LabelSize::Default)
                                                        .when(is_head, |label| {
                                                            label.weight(FontWeight::BOLD)
                                                        })
                                                        .single_line()
                                                        .truncate(),
                                                ),
                                            )
                                            .when(!author.is_empty(), |this| {
                                                this.child(
                                                    div().flex_shrink_0().max_w(px(120.)).child(
                                                        Label::new(author.clone())
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted)
                                                            .single_line()
                                                            .truncate(),
                                                    ),
                                                )
                                            }),
                                    )
                                    .child(render_ref_badges(
                                        &refs,
                                        head_branch.as_ref(),
                                        &remote_names,
                                        filter,
                                        is_unpushed,
                                        badge_color,
                                        cx,
                                    ))
                                    .when(!has_context_menu, |this| {
                                        let meta: SharedString =
                                            [author.as_ref(), time.as_ref(), short_sha.as_ref()]
                                                .into_iter()
                                                .filter(|part| !part.is_empty())
                                                .collect::<Vec<_>>()
                                                .join(" · ")
                                                .into();
                                        let title = subject.clone();
                                        this.tooltip(move |_, cx| {
                                            Tooltip::with_meta(
                                                title.clone(),
                                                None,
                                                meta.clone(),
                                                cx,
                                            )
                                        })
                                    })
                                    .on_mouse_down(MouseButton::Left, {
                                        let git_panel = git_panel.clone();
                                        move |_, _, cx| {
                                            git_panel
                                                .update(cx, |panel, cx| {
                                                    panel.focused_history_entry = Some(index);
                                                    panel.history_keyboard_nav = false;
                                                    cx.notify();
                                                })
                                                .ok();
                                        }
                                    })
                                    .on_mouse_down(MouseButton::Right, {
                                        let git_panel = git_panel.clone();
                                        move |event, window, cx| {
                                            git_panel
                                                .update(cx, |panel, cx| {
                                                    panel.deploy_history_context_menu(
                                                        event.position,
                                                        index,
                                                        window,
                                                        cx,
                                                    );
                                                })
                                                .ok();
                                            cx.stop_propagation();
                                        }
                                    })
                                    .on_click({
                                        let workspace = workspace.clone();
                                        let repo = repo_weak.clone();
                                        move |_, window, cx| {
                                            CommitView::open(
                                                sha.to_string(),
                                                repo.clone(),
                                                workspace.clone(),
                                                Default::default(),
                                                window,
                                                cx,
                                            );
                                        }
                                    })
                                    .into_any_element()
                            })
                            .collect()
                    },
                )
                .size_full()
                .track_scroll(&scroll_handle),
            )
            .vertical_scrollbar_for(&scroll_handle, window, cx)
            .into_any_element()
    }
}

/// Branch, remote and tag badges at the end of a history row, like VS Code's
/// graph: the checked-out branch first, a remote that points at the same
/// commit as a local branch collapses into a small cloud icon.
fn render_ref_badges(
    refs: &[SharedString],
    head_branch: Option<&SharedString>,
    remote_names: &[SharedString],
    filter: ForkGitPanelLayoutSettings,
    is_unpushed: bool,
    color: Hsla,
    cx: &App,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let mut branches: Vec<(SharedString, bool)> = Vec::new();
    let mut remotes: Vec<SharedString> = Vec::new();
    let mut tags: Vec<SharedString> = Vec::new();
    for decoration in refs {
        if let Some(tag) = decoration.strip_prefix("tag: ") {
            tags.push(tag.to_string().into());
        } else if let Some(branch) = decoration.strip_prefix("HEAD -> ") {
            branches.push((branch.to_string().into(), true));
        } else if decoration.as_ref() == "HEAD" {
            continue;
        } else if remote_names.iter().any(|remote| {
            decoration
                .strip_prefix(remote.as_ref())
                .is_some_and(|rest| rest.starts_with('/'))
        }) {
            remotes.push(decoration.clone());
        } else {
            let is_head = head_branch.is_some_and(|head| head == decoration);
            branches.push((decoration.clone(), is_head));
        }
    }
    branches.sort_by_key(|(_, is_head)| !is_head);
    if !filter.show_tags {
        tags.clear();
    }
    if !filter.show_remote_branches {
        remotes.clear();
    }

    // A remote branch next to its local branch only gets an icon.
    let (paired_remotes, lone_remotes): (Vec<_>, Vec<_>) =
        remotes.into_iter().partition(|remote| {
            branches.iter().any(|(branch, _)| {
                remote
                    .split_once('/')
                    .is_some_and(|(_, name)| name == branch.as_ref())
            })
        });

    let pill = |id: SharedString, icon: IconName, label: SharedString, emphasized: bool| {
        h_flex()
            .id(id)
            .h(px(18.))
            .max_w(px(140.))
            .min_w_0()
            .px_1()
            .gap_0p5()
            .rounded_full()
            .border_1()
            .border_color(color.opacity(0.6))
            .bg(color.opacity(if emphasized { 0.35 } else { 0.15 }))
            .child(
                Icon::new(icon)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(color)),
            )
            .child(
                Label::new(label.clone())
                    .size(LabelSize::XSmall)
                    .single_line()
                    .truncate(),
            )
            .tooltip(Tooltip::text(label))
    };

    let mut badges: Vec<AnyElement> = Vec::new();
    for (branch, is_head) in &branches {
        badges.push(
            pill(
                format!("fork-ref-branch-{branch}").into(),
                if *is_head {
                    IconName::Check
                } else {
                    IconName::GitBranch
                },
                branch.clone(),
                *is_head,
            )
            .into_any_element(),
        );
    }
    for remote in &lone_remotes {
        badges.push(
            pill(
                format!("fork-ref-remote-{remote}").into(),
                IconName::CloudDownload,
                remote.clone(),
                false,
            )
            .into_any_element(),
        );
    }
    for tag in &tags {
        badges.push(
            pill(
                format!("fork-ref-tag-{tag}").into(),
                IconName::Bookmark,
                tag.clone(),
                false,
            )
            .into_any_element(),
        );
    }

    let hidden = badges.len().saturating_sub(MAX_REF_BADGES);
    let hidden_names: SharedString = branches
        .iter()
        .map(|(name, _)| name.clone())
        .chain(lone_remotes.iter().cloned())
        .chain(tags.iter().cloned())
        .skip(MAX_REF_BADGES)
        .collect::<Vec<_>>()
        .join("\n")
        .into();
    badges.truncate(MAX_REF_BADGES);

    h_flex()
        .flex_none()
        .gap_0p5()
        .children(badges)
        .when(hidden > 0, |this| {
            this.child(
                div()
                    .id("fork-ref-hidden")
                    .px_1()
                    .rounded_full()
                    .bg(colors.element_background)
                    .child(
                        Label::new(format!("+{hidden}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(hidden_names)),
            )
        })
        .when(!paired_remotes.is_empty(), |this| {
            let names: SharedString = paired_remotes.join("\n").into();
            this.child(
                div()
                    .id("fork-ref-paired-remotes")
                    .size(px(16.))
                    .flex_none()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(color.opacity(0.15))
                    .border_1()
                    .border_color(color.opacity(0.6))
                    .child(
                        Icon::new(IconName::CloudDownload)
                            .size(IconSize::XSmall)
                            .color(Color::Custom(color)),
                    )
                    .tooltip(Tooltip::text(names)),
            )
        })
        .when(is_unpushed, |this| {
            this.child(
                div()
                    .id("fork-ref-unpushed")
                    .flex_none()
                    .child(
                        Icon::new(IconName::ArrowUp)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text("Not pushed yet")),
            )
        })
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
                            ..Default::default()
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
        // The commit box sits at the top of the changes section, the history
        // below it.
        assert!(changes.top() <= commit.top(), "{changes:?} {commit:?}");
        assert!(
            commit.bottom() <= changes.bottom(),
            "{commit:?} {changes:?}"
        );
        assert!(changes.bottom() <= history.top(), "{changes:?} {history:?}");
        assert!(history.size.height > px(60.), "{history:?}");
        assert!(changes.size.height >= MIN_SECTION_HEIGHT, "{changes:?}");

        // Collapsing leaves only the header and gives the space to the changes.
        panel.update(&mut cx, |panel, cx| {
            panel.fork_set_history_collapsed(true, cx)
        });
        cx.run_until_parked();
        let collapsed_history = cx.debug_bounds("fork-git-history-section").unwrap();
        let grown_changes = cx.debug_bounds("fork-git-changes-section").unwrap();
        assert!(collapsed_history.size.height < history.size.height);
        assert!(grown_changes.size.height > changes.size.height);
    }
}
