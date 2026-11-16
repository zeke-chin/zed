use editor::Editor;
use gpui::{App, Context, Window};
use settings::{RegisterSetting, Settings, SettingsContent};
use workspace::Workspace;

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct JsonExplorerSettings {
    pub enabled: bool,
    pub python_repr_auto_convert: bool,
    pub line_break_repair_auto_convert: bool,
}

impl Settings for JsonExplorerSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let json_explorer = content
            .fork
            .as_ref()
            .and_then(|fork| fork.json_explorer.as_ref());
        Self {
            enabled: json_explorer
                .and_then(|json_explorer| json_explorer.enabled)
                .unwrap_or(true),
            python_repr_auto_convert: json_explorer
                .and_then(|json_explorer| json_explorer.python_repr_auto_convert)
                .unwrap_or(false),
            line_break_repair_auto_convert: json_explorer
                .and_then(|json_explorer| json_explorer.line_break_repair_auto_convert)
                .unwrap_or(false),
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
