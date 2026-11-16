use editor::Editor;
use gpui::{App, Context, Entity, IntoElement, Render, Subscription, WeakEntity, Window};
use language::Buffer;
use ui::{Button, ButtonCommon as _, Clickable as _, LabelSize, Tooltip};
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle};

use crate::{
    convert::{DocumentKind, document_kind, is_enabled, toggle},
    state::{ConvertHintKind, ConvertState},
};

/// Shows a clickable "convert" item while the active plain text document holds
/// content found convertible at paste time.
pub(crate) struct ConvertHintItem {
    state: Entity<ConvertState>,
    editor: Option<WeakEntity<Editor>>,
    buffer: Option<Entity<Buffer>>,
    _buffer_subscription: Option<Subscription>,
}

impl ConvertHintItem {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let state = ConvertState::global(cx);
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        cx.observe_global::<settings::SettingsStore>(|_, cx| cx.notify())
            .detach();
        Self {
            state,
            editor: None,
            buffer: None,
            _buffer_subscription: None,
        }
    }

    fn current_hint(&self, cx: &App) -> Option<ConvertHintKind> {
        if !is_enabled(cx) {
            return None;
        }
        let buffer = self.buffer.as_ref()?.read(cx);
        if document_kind(buffer) != DocumentKind::PlainText {
            return None;
        }
        self.state.read(cx).hint_for(buffer)
    }
}

impl Render for ConvertHintItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(kind) = self.current_hint(cx) else {
            return gpui::Empty.into_any_element();
        };
        let editor = self.editor.clone();
        Button::new("json-explorer-convert", kind.label())
            .label_size(LabelSize::Small)
            .tooltip(Tooltip::text("Convert the document to JSON (cmd-;)"))
            .on_click(move |_, window, cx| {
                if let Some(editor) = editor.as_ref().and_then(|editor| editor.upgrade()) {
                    editor.update(cx, |editor, cx| toggle(editor, window, cx));
                }
            })
            .into_any_element()
    }
}

impl StatusItemView for ConvertHintItem {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = active_pane_item.and_then(|item| item.act_as::<Editor>(cx));
        self.buffer = editor
            .as_ref()
            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton());
        self._buffer_subscription = self
            .buffer
            .as_ref()
            .map(|buffer| cx.observe(buffer, |_, _, cx| cx.notify()));
        self.editor = editor.map(|editor| editor.downgrade());
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
