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
use gpui::{DragMoveEvent, EntityId, FontWeight, Hsla, MouseUpEvent};
use settings::{
    ForkGitPanelLayout, ForkGitPanelSettingsContent, ProfileBase, RegisterSetting, Settings,
    SettingsContent, SettingsFile, SettingsStore, UserSettingsContentExt,
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
const DEFAULT_GRAPH_PAGE_SIZE: usize = 50;
/// Hover group of a changes list row, so its inline actions show on hover.
pub(super) const FORK_ENTRY_GROUP: &str = "fork-git-entry";

/// Hook in `GitPanelSettings::from_settings`: VS Code splits the changes into
/// "Staged Changes" and "Changes", so the file status grouping becomes the
/// staging one. Applied to the setting itself so the project diff orders its
/// files the same way.
pub(crate) fn fork_group_by(
    content: &SettingsContent,
    group_by: GitPanelGroupBy,
) -> GitPanelGroupBy {
    let is_vscode_layout = content
        .fork
        .as_ref()
        .and_then(|fork| fork.git_panel.as_ref())
        .and_then(|git_panel| git_panel.layout)
        == Some(ForkGitPanelLayout::Vscode);
    if is_vscode_layout && group_by == GitPanelGroupBy::Status {
        GitPanelGroupBy::Staging
    } else {
        group_by
    }
}

fn fork_commit_editor_font_size(rem_size: Pixels, cx: &App) -> Pixels {
    let settings = ThemeSettings::get_global(cx);
    if cx.has_global::<theme_settings::GitCommitBufferFontSize>() {
        return settings.git_commit_buffer_font_size(cx);
    }

    // The upstream default is 12px, so the merged theme cannot tell whether it was configured.
    let store = cx.global::<SettingsStore>();
    let has_font_size =
        |content: &SettingsContent| content.theme.git_commit_buffer_font_size.is_some();
    let configured_in_user_settings = store.raw_user_settings().is_some_and(|user_settings| {
        let profile = user_settings.for_profile(cx);
        profile.is_some_and(|profile| has_font_size(&profile.settings))
            || (profile.is_none_or(|profile| profile.base == ProfileBase::User)
                && [
                    Some(user_settings.content.as_ref()),
                    user_settings.for_release_channel(),
                    user_settings.for_os(),
                ]
                .into_iter()
                .flatten()
                .any(has_font_size))
    });
    let configured_in_other_settings = [SettingsFile::Global, SettingsFile::Server]
        .into_iter()
        .filter_map(|file| store.get_content_for_file(file))
        .any(has_font_size);
    if configured_in_user_settings || configured_in_other_settings {
        settings.git_commit_buffer_font_size(cx)
    } else {
        TextSize::Default.rems(cx).to_pixels(rem_size)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, RegisterSetting)]
pub struct ForkGitPanelLayoutSettings {
    pub layout: ForkGitPanelLayout,
    pub show_tags: bool,
    pub show_remote_branches: bool,
    pub graph_page_size: usize,
    pub graph_page_on_scroll: bool,
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
            graph_page_size: git_panel
                .and_then(|git_panel| git_panel.graph_page_size)
                .unwrap_or(DEFAULT_GRAPH_PAGE_SIZE)
                .max(1),
            graph_page_on_scroll: git_panel
                .and_then(|git_panel| git_panel.graph_page_on_scroll)
                .unwrap_or(false),
        }
    }
}

