use fuzzy_nucleo::{StringMatch, StringMatchCandidate, StringMatchOptions, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity, Window, WindowHandle, actions,
};
use picker::{Picker, PickerDelegate};
use project::ProjectGroupKey;
use remote_connection::connect_with_modal;
use std::{collections::HashMap, sync::Arc};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt, paths::PathExt};
use workspace::{ModalView, MultiWorkspace, OpenMode, Workspace};

actions!(
    window_switcher,
    [
        /// Searches projects in all open windows and switches to the selected project.
        Toggle,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
                return;
            };
            let workspace = workspace.weak_handle();
            // Window handles and project groups cannot be read while the source window or
            // workspace is being updated by the action dispatcher.
            cx.defer(move |cx| {
                let entries = collect_entries(handle, cx);
                handle
                    .update(cx, |_, window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                let weak_workspace = workspace.weak_handle();
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    WindowSwitcher::new(entries, weak_workspace, window, cx)
                                });
                            })
                            .log_err();
                    })
                    .log_err();
            });
        });
    })
    .detach();
}

#[derive(Clone)]
struct ProjectEntry {
    window: WindowHandle<MultiWorkspace>,
    key: ProjectGroupKey,
    name: SharedString,
    path: SharedString,
    window_number: usize,
    is_current: bool,
}

impl ProjectEntry {
    fn candidate(&self, index: usize) -> StringMatchCandidate {
        StringMatchCandidate::new(index, format!("{}\n{}", self.name, self.path))
    }

    fn marker(&self) -> Option<SharedString> {
        if self.is_current {
            Some("Current".into())
        } else if self.window_number > 1 {
            Some(format!("Window {}", self.window_number).into())
        } else {
            None
        }
    }
}

fn collect_entries(current_window: WindowHandle<MultiWorkspace>, cx: &App) -> Vec<ProjectEntry> {
    let mut windows = cx.window_stack().unwrap_or_default();
    for window in cx.windows() {
        if !windows.contains(&window) {
            windows.push(window);
        }
    }
    windows.retain(|window| *window != current_window.into());
    windows.insert(0, current_window.into());

    let mut entries = Vec::new();
    let mut window_number = 0;
    for handle in windows {
        let Some(handle) = handle.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Some(multi_workspace) = handle.read(cx).log_err() else {
            continue;
        };
        window_number += 1;
        let active_key = multi_workspace.workspace().read(cx).project_group_key(cx);
        let mut keys = multi_workspace.project_group_keys();
        keys.retain(|key| *key != active_key);
        // A single-project window may not have any retained sidebar groups yet.
        keys.insert(0, active_key.clone());

        let mut paths: Vec<_> = keys
            .iter()
            .flat_map(|key| key.path_list().paths().iter().cloned())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        let details = util::disambiguate::compute_disambiguation_details(&paths, |path, detail| {
            project::path_suffix(path, detail)
        });
        let details: HashMap<_, _> = paths.into_iter().zip(details).collect();

        for key in keys {
            let path = key
                .path_list()
                .ordered_paths()
                .next()
                .map(|path| {
                    if key.host().is_some() {
                        path.to_string_lossy().into_owned()
                    } else {
                        path.compact().to_string_lossy().into_owned()
                    }
                })
                .unwrap_or_default();
            let path = if let Some(host) = key.host() {
                format!(
                    "{}: {} {}",
                    host.connection_type(),
                    host.display_name(),
                    path
                )
            } else {
                path
            };
            entries.push(ProjectEntry {
                window: handle,
                name: key.display_name(&details),
                path: path.into(),
                window_number,
                is_current: handle == current_window && key == active_key,
                key,
            });
        }
    }
    entries
}

struct WindowSwitcher {
    picker: Entity<Picker<WindowSwitcherDelegate>>,
}

impl WindowSwitcher {
    fn new(
        entries: Vec<ProjectEntry>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| {
            Picker::uniform_list(WindowSwitcherDelegate::new(entries, workspace), window, cx)
        });
        cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent))
            .detach();
        Self { picker }
    }
}

impl ModalView for WindowSwitcher {}
impl EventEmitter<DismissEvent> for WindowSwitcher {}

impl Focusable for WindowSwitcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for WindowSwitcher {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("WindowSwitcher")
            .w(rems(42.))
            .child(self.picker.clone())
    }
}

struct WindowSwitcherDelegate {
    entries: Vec<ProjectEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    workspace: WeakEntity<Workspace>,
}

