pub mod core;

use editor::Editor;
use gpui::{App, Context, Window};
use settings::{RegisterSetting, Settings, SettingsContent};
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

pub fn init(_cx: &mut App) {}

pub fn attach_editor(_editor: &mut Editor, _window: &mut Window, _cx: &mut Context<Editor>) {}

pub fn status_items(
    _workspace: &mut Workspace,
    _window: &mut Window,
    _cx: &mut Context<Workspace>,
) {
}
