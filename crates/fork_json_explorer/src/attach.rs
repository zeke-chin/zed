use editor::{Addon, Editor};
use gpui::{App, Context, Entity, KeyContext, Window};
use language::Buffer;

use crate::{
    convert::{
        DocumentKind, ToggleCurrentDocument, document_kind, is_enabled, toggle_current_document,
    },
    paste,
};

pub(crate) const ENABLED_CONTEXT: &str = "json_explorer_enabled";
pub(crate) const CONVERTIBLE_CONTEXT: &str = "json_explorer_convertible";

/// Marks editors for key bindings: `json_explorer_enabled` whenever the
/// feature is on, `json_explorer_convertible` additionally when the document
/// is JSON, JSONC or plain text.
struct JsonExplorerAddon {
    buffer: Entity<Buffer>,
}

impl Addon for JsonExplorerAddon {
    fn extend_key_context(&self, key_context: &mut KeyContext, cx: &App) {
        if !is_enabled(cx) {
            return;
        }
        key_context.add(ENABLED_CONTEXT);
        if document_kind(self.buffer.read(cx)) != DocumentKind::Other {
            key_context.add(CONVERTIBLE_CONTEXT);
        }
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(crate) fn attach(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
        return;
    };
    editor.register_addon(JsonExplorerAddon {
        buffer: buffer.clone(),
    });
    let weak_editor = cx.entity().downgrade();
    editor
        .register_action(move |_: &ToggleCurrentDocument, window, cx| {
            toggle_current_document(&weak_editor, window, cx)
        })
        .detach();
    paste::watch_buffer(&buffer, window, cx);
}
