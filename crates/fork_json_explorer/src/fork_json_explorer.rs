mod attach;
mod convert;
pub mod core;
mod hover;
mod nested;
mod paste;
mod pending_opens;
mod state;
mod status;

#[cfg(test)]
mod hover_tests;
#[cfg(test)]
mod tests;

pub use convert::ToggleCurrentDocument;
pub use nested::{LINK_PREFIX as HOVER_LINK_PREFIX, ParseNestedValue, handle_hover_link};

use editor::Editor;
use gpui::{App, AppContext as _, Context, Window};
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

pub fn init(cx: &mut App) {
    state::ConvertState::global(cx);
}

pub fn attach_editor(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    attach::attach(editor, window, cx);
    hover::attach(editor, cx);
    nested::attach(editor, cx);
}

pub fn status_items(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let status_bar = workspace.status_bar().clone();
    cx.defer_in(window, move |_, window, cx| {
        let hint_item = cx.new(status::ConvertHintItem::new);
        status_bar.update(cx, |status_bar, cx| {
            status_bar.add_left_item(hint_item, window, cx);
        });
    });
}
