use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Window, div,
};
use settings::Settings as _;
use ui::{Color, IconButton, IconName, IconSize, Label, LabelSize, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle};

use crate::{
    CopyAnchorSettings,
    copy::{ToggleContextMode, toggle_from_status_bar},
    state::CopyAnchorModel,
};

pub(crate) struct CopyPreviewItem {
    model: Option<Entity<CopyAnchorModel>>,
}

impl CopyPreviewItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let model = CopyAnchorModel::try_global(cx);
        if let Some(model) = &model {
            cx.observe(model, |_, _, cx| cx.notify()).detach();
        }
        cx.observe_global::<settings::SettingsStore>(|_, cx| cx.notify())
            .detach();
        Self { model }
    }
}

impl Render for CopyPreviewItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = if CopyAnchorSettings::get_global(cx).enabled {
            self.model
                .as_ref()
                .and_then(|model| model.read(cx).preview().map(str::to_string))
        } else {
            None
        };
        match preview {
            Some(preview) => div().child(
                Label::new(format!("copied: {preview}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .single_line(),
            ),
            None => div().hidden(),
        }
    }
}

impl StatusItemView for CopyPreviewItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

pub(crate) struct ModeItem {
    model: Option<Entity<CopyAnchorModel>>,
}

impl ModeItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let model = CopyAnchorModel::try_global(cx);
        if let Some(model) = &model {
            cx.observe(model, |_, _, cx| cx.notify()).detach();
        }
        cx.observe_global::<settings::SettingsStore>(|_, cx| cx.notify())
            .detach();
        Self { model }
    }
}

impl Render for ModeItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !CopyAnchorSettings::get_global(cx).enabled {
            return div().hidden();
        }
        let context_mode = self
            .model
            .as_ref()
            .map(|model| model.read(cx).context_mode(cx))
            .unwrap_or_else(|| CopyAnchorSettings::get_global(cx).context_mode);
        let (icon, title) = if context_mode {
            (
                IconName::Pin,
                "Copy Anchor: copying with file path and line numbers",
            )
        } else {
            (IconName::Copy, "Copy Anchor: plain copy")
        };
        div().child(
            IconButton::new("copy-anchor-mode", icon)
                .icon_size(IconSize::Small)
                .tooltip(move |_window, cx| Tooltip::for_action(title, &ToggleContextMode, cx))
                .on_click(|_, window, cx| toggle_from_status_bar(window, cx)),
        )
    }
}

impl StatusItemView for ModeItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