pub(super) struct LayoutState {
    changes_collapsed: bool,
    history_collapsed: bool,
    /// `None` until the user drags the divider.
    history_height: Option<Pixels>,
    graph: Option<GraphCache>,
    /// How many commits of the history are shown; grows by a page on "Show
    /// More".
    history_limit: usize,
    /// Whether `git log` returned more commits than `history_limit`.
    history_has_more: bool,
    /// The repository and log source `history_limit` was counted for, so
    /// switching either starts again from the first page.
    history_source: Option<(EntityId, LogSource)>,
    _settings_subscription: Subscription,
    _window_activation_subscription: Subscription,
    /// Set on the first render: the panel's focus handle doesn't exist yet
    /// when this state is built.
    focus_in_subscription: Option<Subscription>,
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
        let settings = *ForkGitPanelLayoutSettings::get_global(cx);
        let mut was_layout = settings.layout;
        let mut was_page_size = settings.graph_page_size;
        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |panel, window, cx| {
                let settings = *ForkGitPanelLayoutSettings::get_global(cx);
                let (layout, page_size) = (settings.layout, settings.graph_page_size);
                if layout != was_layout {
                    was_layout = layout;
                    panel.fork_layout_changed(window, cx);
                }
                if page_size != was_page_size {
                    was_page_size = page_size;
                    panel.fork_layout.history_source = None;
                    if panel.fork_history_is_shown(cx) {
                        panel.fetch_commit_history_entries(cx);
                    }
                }
            });
        // VS Code refreshes the status when its window comes back into focus,
        // which picks up commands run in a terminal in the meantime.
        let window_activation_subscription =
            cx.observe_window_activation(window, |panel, window, cx| {
                if window.is_window_active() {
                    panel.fork_rescan_repository(cx);
                }
            });
        Self {
            changes_collapsed: false,
            history_collapsed: false,
            history_height: None,
            graph: None,
            history_limit: settings.graph_page_size,
            history_has_more: false,
            history_source: None,
            _settings_subscription: settings_subscription,
            _window_activation_subscription: window_activation_subscription,
            focus_in_subscription: None,
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

    /// Re-reads the repository status. File events miss index changes when
    /// `.git` is a symlink to a directory outside the worktree, so the panel
    /// also refreshes after its own git operations, on window focus and from
    /// the refresh button, like VS Code.
    pub(super) fn fork_rescan_repository(&self, cx: &mut Context<Self>) {
        if let Some(repository) = self.active_repository.clone() {
            repository.update(cx, |repository, cx| repository.fork_rescan(cx));
        }
    }

    fn fork_unload_commit_history(&mut self, cx: &mut Context<Self>) {
        self.fork_layout.history_source = None;
        self.set_commit_history(CommitHistory::Loading, cx);
        self._repo_subscriptions.clear();
    }

    /// Hook in `fetch_commit_history_entries`: the VS Code layout only keeps
    /// the first `history_limit` commits, so the keyboard navigation and the
    /// context menu upstream only ever see the commits on screen.
    pub(super) fn fork_limit_history(
        &mut self,
        entries: Rc<[CommitHistoryEntry]>,
        cx: &mut Context<Self>,
    ) -> Rc<[CommitHistoryEntry]> {
        if !Self::fork_is_vscode_layout(cx) {
            return entries;
        }
        let source = self.active_repository.as_ref().and_then(|repository| {
            Some((
                repository.entity_id(),
                Self::commit_history_log_source(repository, cx)?,
            ))
        });
        if self.fork_layout.history_source != source {
            self.fork_layout.history_source = source;
            self.fork_layout.history_limit =
                ForkGitPanelLayoutSettings::get_global(cx).graph_page_size;
        }
        let limit = self.fork_layout.history_limit;
        let has_more = entries.len() > limit;
        if self.fork_layout.history_has_more != has_more {
            self.fork_layout.history_has_more = has_more;
            cx.notify();
        }
        match entries.get(..limit) {
            Some(shown) if has_more => Rc::from(shown),
            _ => entries,
        }
    }

    /// Shows the next page of the history. Only acts when the limit is still
    /// `shown_limit`, so a request made twice for the same page (the
    /// auto-loading row can be laid out more than once) adds one page.
    fn fork_show_more_history(&mut self, shown_limit: usize, cx: &mut Context<Self>) {
        if self.fork_layout.history_limit != shown_limit || !self.fork_layout.history_has_more {
            return;
        }
        self.fork_layout.history_limit +=
            ForkGitPanelLayoutSettings::get_global(cx).graph_page_size;
        self.fetch_commit_history_entries(cx);
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
        if self.fork_layout.focus_in_subscription.is_none() {
            // Switching to the panel or clicking into it refreshes the status,
            // which picks up git commands run in Zed's own terminal.
            let focus_handle = self.focus_handle.clone();
            self.fork_layout.focus_in_subscription =
                Some(cx.on_focus_in(&focus_handle, window, |panel, _window, cx| {
                    panel.fork_rescan_repository(cx);
                }));
        }
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
                // The VS Code layout only shows the first page of the log.
                let commits = commits.get(..entries.len())?;
                let matches = commits
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
                .child(
                    IconButton::new("fork-refresh-status", IconName::RotateCw)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Refresh"))
                        .on_click(cx.listener(|this, _, _, cx| this.fork_rescan_repository(cx))),
                )
                .when(!is_branch_mode, |this| {
                    this.child(self.render_git_changes_actions_button(cx))
                })
                .into_any_element(),
        )
    }

    pub(super) fn fork_handle_font_size_action(
        &mut self,
        persist: bool,
        delta: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !Self::fork_is_vscode_layout(cx) {
            return false;
        }

        let rem_size = window.rem_size();
        if persist {
            update_settings_file(self.fs.clone(), cx, move |settings, cx| {
                // Resolve inside the queued update so repeated shortcuts build on each other.
                let font_size = fork_commit_editor_font_size(rem_size, cx) + delta;
                settings.theme.git_commit_buffer_font_size =
                    Some(f32::from(theme_settings::clamp_font_size(font_size)).into());
            });
        } else {
            let font_size = fork_commit_editor_font_size(rem_size, cx) + delta;
            theme_settings::adjust_git_commit_buffer_font_size(cx, |_| font_size);
        }
        true
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
        let font_size = fork_commit_editor_font_size(window.rem_size(), cx);
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
        let has_more = self.fork_layout.history_has_more;
        let shown_limit = self.fork_layout.history_limit;
        let commit_count = entries.len();

        v_flex()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .child(
                uniform_list(
                    "fork-commit-history",
                    commit_count + usize::from(has_more),
                    move |range, _window, cx| {
                        let show_more_row =
                            (has_more && range.contains(&commit_count)).then(|| {
                                render_show_more_row(
                                    git_panel.clone(),
                                    shown_limit,
                                    graph_lanes,
                                    cx,
                                )
                            });
                        let range = range.start.min(commit_count)..range.end.min(commit_count);
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
                            .chain(show_more_row)
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

/// Inline actions of the changes list, like VS Code's source control view:
/// group headers and rows get icon buttons that show on hover, followed by
/// the file count or the status letter.
impl GitPanel {
    /// Hook in `render_list_header`.
    pub(super) fn fork_section_title(section: Section, cx: &App) -> Option<&'static str> {
        if !Self::fork_is_vscode_layout(cx) {
            return None;
        }
        match section {
            Section::Conflict => Some("Merge Changes"),
            Section::Staged => Some("Staged Changes"),
            Section::Tracked | Section::Unstaged => Some("Changes"),
            Section::New => Some("Untracked Changes"),
            Section::Branch => None,
        }
    }

    /// Hook in `render_list_header`.
    pub(super) fn fork_render_header_actions(
        &self,
        section: Section,
        file_count: usize,
        section_is_empty: bool,
        has_write_access: bool,
        all_conflicts_resolved: bool,
        group_name: SharedString,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !Self::fork_is_vscode_layout(cx) || self.is_branch_mode() {
            return None;
        }
        let read_only = !has_write_access;
        let stage = match section {
            Section::Staged => Some(false),
            Section::Unstaged => Some(true),
            Section::Conflict if all_conflicts_resolved => None,
            Section::Conflict => Some(true),
            Section::Tracked | Section::New => Some(!self.header_state(section).selected()),
            Section::Branch => None,
        };
        let view_title = match section {
            Section::Tracked => Some("Open Changes"),
            Section::Unstaged => Some("Open Unstaged Changes"),
            Section::Staged => Some("Open Staged Changes"),
            Section::New | Section::Conflict | Section::Branch => None,
        };
        let can_stash = matches!(section, Section::Tracked | Section::Unstaged);
        let discard_title = match section {
            Section::Tracked | Section::Unstaged => Some("Discard All Changes"),
            Section::New => Some("Trash All Untracked Files"),
            Section::Staged | Section::Conflict | Section::Branch => None,
        };
        let id = |name: &str| SharedString::from(format!("fork-header-{name}-{section:?}"));

        let actions = h_flex()
            .flex_none()
            .gap_0p5()
            .visible_on_hover(group_name)
            .when(can_stash, |this| {
                this.child(fork_inline_button(
                    id("stash"),
                    IconName::Download,
                    "Stash All Changes",
                    read_only,
                    cx,
                    |this, window, cx| this.stash_all(&StashAll, window, cx),
                ))
            })
            .when_some(view_title, |this, title| {
                this.child(fork_inline_button(
                    id("view"),
                    IconName::FileDiff,
                    title,
                    false,
                    cx,
                    move |this, window, cx| this.fork_open_section_diff(section, window, cx),
                ))
            })
            .when_some(discard_title, |this, title| {
                this.child(fork_inline_button(
                    id("discard"),
                    IconName::Undo,
                    title,
                    read_only,
                    cx,
                    move |this, window, cx| this.fork_discard_section(section, window, cx),
                ))
            })
            .when_some(stage, |this, stage| {
                let (icon, title, intent) = if stage {
                    (IconName::Plus, "Stage All Changes", StageIntent::Stage)
                } else {
                    (IconName::Dash, "Unstage All Changes", StageIntent::Unstage)
                };
                this.child(fork_inline_button(
                    id("stage"),
                    icon,
                    title,
                    read_only,
                    cx,
                    move |this, window, cx| {
                        this.toggle_staged_for_entry(
                            &GitListEntry::Header(GitHeaderEntry { header: section }),
                            intent,
                            window,
                            cx,
                        )
                    },
                ))
            });

        Some(
            h_flex()
                .flex_none()
                .gap_1()
                .when(!section_is_empty, |this| this.child(actions))
                // VS Code keeps showing "0" on the group a file just left.
                .when(
                    file_count > 0 || matches!(section, Section::Staged | Section::Unstaged),
                    |this| this.child(fork_count_badge(file_count, cx)),
                )
                .into_any_element(),
        )
    }

    /// Hook in `render_status_entry`. The diff stat goes in front of the
    /// actions so it stays where it is when they appear.
    pub(super) fn fork_render_status_actions(
        &self,
        ix: usize,
        entry: &GitStatusEntry,
        selected: bool,
        has_write_access: bool,
        repo: &Repository,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !Self::fork_is_vscode_layout(cx) || self.is_branch_mode() {
            return None;
        }
        let read_only = !has_write_access;
        let status = entry.status;
        let stage_status = GitPanel::stage_status_for_entry(entry, repo);
        let stage = self
            .stage_intent_for_entry_index(ix)
            .resolve_with(|| stage_status);
        // Unstaging a resolved conflict would drop its merge stages, so the
        // upstream checkbox is locked too.
        let can_toggle_stage = !self.is_resolved_conflict(ix, cx);
        let discard_title = if status.is_deleted() {
            "Restore File"
        } else if status.is_created() && !status.staging().has_staged() {
            "Trash File"
        } else {
            "Discard Changes"
        };
        let id = |name: &str| SharedString::from(format!("fork-entry-{name}-{ix}"));

        let actions = h_flex()
            .flex_none()
            .gap_0p5()
            .when(!selected, |this| this.visible_on_hover(FORK_ENTRY_GROUP))
            .when(!status.is_deleted(), |this| {
                this.child(fork_inline_button_from_path(
                    id("open"),
                    "icons/fork/go_to_file.svg",
                    "Open File",
                    false,
                    cx,
                    move |this, window, cx| {
                        this.fork_select_for_inline_action(ix);
                        this.view_file(&Default::default(), window, cx);
                    },
                ))
            })
            .child(fork_inline_button(
                id("discard"),
                IconName::Undo,
                discard_title,
                read_only,
                cx,
                move |this, window, cx| {
                    this.fork_select_for_inline_action(ix);
                    this.revert_selected(&RestoreFile::default(), window, cx);
                },
            ))
            .when(can_toggle_stage, |this| {
                this.child(fork_stage_button(id("stage"), ix, stage, read_only, cx))
            });

        let diff_stat = GitPanelSettings::get_global(cx)
            .diff_stats
            .then_some(entry.diff_stat)
            .flatten();

        Some(
            h_flex()
                .flex_none()
                .gap_1()
                .when_some(diff_stat, |this, stat| {
                    this.child(ui::DiffStat::new(
                        id("diff-stat"),
                        stat.added as usize,
                        stat.deleted as usize,
                    ))
                })
                .child(actions)
                .child(fork_status_letter(status))
                .into_any_element(),
        )
    }

    /// Hook in `render_directory_entry`.
    pub(super) fn fork_render_directory_actions(
        &self,
        ix: usize,
        entry: &GitTreeDirEntry,
        selected: bool,
        stage_status: StageStatus,
        has_write_access: bool,
        resolved_conflict: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !Self::fork_is_vscode_layout(cx) || self.is_branch_mode() {
            return None;
        }
        let read_only = !has_write_access;
        let stage = StageIntent::for_section(entry.key.section).resolve_with(|| stage_status);
        let id = |name: &str| SharedString::from(format!("fork-dir-{name}-{ix}"));

        Some(
            h_flex()
                .flex_none()
                .gap_0p5()
                .when(!selected, |this| this.visible_on_hover(FORK_ENTRY_GROUP))
                .child(fork_inline_button(
                    id("discard"),
                    IconName::Undo,
                    "Discard Changes",
                    read_only,
                    cx,
                    move |this, window, cx| {
                        this.fork_select_for_inline_action(ix);
                        this.revert_selected(&RestoreFile::default(), window, cx);
                    },
                ))
                .when(!resolved_conflict, |this| {
                    this.child(fork_stage_button(id("stage"), ix, stage, read_only, cx))
                })
                .into_any_element(),
        )
    }

    /// Inline row actions act on the clicked row, or on the whole selection
    /// when the row is part of it, the same way the context menu does.
    fn fork_select_for_inline_action(&mut self, ix: usize) {
        if !self.row_is_marked(ix) {
            self.clear_marks();
        }
        self.selected_entry = Some(ix);
    }

    fn fork_open_section_diff(
        &mut self,
        section: Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .update(cx, |workspace, cx| match section {
                Section::Staged => StagedDiff::deploy_at(workspace, None, window, cx),
                Section::Unstaged => UnstagedDiff::deploy_at(workspace, None, window, cx),
                _ => ProjectDiff::deploy_at(workspace, None, window, cx),
            })
            .log_err();
    }

    fn fork_discard_section(
        &mut self,
        section: Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.active_repository.clone() else {
            return;
        };
        // The "Unstaged" group only owns the working tree half of a partially
        // staged file.
        let scope = if section == Section::Unstaged {
            DiscardScope::Unstaged
        } else {
            DiscardScope::All
        };
        let header = GitHeaderEntry { header: section };
        let targets = {
            let repository = repository.read(cx);
            self.change_entries
                .iter()
                .filter(|entry| header.contains(entry, repository))
                .map(|entry| DiscardTarget::new(entry.clone(), scope))
                .collect::<Vec<_>>()
        };
        self.revert_entries(targets, false, window, cx);
    }
}

fn fork_stage_button(
    id: SharedString,
    ix: usize,
    stage: bool,
    disabled: bool,
    cx: &Context<GitPanel>,
) -> IconButton {
    let (icon, title) = if stage {
        (IconName::Plus, "Stage Changes")
    } else {
        (IconName::Dash, "Unstage Changes")
    };
    fork_inline_button(id, icon, title, disabled, cx, move |this, window, cx| {
        this.fork_select_for_inline_action(ix);
        if stage {
            this.stage_selected(&git::StageFile, window, cx);
        } else {
            this.unstage_selected(&git::UnstageFile, window, cx);
        }
    })
}

/// The buttons sit inside clickable rows and headers, so they stop the click
/// from also opening the file or collapsing the group.
fn fork_inline_button(
    id: SharedString,
    icon: IconName,
    tooltip: &'static str,
    disabled: bool,
    cx: &Context<GitPanel>,
    handler: impl Fn(&mut GitPanel, &mut Window, &mut Context<GitPanel>) + 'static,
) -> IconButton {
    IconButton::new(id, icon)
        .icon_size(IconSize::Small)
        .disabled(disabled)
        .tooltip(Tooltip::text(tooltip))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            handler(this, window, cx);
        }))
}