impl WindowSwitcherDelegate {
    fn new(entries: Vec<ProjectEntry>, workspace: WeakEntity<Workspace>) -> Self {
        let mut delegate = Self {
            entries,
            matches: Vec::new(),
            selected_index: 0,
            workspace,
        };
        delegate.filter("");
        delegate
    }

    fn filter(&mut self, query: &str) {
        let candidates: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| entry.candidate(index))
            .collect();
        self.matches = if query.is_empty() {
            candidates
                .into_iter()
                .map(|candidate| StringMatch {
                    candidate_id: candidate.id,
                    score: 0.,
                    positions: Vec::new(),
                    string: candidate.string,
                })
                .collect()
        } else {
            match_strings(&candidates, query, StringMatchOptions::default())
        };
        self.selected_index = usize::from(query.is_empty() && self.matches.len() > 1);
    }
}

impl PickerDelegate for WindowSwitcherDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "WindowSwitcher"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index.min(self.matches.len().saturating_sub(1));
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Switch to project…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.filter(query.trim());
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let entry = self
            .matches
            .get(self.selected_index)
            .and_then(|matched| self.entries.get(matched.candidate_id))
            .cloned();
        cx.emit(DismissEvent);
        if let Some(entry) = entry.filter(|entry| !entry.is_current) {
            let workspace = self.workspace.clone();
            let source_window = window.window_handle();
            cx.defer(move |cx| {
                // The picker dismissal is relayed through the modal, so its event may
                // still be queued. Restore source focus before activating the target.
                source_window
                    .update(cx, |_, window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                if workspace.active_modal::<WindowSwitcher>(cx).is_some() {
                                    workspace.hide_modal(window, cx);
                                }
                            })
                            .log_err();
                    })
                    .log_err();
                switch_to_entry(entry, workspace, cx);
            });
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matched = self.matches.get(index)?;
        let entry = self.entries.get(matched.candidate_id)?;
        let path_offset = entry.name.len() + 1;
        let name_positions = matched
            .positions
            .iter()
            .copied()
            .filter(|position| *position < entry.name.len())
            .collect();
        let path_positions = matched
            .positions
            .iter()
            .filter_map(|position| position.checked_sub(path_offset))
            .collect();
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    HighlightedLabel::new(entry.name.clone(), name_positions)
                                        .truncate_middle(),
                                )
                                .child(
                                    HighlightedLabel::new(entry.path.clone(), path_positions)
                                        .truncate_middle()
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                        )
                        .children(entry.marker().map(|marker| {
                            Label::new(marker)
                                .color(Color::Muted)
                                .size(LabelSize::Small)
                        })),
                ),
        )
    }
}

fn switch_to_entry(entry: ProjectEntry, workspace: WeakEntity<Workspace>, cx: &mut App) {
    let result = entry.window.update(cx, |multi_workspace, window, cx| {
        let key = entry.key;
        let active_key = multi_workspace.workspace().read(cx).project_group_key(cx);
        anyhow::ensure!(
            active_key == key || multi_workspace.group_state_by_key(&key).is_some(),
            "The selected project is no longer open in this window"
        );
        window.activate_window();
        if let Some(workspace) = multi_workspace.last_active_workspace_for_group(&key, cx) {
            multi_workspace.activate(workspace, None, window, cx);
            return Ok(None);
        }
        let modal_workspace = multi_workspace.workspace().clone();
        let task = multi_workspace.find_or_create_workspace(
            key.path_list().clone(),
            key.host().cloned(),
            Some(key),
            move |options, window, cx| connect_with_modal(&modal_workspace, options, window, cx),
            None,
            OpenMode::Activate,
            None,
            window,
            cx,
        );
        Ok(Some(task))
    });
    match result.and_then(|result| result) {
        Ok(Some(task)) => {
            cx.spawn(async move |cx| {
                if let Err(error) = task.await {
                    let workspace = cx
                        .update(|cx| {
                            entry
                                .window
                                .read(cx)
                                .log_err()
                                .map(|window| window.workspace().downgrade())
                        })
                        .unwrap_or(workspace);
                    util::log_err(&error);
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.show_error(format!("Could not switch project: {error:#}"), cx)
                        })
                        .log_err();
                }
            })
            .detach();
        }
        Ok(None) => {}
        Err(error) => {
            util::log_err(&error);
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(format!("Could not switch project: {error:#}"), cx);
                })
                .log_err();
        }
    }
}

#[cfg(test)]
mod tests;
