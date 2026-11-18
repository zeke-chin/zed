use collections::HashMap;
use editor::{Editor, EditorElement, EditorStyle, actions::SelectAll};
use file_icons::FileIcons;
use futures::{StreamExt, pin_mut};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle,
    IntoElement, ScrollStrategy, StyledText, Subscription, Task, TextStyle,
    UniformListScrollHandle, WeakEntity, Window, actions, div, relative, rems, uniform_list,
};
use language::{Anchor, Buffer, BufferSnapshot, Point};
use menu::{Cancel, Confirm, SelectNext, SelectPrevious};
use project::{
    Project,
    search::{SearchQuery, SearchResult},
};
use search::{SearchOption, SearchOptions, ToggleCaseSensitive, ToggleRegex, ToggleWholeWord};
use settings::{ForkSearchPanelDock, RegisterSetting, Settings, SettingsContent, SettingsStore};
use std::{ops::Range, path::Path, time::Duration};
use theme_settings::ThemeSettings;
use ui::{CommonAnimationExt, IconButtonShape, LabelLike, Tooltip, prelude::*};
use util::{ResultExt, paths::PathMatcher};
use workspace::{
    DeploySearch, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    fork_search_panel,
    [
        /// Searches the project in the search sidebar.
        Deploy,
        /// Toggles focus on the search sidebar.
        ToggleFocus,
        /// Opens the current search in an editable tab.
        OpenInEditor,
        /// Shows or hides the file filters.
        ToggleFilters,
        /// Shows or hides the replacement input.
        ToggleReplace,
        /// Toggles preserving the case of replaced text.
        TogglePreserveCase,
        /// Replaces the selected search match.
        ReplaceSelected,
        /// Replaces all search matches after confirmation.
        ReplaceAll,
        /// Runs the current search immediately.
        Search,
        /// Clears the current search and its results.
        Clear,
        /// Collapses or expands all result files.
        ToggleCollapseAll,
        /// Moves focus from the search input to the results.
        FocusResults,
        /// Opens the selected match and focuses the editor.
        OpenSelected,
        /// Collapses the selected file.
        Collapse,
        /// Expands the selected file.
        Expand,
    ]
);

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct SearchPanelSettings {
    enabled: bool,
    dock: ForkSearchPanelDock,
    default_width: f32,
    search_on_type: bool,
}

impl Settings for SearchPanelSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content
            .fork
            .as_ref()
            .and_then(|fork| fork.search_panel.as_ref());
        Self {
            enabled: content.and_then(|content| content.enabled).unwrap_or(true),
            dock: content.and_then(|content| content.dock).unwrap_or_default(),
            default_width: content
                .and_then(|content| content.default_width)
                .filter(|width| width.is_finite() && *width > 0.)
                .unwrap_or(300.),
            search_on_type: content
                .and_then(|content| content.search_on_type)
                .unwrap_or(true),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchState {
    Idle,
    Searching,
    NoResults,
    Results,
    LimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InputPanel {
    Query,
    Replacement,
    Include,
    Exclude,
}

struct FileResults {
    path_label: SharedString,
    file_name: SharedString,
    directory: SharedString,
    icon: Option<SharedString>,
    matches: Vec<PanelMatch>,
    collapsed: bool,
}

#[derive(Clone)]
struct PanelMatch {
    buffer: Entity<Buffer>,
    range: Range<Anchor>,
    line_number: u32,
    preview: SharedString,
    highlight_ranges: Vec<Range<usize>>,
    matched_text: SharedString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultRow {
    File(usize),
    Match {
        file_index: usize,
        match_index: usize,
    },
}

impl ResultRow {
    fn file_index(self) -> usize {
        match self {
            Self::File(index) => index,
            Self::Match { file_index, .. } => file_index,
        }
    }
}

#[derive(Clone, Copy)]
enum ReplacementScope {
    Match {
        file_index: usize,
        match_index: usize,
    },
    File(usize),
    All,
}

struct ReplacementTarget {
    buffer: Entity<Buffer>,
    snapshot: BufferSnapshot,
    path: SharedString,
    matches: Vec<PanelMatch>,
}

struct ReplacementPlan {
    buffer: Entity<Buffer>,
    snapshot: BufferSnapshot,
    path: SharedString,
    edits: Vec<(Range<usize>, String)>,
}

pub struct SearchPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    results_focus_handle: FocusHandle,
    query_editor: Entity<Editor>,
    replacement_editor: Entity<Editor>,
    replace_visible: bool,
    preserve_case: bool,
    active_query: Option<SearchQuery>,
    replacement_task: Option<Task<()>>,
    included_files_editor: Entity<Editor>,
    excluded_files_editor: Entity<Editor>,
    search_options: SearchOptions,
    filters_visible: bool,
    results: Vec<FileResults>,
    rows: Vec<ResultRow>,
    selected_row: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    pending_search: Option<Task<()>>,
    search_state: SearchState,
    panels_with_errors: HashMap<InputPanel, String>,
    active: bool,
    search_id: usize,
    input_texts: [String; 3],
    _subscriptions: Vec<Subscription>,
}

pub fn init(cx: &mut App) {
    SearchPanelSettings::register(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Deploy, window, cx| {
            if !SearchPanelSettings::get_global(cx).enabled {
                search::ProjectSearchView::deploy_search(
                    workspace,
                    &DeploySearch::default(),
                    window,
                    cx,
                );
                return;
            }
            let selected_text = workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
                .and_then(|editor| selection_query(editor.read(cx), cx));
            let panel = ensure_panel(workspace, window, cx);
            workspace.focus_panel::<SearchPanel>(window, cx);
            panel.update(cx, |panel, cx| {
                panel.deploy(selected_text.as_deref(), window, cx)
            });
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            if SearchPanelSettings::get_global(cx).enabled {
                ensure_panel(workspace, window, cx);
                workspace.toggle_panel_focus::<SearchPanel>(window, cx);
            }
        });
    })
    .detach();
}

