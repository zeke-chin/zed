use std::time::Duration;

use gpui::{App, AppContext as _, ClipboardItem, Context, Entity, Global, Task};
use settings::Settings as _;
use util::ResultExt as _;

use crate::{CopyAnchorSettings, core::format_copy_preview};

/// The two forms of the most recently copied text, kept in memory so a mode
/// switch can swap the clipboard between them.
#[derive(Clone, Debug)]
pub(crate) struct CopyContent {
    pub plain: String,
    pub plain_item: Option<ClipboardItem>,
    pub context: String,
}

impl CopyContent {
    pub fn text_for_mode(&self, context_mode: bool) -> &str {
        if context_mode {
            &self.context
        } else {
            &self.plain
        }
    }

    pub fn clipboard_item_for_mode(&self, context_mode: bool) -> ClipboardItem {
        match (context_mode, &self.plain_item) {
            (false, Some(item)) => item.clone(),
            _ => ClipboardItem::new_string(self.text_for_mode(context_mode).to_string()),
        }
    }

    pub fn is_context(&self, context_mode: bool) -> bool {
        context_mode && self.context != self.plain
    }
}

struct LastCopy {
    content: CopyContent,
    written: String,
}

#[derive(Default)]
pub(crate) struct CopyAnchorModel {
    last_copy: Option<LastCopy>,
    preview: Option<String>,
    preview_task: Option<Task<()>>,
    pending_context_mode: Option<bool>,
    applied_settings: Option<CopyAnchorSettings>,
}

struct GlobalCopyAnchorModel(Entity<CopyAnchorModel>);

impl Global for GlobalCopyAnchorModel {}

impl CopyAnchorModel {
    pub fn install(cx: &mut App) -> Entity<Self> {
        let model = cx.new(|_| Self::default());
        cx.set_global(GlobalCopyAnchorModel(model.clone()));
        model
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalCopyAnchorModel>()
            .map(|global| global.0.clone())
    }

    pub fn preview(&self) -> Option<&str> {
        self.preview.as_deref()
    }

    /// The mode a copy happening right now should use. A toggle that is still
    /// being written to the settings file takes precedence over the stored value.
    pub fn context_mode(&self, cx: &App) -> bool {
        self.pending_context_mode
            .unwrap_or_else(|| CopyAnchorSettings::get_global(cx).context_mode)
    }

    pub fn set_pending_context_mode(&mut self, context_mode: Option<bool>, cx: &mut Context<Self>) {
        self.pending_context_mode = context_mode;
        cx.notify();
    }

    /// Returns the content to re-copy when the clipboard still holds what this
    /// feature wrote last.
    pub fn content_still_on_clipboard(&self, cx: &App) -> Option<CopyContent> {
        let last_copy = self.last_copy.as_ref()?;
        let clipboard_text = cx.read_from_clipboard()?.text()?;
        (clipboard_text == last_copy.written).then(|| last_copy.content.clone())
    }

    pub fn record_copy(
        &mut self,
        content: Option<CopyContent>,
        written: String,
        is_context: bool,
        cx: &mut Context<Self>,
    ) {
        self.last_copy = content.map(|content| LastCopy {
            content,
            written: written.clone(),
        });
        self.show_preview(&written, is_context, cx);
    }

    fn show_preview(&mut self, written: &str, is_context: bool, cx: &mut Context<Self>) {
        self.preview = None;
        self.preview_task = None;
        let settings = CopyAnchorSettings::get_global(cx);
        let duration_seconds = settings.preview_duration_seconds.clamp(0.0, 3600.0);
        if duration_seconds.is_nan() || duration_seconds <= 0.0 {
            cx.notify();
            return;
        }
        let max_length = settings.preview_max_length.clamp(1, 1000);
        self.preview = Some(format_copy_preview(written, is_context, max_length));
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs_f32(duration_seconds))
                .await;
            this.update(cx, |this, cx| {
                this.preview = None;
                this.preview_task = None;
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    pub fn apply_settings(&mut self, settings: CopyAnchorSettings, cx: &mut Context<Self>) {
        let previous = self.applied_settings.replace(settings.clone());
        if self.pending_context_mode == Some(settings.context_mode) {
            self.pending_context_mode = None;
        }
        let preview_settings_changed = previous.as_ref().is_some_and(|previous| {
            previous.preview_max_length != settings.preview_max_length
                || previous.preview_duration_seconds != settings.preview_duration_seconds
        });
        if !settings.enabled {
            self.last_copy = None;
            self.pending_context_mode = None;
        }
        if !settings.enabled || preview_settings_changed {
            self.preview = None;
            self.preview_task = None;
        }
        cx.notify();
    }
}