/// Same as [`fork_inline_button`] for icons that aren't in `IconName`.
fn fork_inline_button_from_path(
    id: SharedString,
    icon_path: &'static str,
    tooltip: &'static str,
    disabled: bool,
    cx: &Context<GitPanel>,
    handler: impl Fn(&mut GitPanel, &mut Window, &mut Context<GitPanel>) + 'static,
) -> ButtonLike {
    ButtonLike::new(id)
        .style(ButtonStyle::Subtle)
        .size(ButtonSize::None)
        .disabled(disabled)
        .child(
            h_flex()
                .p_0p5()
                .child(
                    Icon::from_path(icon_path)
                        .size(IconSize::Small)
                        .color(if disabled {
                            Color::Disabled
                        } else {
                            Color::Muted
                        }),
                ),
        )
        .tooltip(Tooltip::text(tooltip))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            cx.stop_propagation();
            handler(this, window, cx);
        }))
}

fn fork_count_badge(count: usize, cx: &App) -> impl IntoElement {
    h_flex()
        .flex_none()
        .h(px(16.))
        .min_w(px(16.))
        .px_1()
        .justify_center()
        .rounded_full()
        .bg(cx.theme().colors().element_background)
        .child(Label::new(count.to_string()).size(LabelSize::XSmall))
}