pub fn attach_workspace(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Capture the forwarding action so upstream registration order cannot open a search tab first.
    workspace.register_action_renderer(|div, _, _, cx| {
        div.capture_action(cx.listener(
            |workspace, action: &zed_actions::search::NewSearchInDirectory, window, cx| {
                if !SearchPanelSettings::get_global(cx).enabled {
                    return;
                }
                cx.stop_propagation();
                let selected_text = workspace
                    .active_item(cx)
                    .and_then(|item| item.act_as::<Editor>(cx))
                    .and_then(|editor| selection_query(editor.read(cx), cx));
                let panel = ensure_panel(workspace, window, cx);
                workspace.focus_panel::<SearchPanel>(window, cx);
                panel.update(cx, |panel, cx| {
                    panel.filters_visible = true;
                    panel.included_files_editor.update(cx, |editor, cx| {
                        editor.set_text(action.directory.clone(), window, cx);
                    });
                    panel.excluded_files_editor.update(cx, |editor, cx| {
                        editor.set_text("", window, cx);
                    });
                    panel.deploy(selected_text.as_deref(), window, cx);
                });
            },
        ))
    });
    if SearchPanelSettings::get_global(cx).enabled {
        cx.spawn_in(window, async move |workspace, cx| {
            let panel = SearchPanel::load(workspace.clone(), cx.clone()).await?;
            workspace.update_in(cx, |workspace, window, cx| {
                if SearchPanelSettings::get_global(cx).enabled
                    && workspace.panel::<SearchPanel>(cx).is_none()
                {
                    workspace.add_panel(panel, window, cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
    cx.observe_global_in::<SettingsStore>(window, |workspace, window, cx| {
        if SearchPanelSettings::get_global(cx).enabled {
            ensure_panel(workspace, window, cx);
        } else if let Some(panel) = workspace.panel::<SearchPanel>(cx) {
            workspace.remove_panel(&panel, window, cx);
        }
    })
    .detach();
}

fn ensure_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<SearchPanel> {
    workspace.panel::<SearchPanel>(cx).unwrap_or_else(|| {
        let project = workspace.project().clone();
        let workspace_handle = workspace.weak_handle();
        let panel = cx.new(|cx| SearchPanel::new(workspace_handle, project, window, cx));
        workspace.add_panel(panel.clone(), window, cx);
        panel
    })
}

fn selection_query(editor: &Editor, cx: &App) -> Option<String> {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let selection = editor.selections.newest_anchor();
    let text = snapshot
        .text_for_range(selection.range())
        .collect::<String>();
    (!text.is_empty() && !text.contains(['\n', '\r'])).then_some(text)
}

impl SearchPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let project = workspace.read_with(&cx, |workspace, _| workspace.project().clone())?;
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace.weak_handle(), project, window, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query_editor = new_input("Search", window, cx);
        let replacement_editor = new_input("Replace", window, cx);
        let included_files_editor = new_input("e.g. src/**/*.rs", window, cx);
        let excluded_files_editor = new_input("e.g. **/vendor/**", window, cx);
        let focus_handle = cx.focus_handle();
        let mut subscriptions = Vec::new();
        for editor in [
            &query_editor,
            &included_files_editor,
            &excluded_files_editor,
        ] {
            subscriptions.push(
                cx.subscribe(editor, |this, _, event: &editor::EditorEvent, cx| {
                    if matches!(event, editor::EditorEvent::Edited { .. }) {
                        this.input_changed(cx);
                    }
                }),
            );
        }
        subscriptions.push(cx.subscribe(
            &replacement_editor,
            |this, _, event: &editor::EditorEvent, cx| {
                if matches!(event, editor::EditorEvent::Edited { .. }) {
                    this.panels_with_errors.remove(&InputPanel::Replacement);
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.on_focus(&focus_handle, window, |this, window, cx| {
            this.query_editor.focus_handle(cx).focus(window, cx);
        }));
        subscriptions.push(cx.observe_global::<SettingsStore>(|_, cx| cx.notify()));
        Self {
            workspace,
            project,
            focus_handle,
            results_focus_handle: cx.focus_handle(),
            query_editor,
            replacement_editor,
            replace_visible: false,
            preserve_case: false,
            active_query: None,
            replacement_task: None,
            included_files_editor,
            excluded_files_editor,
            search_options: SearchOptions::from_settings(
                &editor::EditorSettings::get_global(cx).search,
            ),
            filters_visible: false,
            results: Vec::new(),
            rows: Vec::new(),
            selected_row: None,
            scroll_handle: UniformListScrollHandle::new(),
            pending_search: None,
            search_state: SearchState::Idle,
            panels_with_errors: HashMap::default(),
            active: false,
            search_id: 0,
            input_texts: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    fn deploy(&mut self, query: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(query) = query {
            self.query_editor
                .update(cx, |editor, cx| editor.set_text(query, window, cx));
        }
        self.query_editor
            .update(cx, |editor, cx| editor.select_all(&SelectAll, window, cx));
        self.query_editor.focus_handle(cx).focus(window, cx);
        self.search(cx);
    }

    fn reset_results(&mut self) {
        self.active_query = None;
        self.results.clear();
        self.rows.clear();
        self.selected_row = None;
        self.scroll_handle = UniformListScrollHandle::new();
    }

    fn input_texts(&self, cx: &App) -> [String; 3] {
        [
            self.query_text(cx),
            self.included_files_editor.read(cx).text(cx),
            self.excluded_files_editor.read(cx).text(cx),
        ]
    }

    fn input_changed(&mut self, cx: &mut Context<Self>) {
        let input_texts = self.input_texts(cx);
        if input_texts == self.input_texts {
            return;
        }
        self.input_texts = input_texts;
        self.search_id += 1;
        self.pending_search = None;
        self.reset_results();
        self.search_state = SearchState::Idle;
        if SearchPanelSettings::get_global(cx).search_on_type {
            let timer = cx.background_executor().timer(Duration::from_millis(300));
            let search_id = self.search_id;
            self.pending_search = Some(cx.spawn(async move |this, cx| {
                timer.await;
                this.update(cx, |this, cx| {
                    if this.search_id == search_id {
                        this.search(cx);
                    }
                })
                .log_err();
            }));
        } else {
            self.build_search_query(cx);
        }
        cx.notify();
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        self.input_texts = self.input_texts(cx);
        self.search_id += 1;
        self.pending_search = None;
        self.reset_results();
        let Some(query) = self.build_search_query(cx) else {
            self.search_state = SearchState::Idle;
            cx.notify();
            return;
        };
        self.active_query = Some(query.clone());
        let search = self
            .project
            .update(cx, |project, cx| project.search(query, cx));
        let search_id = self.search_id;
        self.search_state = SearchState::Searching;
        self.pending_search = Some(cx.spawn(async move |this, cx| {
            let project::SearchResults { rx, task_handle } = search;
            let _task_handle = task_handle;
            let chunks = rx.ready_chunks(256);
            pin_mut!(chunks);
            let mut limit_reached = false;
            while let Some(results) = chunks.next().await {
                let mut files = Vec::new();
                for result in results {
                    match result {
                        SearchResult::Buffer { buffer, ranges } => {
                            if let Some(file) =
                                cx.update(|cx| build_file_results(buffer, ranges, cx))
                            {
                                files.push(file);
                            }
                        }
                        SearchResult::LimitReached => limit_reached = true,
                        SearchResult::WaitingForScan | SearchResult::Searching => {}
                    }
                }
                if !files.is_empty() {
                    if this
                        .update(cx, |this, cx| {
                            if this.search_id != search_id {
                                return;
                            }
                            this.results.extend(files);
                            this.rebuild_rows();
                            cx.notify();
                        })
                        .log_err()
                        .is_none()
                    {
                        return;
                    }
                }
            }
            this.update(cx, |this, cx| {
                if this.search_id != search_id {
                    return;
                }
                this.pending_search = None;
                this.search_state = if limit_reached {
                    SearchState::LimitReached
                } else if this.results.is_empty() {
                    SearchState::NoResults
                } else {
                    SearchState::Results
                };
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn build_search_query(&mut self, cx: &App) -> Option<SearchQuery> {
        self.panels_with_errors.clear();
        let text = self.query_text(cx);
        if text.is_empty() {
            return None;
        }
        let included_files = self.parse_filter(InputPanel::Include, cx);
        let excluded_files = self.parse_filter(InputPanel::Exclude, cx);
        let match_full_paths = self.project.read(cx).visible_worktrees(cx).count() > 1;
        let query = if self.search_options.contains(SearchOptions::REGEX) {
            SearchQuery::regex(
                text,
                self.search_options.contains(SearchOptions::WHOLE_WORD),
                self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                self.search_options.contains(SearchOptions::INCLUDE_IGNORED),
                false,
                included_files,
                excluded_files,
                match_full_paths,
                None,
            )
        } else {
            SearchQuery::text(
                text,
                self.search_options.contains(SearchOptions::WHOLE_WORD),
                self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                self.search_options.contains(SearchOptions::INCLUDE_IGNORED),
                included_files,
                excluded_files,
                match_full_paths,
                None,
            )
        };
        match query {
            Ok(query) => self.panels_with_errors.is_empty().then_some(query),
            Err(error) => {
                self.panels_with_errors
                    .insert(InputPanel::Query, error.to_string());
                None
            }
        }
    }

    fn parse_filter(&mut self, input: InputPanel, cx: &App) -> PathMatcher {
        let editor = match input {
            InputPanel::Include => &self.included_files_editor,
            InputPanel::Exclude => &self.excluded_files_editor,
            InputPanel::Query => &self.query_editor,
            InputPanel::Replacement => &self.replacement_editor,
        };
        let text = editor.read(cx).text(cx);
        let patterns = split_glob_patterns(&text)
            .into_iter()
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        match PathMatcher::new(&patterns, self.project.read(cx).path_style(cx)) {
            Ok(matcher) => matcher,
            Err(error) => {
                self.panels_with_errors.insert(input, error.to_string());
                PathMatcher::default()
            }
        }
    }

    fn query_text(&self, cx: &App) -> String {
        self.query_editor.read(cx).text(cx)
    }

    fn match_count(&self) -> usize {
        self.results.iter().map(|file| file.matches.len()).sum()
    }

    fn toggle_option(&mut self, option: SearchOptions, cx: &mut Context<Self>) {
        self.search_options.toggle(option);
        self.search(cx);
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for editor in [
            &self.query_editor,
            &self.included_files_editor,
            &self.excluded_files_editor,
        ] {
            editor.update(cx, |editor, cx| editor.set_text("", window, cx));
        }
        self.search(cx);
        self.query_editor.focus_handle(cx).focus(window, cx);
    }

    fn rebuild_rows(&mut self) {
        let selection = self
            .selected_row
            .and_then(|index| self.rows.get(index))
            .copied();
        self.rows.clear();
        for (file_index, file) in self.results.iter().enumerate() {
            self.rows.push(ResultRow::File(file_index));
            if !file.collapsed {
                self.rows
                    .extend((0..file.matches.len()).map(|match_index| ResultRow::Match {
                        file_index,
                        match_index,
                    }));
            }
        }
        self.selected_row = selection
            .and_then(|selected| {
                self.rows
                    .iter()
                    .position(|row| *row == selected)
                    .or_else(|| {
                        self.rows
                            .iter()
                            .position(|row| *row == ResultRow::File(selected.file_index()))
                    })
            })
            .or_else(|| (!self.rows.is_empty()).then_some(0));
    }

    fn select_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.rows.get(index).is_some() {
            self.selected_row = Some(index);
            self.scroll_handle
                .scroll_to_item(index, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn select_next(&mut self, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let index = self
            .selected_row
            .map_or(0, |index| (index + 1) % self.rows.len());
        self.select_row(index, cx);
    }

    fn select_previous(&mut self, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let index = self
            .selected_row
            .filter(|index| *index > 0)
            .map_or(self.rows.len() - 1, |index| index - 1);
        self.select_row(index, cx);
    }

    fn toggle_file(&mut self, file_index: usize, cx: &mut Context<Self>) {
        if let Some(file) = self.results.get_mut(file_index) {
            file.collapsed = !file.collapsed;
            self.rebuild_rows();
            cx.notify();
        }
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let collapsed = self.results.iter().any(|file| !file.collapsed);
        for file in &mut self.results {
            file.collapsed = collapsed;
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn collapse_selected(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self
            .selected_row
            .and_then(|index| self.rows.get(index))
            .copied()
        else {
            return;
        };
        let file_index = row.file_index();
        match row {
            ResultRow::File(_) => {
                if let Some(file) = self.results.get_mut(file_index) {
                    file.collapsed = true;
                }
                self.rebuild_rows();
            }
            ResultRow::Match { .. } => {
                if let Some(index) = self
                    .rows
                    .iter()
                    .position(|row| *row == ResultRow::File(file_index))
                {
                    self.select_row(index, cx);
                }
            }
        }
        cx.notify();
    }

    fn expand_selected(&mut self, cx: &mut Context<Self>) {
        let Some(ResultRow::File(file_index)) = self
            .selected_row
            .and_then(|index| self.rows.get(index))
            .copied()
        else {
            return;
        };
        if let Some(file) = self.results.get_mut(file_index) {
            if file.collapsed {
                file.collapsed = false;
                self.rebuild_rows();
            } else {
                self.select_next(cx);
            }
        }
        cx.notify();
    }

    fn focus_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.results_focus_handle.focus(window, cx);
            if let Some(index) = self.selected_row {
                self.select_row(index, cx);
            }
        }
    }

    fn open_selected(&mut self, focus_editor: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self
            .selected_row
            .and_then(|index| self.rows.get(index))
            .copied()
        else {
            return;
        };
        match row {
            ResultRow::File(index) => self.toggle_file(index, cx),
            ResultRow::Match {
                file_index,
                match_index,
            } => self.open_match(file_index, match_index, focus_editor, window, cx),
        }
    }

    fn open_match(
        &mut self,
        file_index: usize,
        match_index: usize,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search_match) = self
            .results
            .get(file_index)
            .and_then(|file| file.matches.get(match_index))
            .cloned()
        else {
            return;
        };
        let PanelMatch { buffer, range, .. } = search_match;
        let result = self.workspace.update(cx, |workspace, cx| {
            let pane = workspace.active_pane().clone();
            let editor = workspace.open_project_item::<Editor>(
                Some(pane),
                buffer.clone(),
                true,
                focus_editor,
                false,
                true,
                window,
                cx,
            );
            editor.update(cx, |editor, cx| {
                let snapshot = buffer.read(cx).snapshot();
                let start = snapshot.summary_for_anchor::<Point>(&range.start);
                let end = snapshot.summary_for_anchor::<Point>(&range.end);
                editor.go_to_singleton_buffer_range(start..end, window, cx);
            });
        });
        if let Err(error) = result {
            self.panels_with_errors
                .insert(InputPanel::Query, format!("Could not open result: {error}"));
        } else if !focus_editor {
            self.results_focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn editor_search_action(&self, cx: &App) -> DeploySearch {
        DeploySearch {
            query: Some(self.query_text(cx)),
            regex: Some(self.search_options.contains(SearchOptions::REGEX)),
            case_sensitive: Some(self.search_options.contains(SearchOptions::CASE_SENSITIVE)),
            whole_word: Some(self.search_options.contains(SearchOptions::WHOLE_WORD)),
            include_ignored: Some(self.search_options.contains(SearchOptions::INCLUDE_IGNORED)),
            included_files: Some(self.included_files_editor.read(cx).text(cx)),
            excluded_files: Some(self.excluded_files_editor.read(cx).text(cx)),
            replace_enabled: false,
        }
    }

    fn open_in_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.build_search_query(cx).is_none() {
            cx.notify();
            return;
        }
        let action = self.editor_search_action(cx);
        if let Err(error) = self.workspace.update(cx, |workspace, cx| {
            search::ProjectSearchView::deploy_search(workspace, &action, window, cx);
        }) {
            self.panels_with_errors.insert(
                InputPanel::Query,
                format!("Could not open search in editor: {error}"),
            );
        }
        cx.notify();
    }

    fn toggle_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_visible = !self.replace_visible;
        if self.replace_visible {
            self.replacement_editor.focus_handle(cx).focus(window, cx);
        } else {
            self.query_editor.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn can_replace(&self) -> bool {
        self.search_state == SearchState::Results && self.replacement_task.is_none()
    }

    fn replace_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self
            .selected_row
            .and_then(|index| self.rows.get(index))
            .copied();
        let scope = match row {
            Some(ResultRow::Match {
                file_index,
                match_index,
            }) => ReplacementScope::Match {
                file_index,
                match_index,
            },
            Some(ResultRow::File(file_index)) => ReplacementScope::Match {
                file_index,
                match_index: 0,
            },
            None => return,
        };
        self.replace(scope, window, cx);
    }

    fn replace(&mut self, scope: ReplacementScope, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_replace() {
            return;
        }
        let Some(query) = self.active_query.clone() else {
            return;
        };
        let replacement = self.replacement_editor.read(cx).text(cx);
        let query = query.with_replacement(replacement.clone());
        let preserve_case = self.preserve_case;
        let search_id = self.search_id;
        let mut targets = Vec::new();
        for (file_index, file) in self.results.iter().enumerate() {
            let matches = match scope {
                ReplacementScope::All => file.matches.clone(),
                ReplacementScope::File(index) if index == file_index => file.matches.clone(),
                ReplacementScope::Match {
                    file_index: index,
                    match_index,
                } if index == file_index => {
                    file.matches.get(match_index).cloned().into_iter().collect()
                }
                _ => continue,
            };
            if let Some(first) = matches.first() {
                targets.push(ReplacementTarget {
                    buffer: first.buffer.clone(),
                    snapshot: first.buffer.read(cx).snapshot(),
                    path: file.path_label.clone(),
                    matches,
                });
            }
        }
        let count = targets
            .iter()
            .map(|target| target.matches.len())
            .sum::<usize>();
        if count == 0 {
            return;
        }
        let confirmation = if matches!(scope, ReplacementScope::Match { .. }) {
            None
        } else {
            Some(window.prompt(
                gpui::PromptLevel::Warning,
                &format!("Replace {count} matches in {} files?", targets.len()),
                Some("Changed files open in editor tabs and can be undone per file. This operation does not save them; existing auto-save settings still apply."),
                &["Replace", "Cancel"],
                cx,
            ))
        };
        self.panels_with_errors.remove(&InputPanel::Replacement);
        self.replacement_task = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome: anyhow::Result<()> = async {
                if let Some(confirmation) = confirmation {
                    if confirmation.await? != 0 {
                        return Ok(());
                    }
                }
                let plans = cx.background_spawn(prepare_replacements(query, targets, preserve_case)).await?;
                this.update_in(cx, |this, window, cx| {
                    anyhow::ensure!(
                        this.search_id == search_id
                            && this.input_texts(cx) == this.input_texts
                            && this.replacement_editor.read(cx).text(cx) == replacement
                            && this.preserve_case == preserve_case,
                        "Search or replacement changed. Run the replacement again."
                    );
                    for plan in &plans {
                        let buffer = plan.buffer.read(cx);
                        anyhow::ensure!(buffer.capability().editable(), "{} is read-only", plan.path);
                        anyhow::ensure!(
                            &buffer.version() == plan.snapshot.version(),
                            "{} changed while preparing replacement. Search again before replacing.", plan.path
                        );
                    }
                    // The project buffer store holds weak references, and a new search clears these results.
                    // Persistent tabs keep unsaved replacements alive after the next search.
                    this.workspace.update(cx, |workspace, cx| {
                        for plan in &plans {
                            workspace.open_project_item::<Editor>(
                                None, plan.buffer.clone(), false, false, true, false, window, cx,
                            );
                        }
                    })?;
                    for plan in plans {
                        plan.buffer.update(cx, |buffer, cx| {
                            buffer.finalize_last_transaction();
                            buffer.edit(plan.edits, None, cx);
                            buffer.finalize_last_transaction();
                        });
                    }
                    this.search(cx);
                    anyhow::Ok(())
                })??;
                Ok(())
            }.await;
            this.update(cx, |this, cx| {
                this.replacement_task = None;
                if let Err(error) = outcome {
                    this.panels_with_errors.insert(InputPanel::Replacement, error.to_string());
                }
                cx.notify();
            }).log_err();
        }));
        cx.notify();
    }

    fn render_input(&self, input: InputPanel, editor: &Entity<Editor>, cx: &App) -> gpui::Div {
        let theme = cx.theme();
        let border = if self.panels_with_errors.contains_key(&input) {
            theme.status().error
        } else {
            theme.colors().border
        };
        let settings = ThemeSettings::get_global(cx);
        let style = EditorStyle {
            background: theme.colors().editor_background,
            local_player: theme.players().local(),
            text: TextStyle {
                color: theme.colors().text,
                font_family: settings.buffer_font.family.clone(),
                font_features: settings.buffer_font.features.clone(),
                font_fallbacks: settings.buffer_font.fallbacks.clone(),
                font_weight: settings.buffer_font.weight,
                font_size: rems(0.875).into(),
                line_height: relative(1.3),
                ..TextStyle::default()
            },
            ..EditorStyle::default()
        };
        h_flex()
            .key_context(if input == InputPanel::Replacement {
                "fork_replace_input"
            } else {
                "fork_search_input"
            })
            .w_full()
            .min_w_0()
            .min_h_8()
            .px_2()
            .py_1()
            .gap_1()
            .border_1()
            .border_color(border)
            .rounded_sm()
            .bg(theme.colors().editor_background)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(EditorElement::new(editor, style)),
            )
    }

    fn render_option(&self, option: SearchOption, cx: &mut Context<Self>) -> impl IntoElement {
        IconButton::new(("search-option", option as usize), option.icon())
            .shape(IconButtonShape::Square)
            .toggle_state(self.search_options.contains(option.as_options()))
            .tooltip(Tooltip::text(option.label()))
            .on_click(
                cx.listener(move |this, _, _, cx| this.toggle_option(option.as_options(), cx)),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .justify_between()
            .px_3()
            .py_2()
            .child(
                Label::new("SEARCH")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("refresh", IconName::ArrowCircle)
                            .shape(IconButtonShape::Square)
                            .tooltip(Tooltip::text("Refresh Search"))
                            .on_click(cx.listener(|this, _, _, cx| this.search(cx))),
                    )
                    .child(
                        IconButton::new("clear", IconName::Trash)
                            .shape(IconButtonShape::Square)
                            .tooltip(Tooltip::text("Clear Search Results"))
                            .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
                    )
                    .child(
                        IconButton::new("collapse-all", IconName::ChevronUp)
                            .shape(IconButtonShape::Square)
                            .tooltip(Tooltip::text("Collapse / Expand All"))
                            .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
                    )
                    .child(
                        IconButton::new("open-editor", IconName::FileTextOutlined)
                            .shape(IconButtonShape::Square)
                            .tooltip(Tooltip::text("Open in Editor"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_in_editor(window, cx)),
                            ),
                    ),
            )
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(row) = self.rows.get(index).copied() else {
            return div().h_6().into_any_element();
        };
        let Some(file) = self.results.get(row.file_index()) else {
            return div().h_6().into_any_element();
        };
        let selected = self.selected_row == Some(index);
        let colors = cx.theme().colors();
        let base = h_flex()
            .id(("search-row", index))
            .h_6()
            .w_full()
            .min_w_0()
            .gap_1()
            .px_2()
            .cursor_pointer()
            .when(selected, |row| row.bg(colors.element_selected))
            .hover(|row| row.bg(colors.element_hover));
        match row {
            ResultRow::File(file_index) => base
                .child(
                    Icon::new(if file.collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(IconSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    file.icon
                        .clone()
                        .map(|path| Icon::from_path(path).size(IconSize::Small))
                        .unwrap_or_else(|| Icon::new(IconName::File).size(IconSize::Small)),
                )
                .child(
                    Label::new(file.file_name.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(file.directory.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
                .child(
                    Label::new(file.matches.len().to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .when(self.replace_visible, |row| {
                    row.child(
                        IconButton::new(("replace-file", file_index), IconName::ReplaceAll)
                            .shape(IconButtonShape::Square)
                            .disabled(!self.can_replace())
                            .tooltip(Tooltip::text("Replace All in File"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.replace(ReplacementScope::File(file_index), window, cx);
                            })),
                    )
                })
                .tooltip(Tooltip::text(file.path_label.clone()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.selected_row = Some(index);
                    this.toggle_file(file_index, cx);
                    this.results_focus_handle.focus(window, cx);
                }))
                .into_any_element(),
            ResultRow::Match {
                file_index,
                match_index,
            } => {
                let Some(search_match) = file.matches.get(match_index) else {
                    return base.into_any_element();
                };
                base.pl_8()
                    .child(
                        div().flex_none().child(
                            Label::new(search_match.line_number.to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(div().flex_1().min_w_0().child(
                        LabelLike::new().size(LabelSize::Small).truncate().child(
                            StyledText::new(search_match.preview.clone()).with_highlights(
                                search_match_highlights(&search_match.highlight_ranges, cx),
                            ),
                        ),
                    ))
                    .when(self.replace_visible, |row| {
                        row.child(
                            IconButton::new(("replace-match", index), IconName::ReplaceNext)
                                .shape(IconButtonShape::Square)
                                .disabled(!self.can_replace())
                                .tooltip(Tooltip::text("Replace Match"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.replace(
                                        ReplacementScope::Match {
                                            file_index,
                                            match_index,
                                        },
                                        window,
                                        cx,
                                    );
                                })),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_row(index, cx);
                        this.open_match(file_index, match_index, false, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }
}

fn search_match_highlights(
    ranges: &[Range<usize>],
    cx: &App,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let highlight = HighlightStyle {
        background_color: Some(cx.theme().colors().search_match_background),
        ..Default::default()
    };
    ranges
        .iter()
        .cloned()
        .map(|range| (range, highlight))
        .collect()
}

fn new_input(placeholder: &'static str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_use_autoclose(false);
        editor.set_use_selection_highlight(false);
        editor
    })
}

fn split_glob_patterns(text: &str) -> Vec<&str> {
    let mut patterns = Vec::new();
    let mut pattern_start = 0;
    let mut brace_depth: usize = 0;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ',' if brace_depth == 0 => {
                patterns.push(&text[pattern_start..index]);
                pattern_start = index + 1;
            }
            _ => {}
        }
    }
    patterns.push(&text[pattern_start..]);
    patterns
}

fn match_preview(line: &str, range: Range<usize>) -> (SharedString, Vec<Range<usize>>) {
    let boundaries = line
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(line.len()))
        .collect::<Vec<_>>();
    let start_character = boundaries.partition_point(|index| *index < range.start.min(line.len()));
    let end_character = boundaries.partition_point(|index| *index < range.end.min(line.len()));
    let whitespace = line
        .chars()
        .take_while(|character| character.is_whitespace())
        .count();
    let mut first_character = whitespace.min(start_character);
    if start_character.saturating_sub(first_character) > 20 {
        first_character = start_character.saturating_sub(12);
    }
    let first_byte = boundaries
        .get(first_character)
        .copied()
        .unwrap_or(line.len());
    let start_byte = boundaries
        .get(start_character)
        .copied()
        .unwrap_or(line.len());
    let end_byte = boundaries.get(end_character).copied().unwrap_or(line.len());
    let prefix = if first_character > whitespace {
        "…"
    } else {
        ""
    };
    let preview = format!("{prefix}{}", line.get(first_byte..).unwrap_or_default());
    let highlight = (prefix.len() + start_byte.saturating_sub(first_byte))
        ..(prefix.len() + end_byte.saturating_sub(first_byte));
    let highlights = if highlight.is_empty() {
        Vec::new()
    } else {
        vec![highlight]
    };
    (preview.into(), highlights)
}

async fn prepare_replacements(
    query: SearchQuery,
    targets: Vec<ReplacementTarget>,
    preserve_case: bool,
) -> anyhow::Result<Vec<ReplacementPlan>> {
    let mut plans = Vec::new();
    for target in targets {
        let ReplacementTarget {
            buffer,
            snapshot,
            path,
            matches,
        } = target;
        anyhow::ensure!(snapshot.capability.editable(), "{path} is read-only");
        let current_matches = query.search(&snapshot, None).await;
        let text = snapshot.text();
        let mut edits = Vec::new();
        for search_match in matches {
            let range = snapshot.summary_for_anchor::<usize>(&search_match.range.start)
                ..snapshot.summary_for_anchor::<usize>(&search_match.range.end);
            anyhow::ensure!(
                text.get(range.clone()) == Some(search_match.matched_text.as_ref())
                    && current_matches
                        .binary_search_by_key(&(range.start, range.end), |hit| (hit.start, hit.end))
                        .is_ok(),
                "Search results in {path} are out of date. Search again before replacing."
            );
            let replacement = query
                .replacement_for(&text, range.clone())
                .ok_or_else(|| anyhow::anyhow!("Could not expand replacement in {path}"))?;
            let replacement = if preserve_case {
                preserve_replacement_case(search_match.matched_text.as_ref(), &replacement)
            } else {
                replacement.into_owned()
            };
            edits.push((range, replacement));
        }
        edits.sort_by_key(|(range, _)| (range.start, range.end));
        plans.push(ReplacementPlan {
            buffer,
            snapshot,
            path,
            edits,
        });
    }
    Ok(plans)
}

fn preserve_replacement_case(matched: &str, replacement: &str) -> String {
    let mut letters = matched
        .chars()
        .filter(|character| character.is_uppercase() || character.is_lowercase());
    let Some(first) = letters.next() else {
        return replacement.to_owned();
    };
    let remaining = letters.collect::<Vec<_>>();
    if first.is_uppercase() && remaining.iter().all(|character| character.is_uppercase()) {
        replacement.to_uppercase()
    } else if first.is_lowercase() && remaining.iter().all(|character| character.is_lowercase()) {
        replacement.to_lowercase()
    } else if first.is_uppercase() && remaining.iter().all(|character| character.is_lowercase()) {
        let mut capitalized = false;
        replacement
            .to_lowercase()
            .chars()
            .flat_map(|character| {
                if !capitalized && character.is_lowercase() {
                    capitalized = true;
                    character.to_uppercase().collect::<Vec<_>>()
                } else {
                    vec![character]
                }
            })
            .collect()
    } else {
        replacement.to_owned()
    }
}

fn build_file_results(
    buffer: Entity<Buffer>,
    ranges: Vec<Range<Anchor>>,
    cx: &mut App,
) -> Option<FileResults> {
    let snapshot = buffer.read(cx).snapshot();
    let file = snapshot.file()?;
    let path = file.path().display(file.path_style(cx)).to_string();
    let file_name = Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    let directory = Path::new(&path)
        .parent()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let matches = ranges
        .into_iter()
        .map(|range| {
            let start = snapshot.summary_for_anchor::<Point>(&range.start);
            let end = snapshot.summary_for_anchor::<Point>(&range.end);
            let line = snapshot
                .text_for_range(
                    Point::new(start.row, 0)..Point::new(start.row, snapshot.line_len(start.row)),
                )
                .collect::<String>();
            let highlight_end = if start.row == end.row {
                end.column
            } else {
                snapshot.line_len(start.row)
            };
            let (preview, highlight_ranges) =
                match_preview(&line, start.column as usize..highlight_end as usize);
            PanelMatch {
                buffer: buffer.clone(),
                matched_text: snapshot
                    .text_for_range(range.clone())
                    .collect::<String>()
                    .into(),
                range,
                line_number: start.row + 1,
                preview,
                highlight_ranges,
            }
        })
        .collect();
    Some(FileResults {
        icon: FileIcons::get_icon(Path::new(&path), cx),
        path_label: path.into(),
        file_name: file_name.into(),
        directory: directory.into(),
        matches,
        collapsed: false,
    })
}

impl Render for SearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query_input = self
            .render_input(InputPanel::Query, &self.query_editor, cx)
            .child(
                h_flex()
                    .flex_none()
                    .child(self.render_option(SearchOption::CaseSensitive, cx))
                    .child(self.render_option(SearchOption::WholeWord, cx))
                    .child(self.render_option(SearchOption::Regex, cx)),
            );
        let replacement_input = self.replace_visible.then(|| {
            h_flex()
                .w_full()
                .gap_1()
                .child(
                    self.render_input(InputPanel::Replacement, &self.replacement_editor, cx)
                        .flex_1()
                        .child(
                            Button::new("preserve-case", "AB")
                                .style(ButtonStyle::Transparent)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .toggle_state(self.preserve_case)
                                .tooltip(Tooltip::text("Preserve Case"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.preserve_case = !this.preserve_case;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    IconButton::new("replace-all", IconName::ReplaceAll)
                        .shape(IconButtonShape::Square)
                        .disabled(!self.can_replace())
                        .tooltip(Tooltip::text("Replace All"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.replace(ReplacementScope::All, window, cx)
                        })),
                )
        });
        let errors = [
            InputPanel::Query,
            InputPanel::Replacement,
            InputPanel::Include,
            InputPanel::Exclude,
        ]
        .into_iter()
        .filter_map(|input| self.panels_with_errors.get(&input))
        .map(|error| {
            Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
        })
        .collect::<Vec<_>>();
        let filters = self.filters_visible.then(|| {
            v_flex()
                .pl_7()
                .gap_1()
                .child(
                    Label::new("files to include")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(self.render_input(InputPanel::Include, &self.included_files_editor, cx))
                .child(
                    Label::new("files to exclude")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    self.render_input(InputPanel::Exclude, &self.excluded_files_editor, cx)
                        .child(
                            IconButton::new("ignored", IconName::Settings)
                                .shape(IconButtonShape::Square)
                                .toggle_state(
                                    self.search_options.contains(SearchOptions::INCLUDE_IGNORED),
                                )
                                .tooltip(Tooltip::text("Include Ignored Files"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_option(SearchOptions::INCLUDE_IGNORED, cx)
                                })),
                        ),
                )
        });
        let searching = self.search_state == SearchState::Searching;
        let summary = if searching {
            format!(
                "{} results in {} files",
                self.match_count(),
                self.results.len()
            )
        } else {
            match self.search_state {
                SearchState::Idle => {
                    if self.query_text(cx).is_empty() {
                        "Type to search".into()
                    } else {
                        "Press Enter to search".into()
                    }
                }
                SearchState::NoResults => "No results found".into(),
                _ => format!(
                    "{} results in {} files",
                    self.match_count(),
                    self.results.len()
                ),
            }
        };
        v_flex()
            .key_context("ForkSearchPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .min_w_0()
            .bg(cx.theme().colors().panel_background)
            .on_action(
                cx.listener(|this, _: &ToggleReplace, window, cx| this.toggle_replace(window, cx)),
            )
            .on_action(cx.listener(|this, _: &TogglePreserveCase, _, cx| {
                this.preserve_case = !this.preserve_case;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ReplaceSelected, window, cx| {
                this.replace_selected(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ReplaceAll, window, cx| {
                this.replace(ReplacementScope::All, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Search, _, cx| this.search(cx)))
            .on_action(cx.listener(|this, _: &Clear, window, cx| this.clear(window, cx)))
            .on_action(
                cx.listener(|this, _: &Confirm, window, cx| this.open_selected(false, window, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                this.open_selected(true, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &FocusResults, window, cx| this.focus_results(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select_next(cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.select_previous(cx)))
            .on_action(cx.listener(|this, _: &Collapse, _, cx| this.collapse_selected(cx)))
            .on_action(cx.listener(|this, _: &Expand, _, cx| this.expand_selected(cx)))
            .on_action(cx.listener(|this, _: &ToggleCollapseAll, _, cx| this.collapse_all(cx)))
            .on_action(cx.listener(|this, _: &ToggleFilters, _, cx| {
                this.filters_visible = !this.filters_visible;
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &OpenInEditor, window, cx| this.open_in_editor(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| {
                this.toggle_option(SearchOptions::REGEX, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _, cx| {
                this.toggle_option(SearchOptions::CASE_SENSITIVE, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleWholeWord, _, cx| {
                this.toggle_option(SearchOptions::WHOLE_WORD, cx)
            }))
            .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                this.workspace
                    .update(cx, |workspace, cx| {
                        workspace.active_pane().focus_handle(cx).focus(window, cx)
                    })
                    .log_err();
            }))
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .flex_none()
                    .px_3()
                    .gap_1()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                IconButton::new(
                                    "toggle-replace",
                                    if self.replace_visible {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    },
                                )
                                .shape(IconButtonShape::Square)
                                .toggle_state(self.replace_visible)
                                .tooltip(Tooltip::text("Toggle Replace"))
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.toggle_replace(window, cx),
                                )),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(query_input)
                                    .children(replacement_input),
                            ),
                    )
                    .child(
                        h_flex().justify_end().child(
                            IconButton::new("filters", IconName::Ellipsis)
                                .shape(IconButtonShape::Square)
                                .toggle_state(self.filters_visible)
                                .tooltip(Tooltip::text("Toggle Search Details"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.filters_visible = !this.filters_visible;
                                    cx.notify();
                                })),
                        ),
                    )
                    .children(filters)
                    .children(errors)
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .py_1()
                            .when(searching, |row| {
                                row.child(
                                    Icon::new(IconName::ArrowCircle)
                                        .size(IconSize::Small)
                                        .with_rotate_animation(2),
                                )
                            })
                            .child(
                                Label::new(summary)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when(!self.results.is_empty(), |row| {
                                row.child(
                                    Button::new("summary-open", "Open in editor")
                                        .style(ButtonStyle::Transparent)
                                        .size(ButtonSize::Compact)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_in_editor(window, cx)
                                        })),
                                )
                            }),
                    )
                    .when(self.search_state == SearchState::LimitReached, |column| {
                        column.child(
                            Label::new("Search limit reached. Narrow your query.")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .track_focus(&self.results_focus_handle)
                    .child(
                        uniform_list(
                            "search-results",
                            self.rows.len(),
                            cx.processor(|this, range: Range<usize>, _, cx| {
                                range
                                    .map(|index| this.render_row(index, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.scroll_handle),
                    ),
            )
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl EventEmitter<PanelEvent> for SearchPanel {}
impl Panel for SearchPanel {
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_editor.focus_handle(cx)
    }
    fn persistent_name() -> &'static str {
        "Search"
    }
    fn panel_key() -> &'static str {
        "ForkSearchPanel"
    }
    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match SearchPanelSettings::get_global(cx).dock {
            ForkSearchPanelDock::Left => DockPosition::Left,
            ForkSearchPanelDock::Right => DockPosition::Right,
            ForkSearchPanelDock::Bottom => DockPosition::Bottom,
        }
    }
    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }
    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        let dock = match position {
            DockPosition::Left => ForkSearchPanelDock::Left,
            DockPosition::Right => ForkSearchPanelDock::Right,
            DockPosition::Bottom => ForkSearchPanelDock::Bottom,
        };
        settings::update_settings_file(
            self.project.read(cx).fs().clone(),
            cx,
            move |settings, _| {
                settings
                    .fork
                    .get_or_insert_default()
                    .search_panel
                    .get_or_insert_default()
                    .dock = Some(dock);
            },
        );
    }
    fn default_size(&self, _: &Window, cx: &App) -> Pixels {
        px(SearchPanelSettings::get_global(cx).default_width)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::MagnifyingGlass)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Search")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }
    fn starts_open(&self, _: &Window, _: &App) -> bool {
        self.active
    }
    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        cx.notify();
    }
    fn activation_priority(&self) -> u32 {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use util::path;

    async fn setup_workspace(
        cx: &mut TestAppContext,
        enabled: bool,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .fork
                        .get_or_insert_default()
                        .search_panel
                        .get_or_insert_default()
                        .enabled = Some(enabled);
                });
            });
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            search::init(cx);
            init(cx);
        });
        let filesystem = FakeFs::new(cx.background_executor.clone());
        filesystem
            .insert_tree(
                path!("/project"),
                json!({
                    ".gitignore": "ignored.txt\n",
                    "ignored.txt": "needle",
                    "one.rs": "needle Needle needles\n  中文 needle\n",
                    "src": { "two.rs": "needle\nother\n" },
                    "three.txt": "other\n"
                }),
            )
            .await;
        let project = Project::test(filesystem, [path!("/project").as_ref()], cx).await;
        let (multiworkspace, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multiworkspace.read_with(cx, |multiworkspace, _| multiworkspace.workspace().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            let search_bar = cx.new(|_| search::project_search::ProjectSearchBar::new());
            workspace.active_pane().update(cx, |pane, cx| {
                pane.toolbar()
                    .update(cx, |toolbar, cx| toolbar.add_item(search_bar, window, cx));
            });
            attach_workspace(workspace, window, cx);
        });
        cx.run_until_parked();
        (workspace, cx)
    }

    async fn setup(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<SearchPanel>,
        &mut VisualTestContext,
    ) {
        let (workspace, cx) = setup_workspace(cx, true).await;
        let panel = workspace.update_in(cx, ensure_panel);
        cx.run_until_parked();
        (workspace, panel, cx)
    }

    fn perform_search(
        panel: &Entity<SearchPanel>,
        text: &str,
        options: SearchOptions,
        included: &str,
        excluded: &str,
        cx: &mut VisualTestContext,
    ) {
        panel.update_in(cx, |panel, window, cx| {
            panel
                .query_editor
                .update(cx, |editor, cx| editor.set_text(text, window, cx));
            panel
                .included_files_editor
                .update(cx, |editor, cx| editor.set_text(included, window, cx));
            panel
                .excluded_files_editor
                .update(cx, |editor, cx| editor.set_text(excluded, window, cx));
            panel.search_options = options;
            panel.search(cx);
        });
        cx.run_until_parked();
    }

    fn count(panel: &Entity<SearchPanel>, cx: &VisualTestContext) -> usize {
        panel.read_with(cx, |panel, _| panel.match_count())
    }

    async fn add_project_panel(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<project_panel::ProjectPanel> {
        let panel = workspace
            .update_in(cx, |_, window, cx| {
                cx.spawn_in(window, async move |workspace, cx| {
                    project_panel::ProjectPanel::load(workspace, cx.clone()).await
                })
            })
            .await
            .expect("project panel should load");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        cx.run_until_parked();
        panel
    }

    fn find_in_folder(
        workspace: &Entity<Workspace>,
        project_panel: &Entity<project_panel::ProjectPanel>,
        path: &str,
        cx: &mut VisualTestContext,
    ) {
        workspace.update_in(cx, |workspace, window, cx| {
            let project_path = workspace
                .project()
                .read(cx)
                .find_project_path(path, cx)
                .expect("selected path should exist");
            workspace.focus_panel::<project_panel::ProjectPanel>(window, cx);
            project_panel.update(cx, |panel, cx| {
                panel.select_path_for_test(project_path, cx);
            });
        });
        cx.run_until_parked();
        cx.dispatch_action(project_panel::NewSearchInDirectory);
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_find_in_folder_routes_to_sidebar(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        let project_panel = add_project_panel(&workspace, cx).await;
        perform_search(
            &panel,
            "needle",
            SearchOptions::empty(),
            "one.rs",
            "*.rs",
            cx,
        );
        let item_count = workspace.read_with(cx, |workspace, cx| {
            workspace.active_pane().read(cx).items_len()
        });
        for path in [path!("/project/src"), path!("/project/src/two.rs")] {
            find_in_folder(&workspace, &project_panel, path, cx);
            panel.read_with(cx, |panel, cx| {
                assert!(panel.filters_visible);
                assert_eq!(panel.query_text(cx), "needle");
                assert_eq!(panel.included_files_editor.read(cx).text(cx), "src");
                assert_eq!(panel.excluded_files_editor.read(cx).text(cx), "");
                assert_eq!(panel.match_count(), 1);
                assert!(
                    panel
                        .results
                        .iter()
                        .all(|file| file.path_label.as_ref() == "src/two.rs")
                );
            });
            cx.update(|window, cx| {
                assert!(
                    panel
                        .read(cx)
                        .query_editor
                        .focus_handle(cx)
                        .is_focused(window)
                );
            });
        }
        find_in_folder(&workspace, &project_panel, path!("/project/one.rs"), cx);
        assert_eq!(count(&panel, cx), 5);
        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.included_files_editor.read(cx).text(cx), "");
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.active_pane().read(cx).items_len(), item_count);
            assert!(
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .all(|item| { item.downcast::<search::ProjectSearchView>().is_none() })
            );
        });
    }

    #[gpui::test]
    async fn test_find_in_folder_scopes_multiple_roots(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let filesystem = project.read_with(cx, |project, _| project.fs().as_fake());
        filesystem
            .insert_tree(
                path!("/other"),
                json!({ "src": { "two.rs": "needle needle" } }),
            )
            .await;
        project
            .update(cx, |project, cx| {
                project.create_worktree(path!("/other"), true, cx)
            })
            .await
            .expect("second worktree should load");
        cx.run_until_parked();
        let project_panel = add_project_panel(&workspace, cx).await;
        perform_search(&panel, "needle", SearchOptions::empty(), "", "", cx);
        for (path, included, matches) in [
            (path!("/project/src"), "project/src", 1),
            (path!("/other/src/two.rs"), "other/src", 2),
            (path!("/other"), "other", 2),
        ] {
            find_in_folder(&workspace, &project_panel, path, cx);
            panel.read_with(cx, |panel, cx| {
                assert_eq!(panel.included_files_editor.read(cx).text(cx), included);
                assert_eq!(panel.match_count(), matches);
                assert!(
                    panel
                        .results
                        .iter()
                        .all(|file| file.matches.iter().all(|matched| {
                            matched
                                .buffer
                                .read(cx)
                                .file()
                                .is_some_and(|file| file.full_path(cx).starts_with(included))
                        }))
                );
            });
        }
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .items()
                .all(|item| item.downcast::<search::ProjectSearchView>().is_none())
        }));
    }

    #[gpui::test]
    async fn test_find_in_folder_disabled_fallback_and_reenabled(cx: &mut TestAppContext) {
        let (workspace, cx) = setup_workspace(cx, false).await;
        let project_panel = add_project_panel(&workspace, cx).await;
        find_in_folder(&workspace, &project_panel, path!("/project/src"), cx);
        let view = workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.panel::<SearchPanel>(cx).is_none());
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<search::ProjectSearchView>())
                .expect("disabled sidebar should fall back to a search tab")
        });
        search::project_search::perform_project_search(&view, "needle", cx);
        assert_eq!(
            view.read_with(cx, |view, cx| {
                view.results_editor()
                    .read(cx)
                    .text(cx)
                    .matches("needle")
                    .count()
            }),
            1
        );
        for enabled in [true, false, true] {
            cx.update(|_, cx| {
                SettingsStore::update_global(cx, |store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings
                            .fork
                            .get_or_insert_default()
                            .search_panel
                            .get_or_insert_default()
                            .enabled = Some(enabled);
                    });
                });
            });
            cx.run_until_parked();
            let item_count = workspace.read_with(cx, |workspace, cx| {
                workspace.active_pane().read(cx).items_len()
            });
            find_in_folder(&workspace, &project_panel, path!("/project/src"), cx);
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(workspace.panel::<SearchPanel>(cx).is_some(), enabled);
                assert_eq!(
                    workspace.active_pane().read(cx).items_len(),
                    item_count + usize::from(!enabled)
                );
            });
        }
    }

    #[gpui::test]
    async fn test_grouping_debounce_and_cancelled_search(cx: &mut TestAppContext) {
        let (_, panel, cx) = setup(cx).await;
        perform_search(&panel, "needle", SearchOptions::empty(), "", "", cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.search_state, SearchState::Results);
            assert_eq!(panel.results.len(), 2);
            assert_eq!(panel.match_count(), 5);
            assert_eq!(panel.rows.len(), 7);
        });
        panel.update_in(cx, |panel, window, cx| {
            panel
                .query_editor
                .update(cx, |editor, cx| editor.set_text("other", window, cx));
            panel.search(cx);
            panel
                .query_editor
                .update(cx, |editor, cx| editor.set_text("中文", window, cx));
        });
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 0);
        cx.executor().advance_clock(Duration::from_millis(299));
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 0);
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.match_count(), 1);
            assert_eq!(panel.results.len(), 1);
            assert_eq!(
                panel.results.first().map(|file| file.file_name.as_ref()),
                Some("one.rs")
            );
            assert_eq!(panel.search_state, SearchState::Results);
        });
        panel.update_in(cx, |panel, window, cx| panel.clear(window, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.search_state, SearchState::Idle);
            assert!(panel.results.is_empty());
            assert!(panel.panels_with_errors.is_empty());
        });
    }

    #[gpui::test]
    async fn test_options_filters_and_errors(cx: &mut TestAppContext) {
        let (_, panel, cx) = setup(cx).await;
        perform_search(&panel, "needle", SearchOptions::CASE_SENSITIVE, "", "", cx);
        assert_eq!(count(&panel, cx), 4);
        perform_search(&panel, "needle", SearchOptions::WHOLE_WORD, "", "", cx);
        assert_eq!(count(&panel, cx), 4);
        perform_search(&panel, "n.edle", SearchOptions::REGEX, "", "", cx);
        assert_eq!(count(&panel, cx), 5);
        perform_search(&panel, "needle", SearchOptions::INCLUDE_IGNORED, "", "", cx);
        assert_eq!(count(&panel, cx), 6);
        perform_search(
            &panel,
            "needle",
            SearchOptions::empty(),
            "src/**/*.rs",
            "",
            cx,
        );
        assert_eq!(count(&panel, cx), 1);
        perform_search(&panel, "needle", SearchOptions::empty(), "", "src/**", cx);
        assert_eq!(count(&panel, cx), 4);
        perform_search(&panel, "[", SearchOptions::REGEX, "", "", cx);
        panel.read_with(cx, |panel, _| {
            assert!(panel.panels_with_errors.contains_key(&InputPanel::Query));
            assert!(panel.pending_search.is_none());
            assert_eq!(panel.match_count(), 0);
        });
        perform_search(&panel, "needle", SearchOptions::empty(), "[", "{", cx);
        panel.read_with(cx, |panel, _| {
            assert!(panel.panels_with_errors.contains_key(&InputPanel::Include));
            assert!(panel.panels_with_errors.contains_key(&InputPanel::Exclude));
            assert!(panel.pending_search.is_none());
        });
        perform_search(&panel, "missing", SearchOptions::empty(), "", "", cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.search_state, SearchState::NoResults);
            assert!(panel.panels_with_errors.is_empty());
        });
    }

    #[gpui::test]
    async fn test_collapse_navigation_and_open_match(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        perform_search(&panel, "needle", SearchOptions::CASE_SENSITIVE, "", "", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.focus_results(window, cx);
            panel.collapse_all(cx);
            assert_eq!(panel.rows.len(), 2);
            panel.select_row(0, cx);
            panel.select_next(cx);
            assert_eq!(panel.selected_row, Some(1));
            panel.expand_selected(cx);
            assert!(panel.rows.len() > 2);
            panel.select_next(cx);
            let selected = panel.selected_row.and_then(|index| panel.rows.get(index));
            assert!(matches!(selected, Some(ResultRow::Match { .. })));
            panel.open_selected(false, window, cx);
            assert!(panel.results_focus_handle.is_focused(window));
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let editor = workspace
                .read(cx)
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx));
            assert_eq!(
                editor
                    .as_ref()
                    .and_then(|editor| selection_query(editor.read(cx), cx)),
                Some("needle".into())
            );
            panel.update(cx, |panel, cx| {
                panel.open_selected(true, window, cx);
                panel.collapse_selected(cx);
                let selected = panel.selected_row.and_then(|index| panel.rows.get(index));
                assert!(matches!(selected, Some(ResultRow::File(_))));
                panel.collapse_all(cx);
                panel.collapse_all(cx);
                assert_eq!(panel.rows.len(), panel.results.len() + panel.match_count());
            });
            assert!(editor.is_some_and(|editor| editor.focus_handle(cx).is_focused(window)));
        });
    }

    #[gpui::test]
    async fn test_deploy_seeds_selection_and_open_in_editor(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        let options = SearchOptions::CASE_SENSITIVE | SearchOptions::WHOLE_WORD;
        perform_search(&panel, "needle", options, "src/**", "*.txt", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.select_row(1, cx);
            panel.open_selected(true, window, cx);
        });
        cx.run_until_parked();
        panel.update_in(cx, |panel, window, cx| {
            panel
                .query_editor
                .update(cx, |editor, cx| editor.set_text("stale", window, cx));
        });
        cx.dispatch_action(Deploy);
        cx.run_until_parked();
        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.query_text(cx), "needle");
            assert_eq!(
                selection_query(panel.query_editor.read(cx), cx),
                Some("needle".into())
            );
        });
        panel.update_in(cx, |panel, window, cx| {
            let action = panel.editor_search_action(cx);
            assert_eq!(action.case_sensitive, Some(true));
            assert_eq!(action.whole_word, Some(true));
            assert_eq!(action.regex, Some(false));
            assert_eq!(action.include_ignored, Some(false));
            assert_eq!(action.included_files.as_deref(), Some("src/**"));
            assert_eq!(action.excluded_files.as_deref(), Some("*.txt"));
            panel.open_in_editor(window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(Confirm);
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let view = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<search::ProjectSearchView>());
            assert_eq!(
                view.as_ref()
                    .map(|view| view.read(cx).search_query_text(cx)),
                Some("needle".into())
            );
            assert_eq!(
                view.as_ref().map(|view| view
                    .read(cx)
                    .results_editor()
                    .read(cx)
                    .text(cx)
                    .matches("needle")
                    .count()),
                Some(1)
            );
        });
    }

    #[gpui::test]
    async fn test_disabled_at_startup_and_reenabled(cx: &mut TestAppContext) {
        let (workspace, cx) = setup_workspace(cx, false).await;
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<SearchPanel>(cx).is_none()
        }));
        cx.dispatch_action(Deploy);
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<SearchPanel>(cx).is_none()
                && workspace
                    .active_item(cx)
                    .is_some_and(|item| item.downcast::<search::ProjectSearchView>().is_some())
        }));
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .fork
                        .get_or_insert_default()
                        .search_panel
                        .get_or_insert_default()
                        .enabled = Some(true);
                });
            });
        });
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<SearchPanel>(cx).is_some()
        }));
    }

    #[gpui::test]
    async fn test_deploy_preserves_query_without_single_line_selection(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        perform_search(&panel, "needle", SearchOptions::empty(), "one.rs", "", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.select_row(1, cx);
            panel.open_selected(true, window, cx);
        });
        cx.run_until_parked();
        for selection in [
            Point::new(0, 0)..Point::new(0, 0),
            Point::new(0, 0)..Point::new(1, 2),
        ] {
            cx.update(|window, cx| {
                let editor = workspace
                    .read(cx)
                    .active_item(cx)
                    .and_then(|item| item.act_as::<Editor>(cx));
                assert!(editor.is_some());
                if let Some(editor) = editor {
                    editor.update(cx, |editor, cx| {
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_ranges([selection]);
                        });
                    });
                    editor.focus_handle(cx).focus(window, cx);
                }
                panel.update(cx, |panel, cx| {
                    panel.query_editor.update(cx, |editor, cx| {
                        editor.set_text("retained", window, cx);
                    });
                });
            });
            cx.dispatch_action(Deploy);
            cx.run_until_parked();
            panel.read_with(cx, |panel, cx| {
                assert_eq!(panel.query_text(cx), "retained");
                assert_eq!(
                    selection_query(panel.query_editor.read(cx), cx),
                    Some("retained".into())
                );
            });
        }
    }

    #[gpui::test]
    async fn test_disabled_fallback_and_manual_search(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .fork
                        .get_or_insert_default()
                        .search_panel
                        .get_or_insert_default()
                        .search_on_type = Some(false);
                });
            })
        });
        panel.update_in(cx, |panel, window, cx| {
            panel
                .query_editor
                .update(cx, |editor, cx| editor.set_text("needle", window, cx));
        });
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 0);
        panel.update(cx, |panel, cx| panel.search(cx));
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 5);
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .fork
                        .get_or_insert_default()
                        .search_panel
                        .get_or_insert_default()
                        .enabled = Some(false);
                });
            })
        });
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.panel::<SearchPanel>(cx).is_none()
        }));
        cx.update(|window, cx| {
            workspace
                .read(cx)
                .active_pane()
                .focus_handle(cx)
                .focus(window, cx)
        });
        cx.dispatch_action(Deploy);
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .is_some_and(|item| item.downcast::<search::ProjectSearchView>().is_some())
        }));
    }

    #[gpui::test]
    async fn test_match_highlights_use_theme_background(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<SearchPanel>(window, cx);
        });
        perform_search(&panel, "中文", SearchOptions::empty(), "one.rs", "", cx);
        panel.read_with(cx, |panel, cx| {
            assert_eq!(panel.match_count(), 1);
            for matched in panel.results.iter().flat_map(|file| &file.matches) {
                let highlights = search_match_highlights(&matched.highlight_ranges, cx);
                assert_eq!(highlights.len(), matched.highlight_ranges.len());
                for ((range, style), expected_range) in
                    highlights.iter().zip(&matched.highlight_ranges)
                {
                    assert_eq!(range, expected_range);
                    assert_eq!(matched.preview.get(range.clone()), Some("中文"));
                    assert_eq!(
                        style.background_color,
                        Some(cx.theme().colors().search_match_background)
                    );
                    assert_eq!(style.color, None);
                    assert_eq!(style.font_weight, None);
                }
                StyledText::new(matched.preview.clone()).with_highlights(highlights);
            }
        });
        perform_search(
            &panel,
            "needle",
            SearchOptions::CASE_SENSITIVE,
            "one.rs",
            "",
            cx,
        );
        panel.read_with(cx, |panel, cx| {
            for matched in panel.results.iter().flat_map(|file| &file.matches) {
                for (range, style) in search_match_highlights(&matched.highlight_ranges, cx) {
                    assert_eq!(matched.preview.get(range), Some("needle"));
                    assert_eq!(
                        style.background_color,
                        Some(cx.theme().colors().search_match_background)
                    );
                    assert_eq!(style.color, None);
                }
            }
        });
    }

    #[test]
    fn test_previews_are_utf8_safe_and_keep_matches_visible() {
        let (preview, highlights) = match_preview("  中文 needle", 9..15);
        assert_eq!(preview.as_ref(), "中文 needle");
        assert_eq!(
            highlights
                .first()
                .and_then(|range| preview.get(range.clone())),
            Some("needle")
        );
        let line = format!("{}中文后缀", "前".repeat(80));
        let (preview, highlights) = match_preview(&line, 240..246);
        assert!(preview.starts_with('…'));
        assert_eq!(
            highlights
                .first()
                .and_then(|range| preview.get(range.clone())),
            Some("中文")
        );
        for start in 0..line.len() {
            let (preview, highlights) = match_preview(&line, start..start + 1);
            for range in highlights {
                assert!(preview.get(range).is_some());
            }
        }
        let (preview, _) = match_preview("  needle", 0..2);
        assert_eq!(preview.as_ref(), "  needle");
    }

    fn result_buffers(panel: &Entity<SearchPanel>, cx: &VisualTestContext) -> Vec<Entity<Buffer>> {
        panel.read_with(cx, |panel, _| {
            panel
                .results
                .iter()
                .filter_map(|file| file.matches.first().map(|hit| hit.buffer.clone()))
                .collect()
        })
    }

    fn set_replacement(panel: &Entity<SearchPanel>, text: &str, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            panel
                .replacement_editor
                .update(cx, |editor, cx| editor.set_text(text, window, cx));
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_replace_input_focus_and_no_search_restart(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<SearchPanel>(window, cx);
        });
        cx.run_until_parked();
        perform_search(&panel, "needle", SearchOptions::empty(), "", "", cx);
        let search_id = panel.read_with(cx, |panel, _| panel.search_id);
        panel.update_in(cx, |panel, window, cx| panel.toggle_replace(window, cx));
        set_replacement(&panel, "thread", cx);
        panel.read_with(cx, |panel, _| {
            assert!(panel.replace_visible);
            assert_eq!(panel.search_id, search_id);
            assert_eq!(panel.match_count(), 5);
        });
        cx.update(|window, cx| {
            assert!(
                panel
                    .read(cx)
                    .replacement_editor
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });
        panel.update_in(cx, |panel, window, cx| panel.toggle_replace(window, cx));
        panel.read_with(cx, |panel, cx| {
            assert!(!panel.replace_visible);
            assert_eq!(panel.replacement_editor.read(cx).text(cx), "thread");
        });
        cx.update(|window, cx| {
            assert!(
                panel
                    .read(cx)
                    .query_editor
                    .focus_handle(cx)
                    .is_focused(window)
            );
        });
    }

    #[gpui::test]
    async fn test_replace_single_undo_and_retained_unsaved_buffer(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<SearchPanel>(window, cx);
        });
        cx.run_until_parked();
        perform_search(&panel, "needle", SearchOptions::empty(), "one.rs", "", cx);
        let buffers = result_buffers(&panel, cx);
        assert_eq!(buffers.len(), 1);
        for buffer in buffers {
            let original = buffer.read_with(cx, |buffer, _| buffer.text());
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "unsaved ")], None, cx);
            });
            perform_search(&panel, "needle", SearchOptions::empty(), "one.rs", "", cx);
            set_replacement(&panel, "thread", cx);
            panel.update_in(cx, |panel, window, cx| {
                panel.toggle_replace(window, cx);
                panel.replace_selected(window, cx);
            });
            cx.run_until_parked();
            buffer.read_with(cx, |buffer, _| {
                assert_eq!(
                    buffer.text(),
                    format!("unsaved {}", original.replacen("needle", "thread", 1))
                );
                assert!(buffer.is_dirty());
            });
            assert_eq!(count(&panel, cx), 3);
            assert!(panel.read_with(cx, |panel, _| panel.panels_with_errors.is_empty()));
            cx.update(|window, cx| {
                assert!(
                    panel
                        .read(cx)
                        .replacement_editor
                        .focus_handle(cx)
                        .is_focused(window)
                );
            });
            buffer.update(cx, |buffer, cx| {
                assert!(buffer.undo(cx).is_some());
            });
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                format!("unsaved {original}")
            );
        }
        panel.update_in(cx, |panel, window, cx| panel.clear(window, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let editors = workspace.items_of_type::<Editor>(cx).collect::<Vec<_>>();
            assert_eq!(editors.len(), 1);
            for editor in editors {
                assert!(editor.read(cx).text(cx).starts_with("unsaved needle"));
            }
        });
    }

    #[gpui::test]
    async fn test_replace_all_confirmation_and_preserve_case(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        perform_search(&panel, "needle", SearchOptions::empty(), "", "", cx);
        let buffers = result_buffers(&panel, cx);
        assert_eq!(buffers.len(), 2);
        let originals = buffers
            .iter()
            .map(|buffer| buffer.read_with(cx, |buffer, _| buffer.text()))
            .collect::<Vec<_>>();
        set_replacement(&panel, "THREAD", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.preserve_case = true;
            panel.replace(ReplacementScope::All, window, cx);
        });
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 5);
        for (buffer, original) in buffers.iter().zip(&originals) {
            assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), *original);
        }
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .items_of_type::<Editor>(cx)
                .count()),
            0
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.replace(ReplacementScope::All, window, cx)
        });
        cx.simulate_prompt_answer("Replace");
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 0);
        assert!(panel.read_with(cx, |panel, _| panel.panels_with_errors.is_empty()));
        for (buffer, original) in buffers.iter().zip(&originals) {
            buffer.read_with(cx, |buffer, _| {
                assert_eq!(
                    buffer.text(),
                    original
                        .replace("needle", "thread")
                        .replace("Needle", "Thread")
                );
                assert!(buffer.is_dirty());
            });
        }
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .items_of_type::<Editor>(cx)
                .count()),
            2
        );
        let project = panel.read_with(cx, |panel, _| panel.project.clone());
        let filesystem = project.read_with(cx, |project, _| project.fs().clone());
        match filesystem.load(path!("/project/one.rs").as_ref()).await {
            Ok(text) => assert_eq!(text, "needle Needle needles\n  中文 needle\n"),
            Err(error) => panic!("Could not read test file: {error}"),
        }
    }

    #[gpui::test]
    async fn test_replace_file_empty_text_and_limit_guard(cx: &mut TestAppContext) {
        let (_, panel, cx) = setup(cx).await;
        perform_search(
            &panel,
            "needle",
            SearchOptions::CASE_SENSITIVE,
            "one.rs",
            "",
            cx,
        );
        let buffers = result_buffers(&panel, cx);
        assert_eq!(buffers.len(), 1);
        panel.update_in(cx, |panel, window, cx| {
            panel.search_state = SearchState::LimitReached;
            panel.replace(ReplacementScope::All, window, cx);
            assert!(panel.replacement_task.is_none());
            panel.search_state = SearchState::Searching;
            panel.replace(ReplacementScope::File(0), window, cx);
            assert!(panel.replacement_task.is_none());
            panel.search_state = SearchState::Results;
            panel.replace(ReplacementScope::File(0), window, cx);
        });
        cx.simulate_prompt_answer("Replace");
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 0);
        for buffer in buffers {
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                " Needle s\n  中文 \n"
            );
        }
    }

    #[gpui::test]
    async fn test_replace_regex_captures_context_and_zero_width(cx: &mut TestAppContext) {
        let (_, panel, cx) = setup(cx).await;
        perform_search(&panel, "needle", SearchOptions::empty(), "one.rs", "", cx);
        let buffers = result_buffers(&panel, cx);
        assert_eq!(buffers.len(), 1);
        for buffer in buffers {
            buffer.update(cx, |buffer, cx| {
                let length = buffer.len();
                buffer.edit([(0..length, "前中文 前中文")], None, cx);
            });
            perform_search(
                &panel,
                r"(?<=前)(?<word>中)(文)",
                SearchOptions::REGEX,
                "one.rs",
                "",
                cx,
            );
            assert_eq!(count(&panel, cx), 2);
            set_replacement(&panel, r"${word}-$2\n", cx);
            panel.update_in(cx, |panel, window, cx| {
                panel.replace(ReplacementScope::File(0), window, cx)
            });
            cx.simulate_prompt_answer("Replace");
            cx.run_until_parked();
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                "前中-文\n 前中-文\n"
            );
            assert!(panel.read_with(cx, |panel, _| panel.panels_with_errors.is_empty()));
            perform_search(&panel, r"(?=前)", SearchOptions::REGEX, "one.rs", "", cx);
            assert_eq!(count(&panel, cx), 2);
            set_replacement(&panel, "注", cx);
            panel.update_in(cx, |panel, window, cx| {
                panel.replace(ReplacementScope::All, window, cx)
            });
            cx.simulate_prompt_answer("Replace");
            cx.run_until_parked();
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                "注前中-文\n 注前中-文\n"
            );
        }
    }

    #[gpui::test]
    async fn test_replace_rejects_stale_whole_word_context_and_changed_inputs(
        cx: &mut TestAppContext,
    ) {
        let (_, panel, cx) = setup(cx).await;
        perform_search(
            &panel,
            "needle",
            SearchOptions::WHOLE_WORD,
            "src/two.rs",
            "",
            cx,
        );
        let buffers = result_buffers(&panel, cx);
        assert_eq!(buffers.len(), 1);
        set_replacement(&panel, "thread", cx);
        for buffer in buffers {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(6..6, "s")], None, cx);
            });
            panel.update_in(cx, |panel, window, cx| panel.replace_selected(window, cx));
            cx.run_until_parked();
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                "needles\nother\n"
            );
            assert!(panel.read_with(cx, |panel, _| {
                panel
                    .panels_with_errors
                    .contains_key(&InputPanel::Replacement)
            }));
            perform_search(
                &panel,
                "needles",
                SearchOptions::empty(),
                "src/two.rs",
                "",
                cx,
            );
            panel.update_in(cx, |panel, window, cx| {
                panel.replace(ReplacementScope::All, window, cx);
                panel
                    .replacement_editor
                    .update(cx, |editor, cx| editor.set_text("different", window, cx));
            });
            cx.simulate_prompt_answer("Replace");
            cx.run_until_parked();
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                "needles\nother\n"
            );
            assert!(panel.read_with(cx, |panel, _| {
                panel
                    .panels_with_errors
                    .contains_key(&InputPanel::Replacement)
            }));
            panel.update_in(cx, |panel, window, cx| {
                panel.replace(ReplacementScope::All, window, cx);
                panel
                    .query_editor
                    .update(cx, |editor, cx| editor.set_text("other", window, cx));
            });
            cx.simulate_prompt_answer("Replace");
            cx.run_until_parked();
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                "needles\nother\n"
            );
        }
    }

    #[gpui::test]
    async fn test_replace_all_preflights_every_buffer(cx: &mut TestAppContext) {
        let (_, panel, cx) = setup(cx).await;
        perform_search(&panel, "needle", SearchOptions::empty(), "", "", cx);
        let buffers = result_buffers(&panel, cx);
        assert_eq!(buffers.len(), 2);
        let originals = buffers
            .iter()
            .map(|buffer| buffer.read_with(cx, |buffer, _| buffer.text()))
            .collect::<Vec<_>>();
        set_replacement(&panel, "thread", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.replace(ReplacementScope::All, window, cx)
        });
        if let Some(buffer) = buffers.last() {
            buffer.update(cx, |buffer, cx| {
                buffer.set_capability(language::Capability::ReadOnly, cx)
            });
        }
        cx.simulate_prompt_answer("Replace");
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| {
            panel
                .panels_with_errors
                .contains_key(&InputPanel::Replacement)
        }));
        for (buffer, original) in buffers.iter().zip(&originals) {
            assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), *original);
            buffer.update(cx, |buffer, cx| {
                buffer.set_capability(language::Capability::ReadWrite, cx)
            });
        }
        perform_search(&panel, "needle", SearchOptions::empty(), "", "", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.replace(ReplacementScope::All, window, cx)
        });
        if let Some(buffer) = buffers.last() {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "changed ")], None, cx);
            });
        }
        cx.simulate_prompt_answer("Replace");
        cx.run_until_parked();
        for (index, (buffer, original)) in buffers.iter().zip(&originals).enumerate() {
            let expected = if index + 1 == buffers.len() {
                format!("changed {original}")
            } else {
                original.clone()
            };
            assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), expected);
        }
        assert!(panel.read_with(cx, |panel, _| {
            panel
                .panels_with_errors
                .contains_key(&InputPanel::Replacement)
        }));
    }

    #[gpui::test]
    async fn test_replace_keyboard_context_and_confirmation(cx: &mut TestAppContext) {
        let (workspace, panel, cx) = setup(cx).await;
        cx.update(|_, cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-shift-h", ToggleReplace, Some("ForkSearchPanel")),
                gpui::KeyBinding::new("enter", Search, Some("fork_search_input > Editor")),
                gpui::KeyBinding::new(
                    "enter",
                    ReplaceSelected,
                    Some("fork_replace_input > Editor"),
                ),
                gpui::KeyBinding::new("cmd-enter", ReplaceAll, Some("fork_replace_input > Editor")),
            ]);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.focus_panel::<SearchPanel>(window, cx);
        });
        perform_search(&panel, "needle", SearchOptions::empty(), "one.rs", "", cx);
        set_replacement(&panel, "thread", cx);
        cx.simulate_keystrokes("cmd-shift-h");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 3);
        assert!(panel.read_with(cx, |panel, _| panel.panels_with_errors.is_empty()));
        cx.simulate_keystrokes("cmd-enter");
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 3);
        cx.simulate_keystrokes("cmd-enter");
        cx.simulate_prompt_answer("Replace");
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 0);
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .fork
                        .get_or_insert_default()
                        .search_panel
                        .get_or_insert_default()
                        .search_on_type = Some(false);
                });
            });
        });
        cx.simulate_keystrokes("cmd-shift-h");
        panel.update_in(cx, |panel, window, cx| {
            panel
                .query_editor
                .update(cx, |editor, cx| editor.set_text("thread", window, cx));
        });
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 0);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(count(&panel, cx), 4);
    }

    #[test]
    fn test_preserve_case_unicode_and_mixed_case() {
        assert_eq!(preserve_replacement_case("NEEDLE", "thread"), "THREAD");
        assert_eq!(preserve_replacement_case("needle", "THREAD"), "thread");
        assert_eq!(preserve_replacement_case("Needle", "tHREAD"), "Thread");
        assert_eq!(preserve_replacement_case("NeEdLe", "tHREAD"), "tHREAD");
        assert_eq!(preserve_replacement_case("École", "über"), "Über");
        assert_eq!(preserve_replacement_case("ÉCOLE", "straße"), "STRASSE");
        assert_eq!(preserve_replacement_case("中文", "tHREAD"), "tHREAD");
        assert_eq!(
            preserve_replacement_case("123Needle", "123tHREAD"),
            "123Thread"
        );
        assert_eq!(preserve_replacement_case("Needle", ""), "");
    }

    #[test]
    fn test_globs_preserve_nested_braces_and_escaped_commas() {
        assert_eq!(
            split_glob_patterns("src/{a,b}/**/*.rs, tests/**/*.rs"),
            vec!["src/{a,b}/**/*.rs", " tests/**/*.rs"]
        );
        assert_eq!(
            split_glob_patterns("a\\,b,{c,{d,e}}"),
            vec!["a\\,b", "{c,{d,e}}"]
        );
    }
}
