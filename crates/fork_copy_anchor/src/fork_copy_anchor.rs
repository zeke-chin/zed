mod copy;
pub mod core;
mod state;
mod status;

#[cfg(test)]
mod tests;

use editor::Editor;
use gpui::{App, AppContext as _, Context, Window};
use settings::{RegisterSetting, Settings, SettingsContent};
use state::CopyAnchorModel;
use workspace::Workspace;

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct CopyAnchorSettings {
    pub enabled: bool,
    pub context_mode: bool,
    pub preview_max_length: usize,
    pub preview_duration_seconds: f32,
}

impl Settings for CopyAnchorSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let copy_anchor = content
            .fork
            .as_ref()
            .and_then(|fork| fork.copy_anchor.as_ref());
        Self {
            enabled: copy_anchor
                .and_then(|copy_anchor| copy_anchor.enabled)
                .unwrap_or(true),
            context_mode: copy_anchor
                .and_then(|copy_anchor| copy_anchor.context_mode)
                .unwrap_or(true),
            preview_max_length: copy_anchor
                .and_then(|copy_anchor| copy_anchor.preview_max_length)
                .unwrap_or(30),
            preview_duration_seconds: copy_anchor
                .and_then(|copy_anchor| copy_anchor.preview_duration)
                .unwrap_or(1.0),
        }
    }
}

pub use copy::{Copy, ToggleContextMode};

pub fn init(cx: &mut App) {
    let model = CopyAnchorModel::install(cx);
    copy::observe_settings(&model, cx);
}

pub fn attach_editor(editor: &mut Editor, _window: &mut Window, cx: &mut Context<Editor>) {
    copy::attach(editor, cx);
}

pub fn status_items(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.register_action(copy::toggle_context_mode);
    let status_bar = workspace.status_bar().clone();
    cx.defer_in(window, move |_, window, cx| {
        let preview_item = cx.new(status::CopyPreviewItem::new);
        let mode_item = cx.new(status::ModeItem::new);
        status_bar.update(cx, |status_bar, cx| {
            status_bar.add_left_item(preview_item, window, cx);
            status_bar.add_right_item(mode_item, window, cx);
        });
    });
}