/// VS Code's one-letter status decoration.
fn fork_status_letter(status: FileStatus) -> impl IntoElement {
    let (letter, color) = if status.is_conflicted() {
        ("!", Color::VersionControlConflict)
    } else if status.is_untracked() {
        ("U", Color::VersionControlAdded)
    } else if status.is_created() {
        ("A", Color::VersionControlAdded)
    } else if status.is_deleted() {
        ("D", Color::VersionControlDeleted)
    } else {
        ("M", Color::VersionControlModified)
    };
    h_flex().flex_none().w(px(14.)).justify_center().child(
        Label::new(letter)
            .size(LabelSize::Small)
            .weight(FontWeight::SEMIBOLD)
            .color(color),
    )
}

/// Branch, remote and tag badges at the end of a history row, like VS Code's
/// graph: the checked-out branch first, a remote that points at the same
/// commit as a local branch collapses into a small cloud icon.
/// Last row of the history while `git log` has more commits than are shown.
fn render_show_more_row(
    git_panel: WeakEntity<GitPanel>,
    shown_limit: usize,
    graph_lanes: usize,
    cx: &mut App,
) -> AnyElement {
    let row = h_flex()
        .id("fork-history-show-more-row")
        .h(HISTORY_ROW_HEIGHT)
        .w_full()
        .child(div().flex_none().w(graph::graph_width(graph_lanes)));
    if ForkGitPanelLayoutSettings::get_global(cx).graph_page_on_scroll {
        // The row is only laid out once it is scrolled into view. Updating the
        // panel has to wait until the list is done rendering.
        cx.defer(move |cx| {
            git_panel
                .update(cx, |panel, cx| {
                    panel.fork_show_more_history(shown_limit, cx)
                })
                .ok();
        });
        return row
            .child(
                Label::new("Loading More…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element();
    }
    row.child(
        Button::new("fork-history-show-more", "Show More")
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .start_icon(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .on_click(move |_, _, cx| {
                git_panel
                    .update(cx, |panel, cx| {
                        panel.fork_show_more_history(shown_limit, cx)
                    })
                    .ok();
            }),
    )
    .into_any_element()
}

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
                    settings
                        .fork
                        .get_or_insert_default()
                        .git_panel
                        .get_or_insert_default()
                        .layout = Some(layout);
                });
            });
        });
        cx.run_until_parked();
    }

    fn set_page_size(page_size: usize, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .fork
                        .get_or_insert_default()
                        .git_panel
                        .get_or_insert_default()
                        .graph_page_size = Some(page_size);
                });
            });
        });
        cx.run_until_parked();
    }

    async fn panel_with_one_commit(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<GitPanel>, VisualTestContext) {
        panel_with_linear_history(1, None, cx).await
    }

    /// A branchless history of `count` commits, each the parent of the one
    /// before it.
    async fn panel_with_linear_history(
        count: u8,
        page_size: Option<usize>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<GitPanel>, VisualTestContext) {
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree("/root", json!({ "project": { ".git": {} } }))
            .await;
        let dot_git = Path::new(path!("/root/project/.git"));
        let sha = |n: u8| -> Oid { format!("{n:040x}").parse().unwrap() };
        fs.with_git_state(dot_git, false, |state| {
            state.current_branch_name = None;
            state.refs.insert("HEAD".into(), sha(count).to_string());
            state.graph_commits = (1..=count)
                .rev()
                .map(|n| {
                    Arc::new(git::repository::InitialGraphCommitData {
                        sha: sha(n),
                        parents: (n > 1).then(|| sha(n - 1)).into_iter().collect(),
                        ref_names: Vec::new(),
                    })
                })
                .collect();
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
        if let Some(page_size) = page_size {
            set_page_size(page_size, &mut cx);
        }
        let panel = workspace.update_in(&mut cx, GitPanel::new);
        cx.run_until_parked();
        (workspace, panel, cx)
    }

    #[gpui::test]
    async fn test_commit_editor_font_size_matches_ui_labels(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, _, mut cx) = panel_with_one_commit(cx).await;
        cx.update(|window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(16.0.into());
                    settings.theme.buffer_font_size = Some(32.0.into());
                    settings.theme.git_commit_buffer_font_size = None;
                });
            });
            theme_settings::setup_ui_font(window, cx);
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(14.));

            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(20.0.into());
                });
            });
            theme_settings::setup_ui_font(window, cx);
            assert_eq!(
                fork_commit_editor_font_size(window.rem_size(), cx),
                TextSize::Default.rems(cx).to_pixels(window.rem_size()),
            );
            assert_eq!(
                fork_commit_editor_font_size(window.rem_size(), cx),
                px(17.5)
            );

            theme_settings::adjust_ui_font_size(cx, |_| px(24.));
            theme_settings::setup_ui_font(window, cx);
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(21.));

            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.git_commit_buffer_font_size = Some(19.0.into());
                });
            });
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(19.));
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.git_commit_buffer_font_size = Some(12.0.into());
                });
            });
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(12.));
        });
    }

    #[gpui::test]
    async fn test_commit_editor_temporary_font_size_actions(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, panel, mut cx) = panel_with_one_commit(cx).await;
        cx.update(|window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(16.0.into());
                    settings.theme.buffer_font_size = Some(32.0.into());
                    settings.theme.git_commit_buffer_font_size = None;
                });
            });
            theme_settings::setup_ui_font(window, cx);
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.increase_font_size(&IncreaseBufferFontSize { persist: false }, window, cx);
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(15.));
            panel.increase_font_size(&IncreaseBufferFontSize { persist: false }, window, cx);
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(16.));
            panel.decrease_font_size(&DecreaseBufferFontSize { persist: false }, window, cx);
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(15.));
            assert_eq!(
                cx.global::<SettingsStore>()
                    .raw_user_settings()
                    .and_then(|settings| settings.content.theme.git_commit_buffer_font_size),
                None,
            );

            panel.handle_font_size_action(false, px(1000.), window, cx);
            assert_eq!(
                fork_commit_editor_font_size(window.rem_size(), cx),
                theme_settings::clamp_font_size(px(1015.)),
            );
            panel.handle_font_size_action(false, px(-1000.), window, cx);
            assert_eq!(
                fork_commit_editor_font_size(window.rem_size(), cx),
                theme_settings::clamp_font_size(px(-1000.)),
            );
            panel.reset_font_size(&ResetBufferFontSize { persist: false }, window, cx);
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(14.));
        });

        set_layout(ForkGitPanelLayout::Tabs, &mut cx);
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.increase_font_size(&IncreaseBufferFontSize { persist: false }, window, cx);
            assert_eq!(
                ThemeSettings::get_global(cx).git_commit_buffer_font_size(cx),
                px(13.)
            );
        });
        set_layout(ForkGitPanelLayout::Vscode, &mut cx);
        cx.update(|window, cx| {
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(13.));
        });
    }

    #[gpui::test]
    async fn test_commit_editor_persisted_font_size_actions(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, panel, mut cx) = panel_with_one_commit(cx).await;
        let fs = panel.read_with(&cx, |panel, _| panel.fs.clone());
        let initialize_settings = cx.update(|_, cx| {
            settings::update_settings_file_with_completion(fs.clone(), cx, |settings, _| {
                settings
                    .fork
                    .get_or_insert_default()
                    .git_panel
                    .get_or_insert_default()
                    .layout = Some(ForkGitPanelLayout::Vscode);
                settings.theme.ui_font_size = Some(16.0.into());
                settings.theme.buffer_font_size = Some(32.0.into());
                settings.theme.git_commit_buffer_font_size = None;
            })
        });
        let initialize_result = initialize_settings.await;
        assert!(
            matches!(initialize_result, Ok(Ok(()))),
            "{initialize_result:?}"
        );
        cx.update(|window, cx| {
            theme_settings::setup_ui_font(window, cx);
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.increase_font_size(&IncreaseBufferFontSize { persist: true }, window, cx);
            panel.increase_font_size(&IncreaseBufferFontSize { persist: true }, window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(16.));
            assert_eq!(
                ThemeSettings::get_global(cx).git_commit_buffer_font_size_settings(),
                Some(px(16.)),
            );
        });
        let saved_settings = SettingsStore::load_settings(&fs)
            .await
            .and_then(|text| settings::parse_json_with_comments::<serde_json::Value>(&text));
        assert!(
            saved_settings.as_ref().is_ok_and(|settings| {
                settings
                    .get("git_commit_buffer_font_size")
                    .and_then(|size| size.as_f64())
                    == Some(16.0)
            }),
            "{saved_settings:?}",
        );

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.decrease_font_size(&DecreaseBufferFontSize { persist: true }, window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(15.));
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.reset_font_size(&ResetBufferFontSize { persist: true }, window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert_eq!(fork_commit_editor_font_size(window.rem_size(), cx), px(14.));
            assert_eq!(
                cx.global::<SettingsStore>()
                    .raw_user_settings()
                    .and_then(|settings| settings.content.theme.git_commit_buffer_font_size),
                None,
            );
        });
    }

    fn history_len(panel: &GitPanel) -> Option<usize> {
        match &panel.commit_history {
            CommitHistory::Loaded(entries) => Some(entries.len()),
            CommitHistory::Loading | CommitHistory::Error(_) => None,
        }
    }

    #[gpui::test]
    async fn test_vscode_layout_groups_changes_by_staging_state(cx: &mut TestAppContext) {
        use git::status::StatusCode;

        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/project"),
            json!({ ".git": {}, "staged": "staged\n", "unstaged": "unstaged\n" }),
        )
        .await;
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("staged", StatusCode::Modified.index()),
                ("unstaged", StatusCode::Modified.worktree()),
            ],
        );
        let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window_handle
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        // The default "status" grouping still splits staged and unstaged
        // files, like VS Code.
        set_layout(ForkGitPanelLayout::Vscode, &mut cx);
        let panel = workspace.update_in(&mut cx, GitPanel::new);
        cx.executor().advance_clock(2 * UPDATE_DEBOUNCE);
        cx.run_until_parked();

        let sections = |panel: &GitPanel| {
            panel
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let path = entry.status_entry()?.repo_path.as_unix_str().to_string();
                    Some((panel.section_for_entry_index(index)?, path))
                })
                .collect::<Vec<_>>()
        };
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                sections(panel),
                vec![
                    (Section::Staged, "staged".to_string()),
                    (Section::Unstaged, "unstaged".to_string()),
                ]
            );
        });

        set_layout(ForkGitPanelLayout::Tabs, &mut cx);
        cx.executor().advance_clock(2 * UPDATE_DEBOUNCE);
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert!(
                sections(panel)
                    .iter()
                    .all(|(section, _)| *section == Section::Tracked),
                "{:?}",
                sections(panel)
            );
        });
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

    #[gpui::test]
    async fn test_history_is_shown_a_page_at_a_time(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, panel, mut cx) = panel_with_linear_history(5, Some(2), cx).await;
        panel.read_with(&cx, |panel, _| {
            assert_eq!(history_len(panel), Some(2));
            assert!(panel.fork_layout.history_has_more);
        });

        panel.update(&mut cx, |panel, cx| {
            panel.fork_show_more_history(2, cx);
            // A second request for the same page is ignored.
            panel.fork_show_more_history(2, cx);
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(history_len(panel), Some(4));
            assert!(panel.fork_layout.history_has_more);
        });

        panel.update(&mut cx, |panel, cx| panel.fork_show_more_history(4, cx));
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(history_len(panel), Some(5));
            assert!(!panel.fork_layout.history_has_more);
        });

        // Collapsing the section starts again from the first page.
        panel.update(&mut cx, |panel, cx| {
            panel.fork_set_history_collapsed(true, cx);
            panel.fork_set_history_collapsed(false, cx);
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| assert_eq!(history_len(panel), Some(2)));

        // So does changing the page size.
        panel.update(&mut cx, |panel, cx| panel.fork_show_more_history(2, cx));
        cx.run_until_parked();
        set_page_size(3, &mut cx);
        panel.read_with(&cx, |panel, _| {
            assert_eq!(history_len(panel), Some(3));
            assert!(panel.fork_layout.history_has_more);
        });

        // The tabs layout keeps upstream's full history.
        set_layout(ForkGitPanelLayout::Tabs, &mut cx);
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.set_active_tab(GitPanelTab::History, window, cx);
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| assert_eq!(history_len(panel), Some(5)));
    }

    #[gpui::test]
    async fn test_history_without_more_commits_has_no_show_more(cx: &mut TestAppContext) {
        init_test(cx);
        let (_, panel, cx) = panel_with_linear_history(2, Some(2), cx).await;
        panel.read_with(&cx, |panel, _| {
            assert_eq!(history_len(panel), Some(2));
            assert!(!panel.fork_layout.history_has_more);
        });
    }

    #[gpui::test]
    async fn test_graph_covers_only_the_shown_commits(cx: &mut TestAppContext) {
        init_test(cx);
        let (workspace, panel, mut cx) = panel_with_linear_history(5, Some(2), cx).await;
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
            workspace.open_panel::<GitPanel>(window, cx);
        });
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                panel
                    .fork_layout
                    .graph
                    .as_ref()
                    .map(|graph| graph.rows.len()),
                Some(2)
            );
        });
    }
}
