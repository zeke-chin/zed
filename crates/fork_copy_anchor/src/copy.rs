use editor::{Editor, EditorEvent};
use gpui::{
    Action as _, App, ClipboardItem, Context, Entity, Focusable as _, WeakEntity, Window, actions,
};
use language::{DiskState, Point};
use project::ProjectPath;
use settings::{Settings as _, SettingsStore};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    CopyAnchorSettings,
    core::{SelectedText, format_context},
    state::{CopyAnchorModel, CopyContent},
};

actions!(
    copy_anchor,
    [
        /// Copies the selection with its file path and line numbers.
        Copy,
        /// Switches between copying with and without file path and line numbers.
        ToggleContextMode
    ]
);

pub(crate) const ENABLED_CONTEXT: &str = "copy_anchor_enabled";
pub(crate) const HAS_SELECTION_CONTEXT: &str = "copy_anchor_has_selection";

pub(crate) struct CopyAnchorAddon {
    has_selection: bool,
}

impl editor::Addon for CopyAnchorAddon {
    fn extend_key_context(&self, key_context: &mut gpui::KeyContext, cx: &App) {
        if !CopyAnchorSettings::get_global(cx).enabled {
            return;
        }
        key_context.add(ENABLED_CONTEXT);
        if self.has_selection {
            key_context.add(HAS_SELECTION_CONTEXT);
        }
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

pub(crate) fn attach(editor: &mut Editor, cx: &mut Context<Editor>) {
    let has_selection = has_non_empty_selections(editor, cx);
    editor.register_addon(CopyAnchorAddon { has_selection });

    cx.subscribe_self(|editor, event: &EditorEvent, cx| {
        if matches!(event, EditorEvent::SelectionsChanged { .. }) {
            let has_selection = has_non_empty_selections(editor, cx);
            if let Some(addon) = editor.addon_mut::<CopyAnchorAddon>() {
                addon.has_selection = has_selection;
            }
        }
    })
    .detach();

    let weak_editor = cx.entity().downgrade();
    editor
        .register_action(move |_: &Copy, window, cx| copy(&weak_editor, window, cx))
        .detach();
}

fn has_non_empty_selections(editor: &Editor, cx: &mut App) -> bool {
    let snapshot = editor.display_snapshot(cx);
    let selections = editor.selections.all::<Point>(&snapshot);
    !selections.is_empty() && selections.iter().all(|selection| !selection.is_empty())
}

struct CapturedSelection {
    plain: String,
    context: Option<String>,
}

impl CapturedSelection {
    fn into_content(self, plain_item: Option<ClipboardItem>) -> CopyContent {
        CopyContent {
            context: self.context.unwrap_or_else(|| self.plain.clone()),
            plain: self.plain,
            plain_item,
        }
    }
}

fn capture_selection(editor: &Editor, cx: &mut App) -> Option<CapturedSelection> {
    let snapshot = editor.display_snapshot(cx);
    let selections = editor.selections.all::<Point>(&snapshot);
    if selections.is_empty() || selections.iter().any(|selection| selection.is_empty()) {
        return None;
    }
    let buffer_snapshot = editor.buffer().read(cx).snapshot(cx);
    let selected_texts: Vec<SelectedText> = selections
        .iter()
        .map(|selection| {
            let text: String = buffer_snapshot
                .text_for_range(selection.start..selection.end)
                .collect();
            SelectedText::new(text, selection.start.row as usize)
        })
        .collect();
    let plain = selected_texts
        .iter()
        .map(|selected| selected.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let context = absolute_path(editor, cx).map(|path| {
        let selects_whole_file = selections.len() == 1
            && selections.first().is_some_and(|selection| {
                selection.start == Point::zero() && selection.end == buffer_snapshot.max_point()
            });
        if selects_whole_file {
            path
        } else {
            format_context(&path, &selected_texts)
        }
    });
    Some(CapturedSelection { plain, context })
}

/// The absolute path of the editor's file, or `None` when the editor shows
/// several buffers, an unsaved new file or a buffer without a disk path.
fn absolute_path(editor: &Editor, cx: &App) -> Option<String> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let buffer = buffer.read(cx);
    let file = buffer.file()?;
    if !matches!(
        file.disk_state(),
        DiskState::Present { .. } | DiskState::Deleted
    ) {
        return None;
    }
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    let path = editor
        .project()
        .and_then(|project| project.read(cx).absolute_path(&project_path, cx))
        .or_else(|| file.as_local().map(|local| local.abs_path(cx)))?;
    let path = path.to_str()?.to_string();
    // A line break in the path cannot be represented in the one-line header.
    if path.contains(['\n', '\r']) {
        return None;
    }
    Some(path)
}

fn native_copy(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    editor
        .read(cx)
        .focus_handle(cx)
        .dispatch_action(&editor::actions::Copy, window, cx);
}

fn copy(editor: &WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    if !CopyAnchorSettings::get_global(cx).enabled {
        native_copy(&editor, window, cx);
        return;
    }
    let Some(model) = CopyAnchorModel::try_global(cx) else {
        native_copy(&editor, window, cx);
        return;
    };
    let context_mode = model.read(cx).context_mode(cx);
    let captured = editor.update(cx, |editor, cx| capture_selection(editor, cx));

    if context_mode
        && let Some(captured) = &captured
        && let Some(context) = captured.context.clone()
    {
        cx.write_to_clipboard(ClipboardItem::new_string(context.clone()));
        let content = CapturedSelection {
            plain: captured.plain.clone(),
            context: Some(context.clone()),
        }
        .into_content(None);
        model.update(cx, |model, cx| {
            model.record_copy(Some(content), context, true, cx);
        });
        return;
    }

    native_copy(&editor, window, cx);
    let item = cx.read_from_clipboard();
    let Some(written) = item.as_ref().and_then(|item| item.text()) else {
        return;
    };
    let content = captured.map(|captured| captured.into_content(item));
    model.update(cx, |model, cx| {
        model.record_copy(content, written, false, cx)
    });
}

pub(crate) fn toggle_context_mode(
    workspace: &mut Workspace,
    _: &ToggleContextMode,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !CopyAnchorSettings::get_global(cx).enabled {
        return;
    }
    let Some(model) = CopyAnchorModel::try_global(cx) else {
        return;
    };
    let captured = workspace
        .active_item_as::<Editor>(cx)
        .and_then(|editor| editor.update(cx, |editor, cx| capture_selection(editor, cx)));

    let new_context_mode = !model.read(cx).context_mode(cx);
    model.update(cx, |model, cx| {
        model.set_pending_context_mode(Some(new_context_mode), cx);
    });
    write_context_mode_setting(new_context_mode, &model, cx);

    let remembered = model.read(cx).content_still_on_clipboard(cx);
    let content = remembered.or_else(|| captured.map(|captured| captured.into_content(None)));
    let Some(content) = content else {
        return;
    };
    let item = content.clipboard_item_for_mode(new_context_mode);
    let written = content.text_for_mode(new_context_mode).to_string();
    let is_context = content.is_context(new_context_mode);
    cx.write_to_clipboard(item);
    model.update(cx, |model, cx| {
        model.record_copy(Some(content), written, is_context, cx);
    });
}

pub(crate) fn toggle_from_status_bar(window: &mut Window, cx: &mut App) {
    window.dispatch_action(ToggleContextMode.boxed_clone(), cx);
}

fn write_context_mode_setting(context_mode: bool, model: &Entity<CopyAnchorModel>, cx: &mut App) {
    let fs = <dyn fs::Fs>::global(cx);
    let completion = settings::update_settings_file_with_completion(fs, cx, move |content, _| {
        content
            .fork
            .get_or_insert_default()
            .copy_anchor
            .get_or_insert_default()
            .context_mode = Some(context_mode);
    });
    let model = model.downgrade();
    cx.spawn(async move |cx| {
        let failure = match completion.await {
            Ok(Ok(())) => return,
            Ok(Err(error)) => format!("{error:#}"),
            Err(_) => "the settings update was dropped".to_string(),
        };
        log::error!("copy_anchor: failed to save context_mode: {failure}");
        model
            .update(cx, |model, cx| model.set_pending_context_mode(None, cx))
            .log_err();
    })
    .detach();
}

pub(crate) fn observe_settings(model: &Entity<CopyAnchorModel>, cx: &mut App) {
    let model = model.clone();
    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = CopyAnchorSettings::get_global(cx).clone();
        model.update(cx, |model, cx| model.apply_settings(settings, cx));
    })
    .detach();
}
