//! The status bar item, its menu, and the `windsurf: show last request` view.

use std::time::{Duration, UNIX_EPOCH};

use editor::{Editor, MultiBuffer};
use gpui::{
    Action as _, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    Window, div,
};
use language::Capability;
use settings::{Settings as _, SettingsStore};
use ui::{Button, ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use crate::{ErrorStatus, ShowLastRequest, Status, Windsurf, WindsurfSettings};

const IMPORT_API_KEY_ACTION: &str = "windsurf::ImportApiKey";
const OPEN_API_KEY_SETTING_ACTION: &str = "windsurf::OpenApiKeySetting";

pub(crate) struct WindsurfStatusItem {
    windsurf: Option<Entity<Windsurf>>,
}

impl WindsurfStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let windsurf = Windsurf::global(cx);
        if let Some(windsurf) = &windsurf {
            cx.observe(windsurf, |_, _, cx| cx.notify()).detach();
        }
        cx.observe_global::<SettingsStore>(|_, cx| cx.notify())
            .detach();
        Self { windsurf }
    }
}

fn format_retry(retry_in: Duration) -> String {
    if retry_in.is_zero() {
        String::new()
    } else {
        format!(", retrying in {}s", retry_in.as_secs().max(1))
    }
}

/// The short label and the tooltip for a status.
pub(crate) fn describe(status: &Status) -> (&'static str, String, Color) {
    match status {
        Status::Disabled => (
            "Windsurf: off",
            "Windsurf edit predictions are turned off".to_string(),
            Color::Muted,
        ),
        Status::MissingApiKey => (
            "Windsurf: no API key",
            "Import the API key from this machine or enter it in settings".to_string(),
            Color::Warning,
        ),
        Status::Ready => ("Windsurf", "Windsurf is ready".to_string(), Color::Muted),
        Status::Requesting => (
            "Windsurf…",
            "Requesting a prediction".to_string(),
            Color::Muted,
        ),
        Status::Error(ErrorStatus::Unauthenticated(message)) => (
            "Windsurf: key rejected",
            format!("The API key was rejected ({message}). Import it again or enter a new one."),
            Color::Error,
        ),
        Status::Error(ErrorStatus::RateLimited { message, retry_in }) => (
            "Windsurf: rate limited",
            format!("Rate limited ({message}){}", format_retry(*retry_in)),
            Color::Warning,
        ),
        Status::Error(ErrorStatus::Network { message, retry_in }) => (
            "Windsurf: offline",
            format!("Network error ({message}){}", format_retry(*retry_in)),
            Color::Warning,
        ),
        Status::Error(ErrorStatus::Protocol(message)) => (
            "Windsurf: protocol error",
            format!("Couldn't read the response: {message}"),
            Color::Error,
        ),
        Status::Error(ErrorStatus::Server(message)) => (
            "Windsurf: server error",
            format!("The server returned an error: {message}"),
            Color::Error,
        ),
    }
}

impl Render for WindsurfStatusItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(windsurf) = self.windsurf.clone() else {
            return div().hidden();
        };
        if !windsurf.read(cx).selected {
            return div().hidden();
        }
        let status = windsurf.read(cx).status(cx);
        let (label, tooltip, color) = describe(&status);
        let enabled = WindsurfSettings::get_global(cx).enabled;
        div().child(
            PopoverMenu::new("windsurf-status-menu")
                .menu(move |window, cx| Some(build_menu(enabled, window, cx)))
                .anchor(gpui::Anchor::BottomRight)
                .trigger_with_tooltip(
                    Button::new("windsurf-status", label)
                        .label_size(LabelSize::Small)
                        .color(color),
                    Tooltip::text(tooltip),
                ),
        )
    }
}

fn build_menu(enabled: bool, window: &mut Window, cx: &mut App) -> Entity<ContextMenu> {
    // Defined by the API key import feature; the entries are left out until it exists.
    let import_action = cx.build_action(IMPORT_API_KEY_ACTION, None).ok();
    let open_setting_action = cx.build_action(OPEN_API_KEY_SETTING_ACTION, None).ok();
    ContextMenu::build(window, cx, move |menu, _, _| {
        menu.toggleable_entry(
            "Windsurf Edit Predictions",
            enabled,
            IconPosition::Start,
            Some(crate::ToggleEnabled.boxed_clone()),
            |window, cx| window.dispatch_action(crate::ToggleEnabled.boxed_clone(), cx),
        )
        .separator()
        .when_some(import_action, |menu, action| {
            menu.entry(
                "Import API Key from This Machine",
                Some(action.boxed_clone()),
                move |window, cx| window.dispatch_action(action.boxed_clone(), cx),
            )
        })
        .when_some(open_setting_action, |menu, action| {
            menu.entry(
                "Enter API Key in Settings",
                Some(action.boxed_clone()),
                move |window, cx| window.dispatch_action(action.boxed_clone(), cx),
            )
        })
        .separator()
        .entry(
            "Show Last Request",
            Some(ShowLastRequest.boxed_clone()),
            |window, cx| window.dispatch_action(ShowLastRequest.boxed_clone(), cx),
        )
        .entry(
            "Open Log",
            Some(workspace::OpenLog.boxed_clone()),
            |window, cx| window.dispatch_action(workspace::OpenLog.boxed_clone(), cx),
        )
    })
}

impl StatusItemView for WindsurfStatusItem {
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

pub(crate) fn toggle_enabled(cx: &mut App) {
    let enabled = WindsurfSettings::get_global(cx).enabled;
    let fs = <dyn fs::Fs>::global(cx);
    settings::update_settings_file(fs, cx, move |content, _| {
        content
            .fork
            .get_or_insert_default()
            .windsurf
            .get_or_insert_default()
            .enabled = Some(!enabled);
    });
}

pub(crate) fn last_request_text(cx: &App) -> String {
    let Some(windsurf) = Windsurf::global(cx) else {
        return "Windsurf isn't initialized.".to_string();
    };
    let windsurf = windsurf.read(cx);
    let Some(last_request) = windsurf.last_request() else {
        return "No request has been sent yet. Requests are only sent after an edit.".to_string();
    };
    let sent_at = last_request
        .sent_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let summary = serde_json::json!({
        "sent_at_unix_ms": sent_at,
        "trajectory_steps": windsurf.trajectory().steps().len(),
        "response": last_request.response,
        "request": last_request.request,
    });
    serde_json::to_string_pretty(&summary)
        .unwrap_or_else(|error| format!("couldn't format the request: {error}"))
}

pub(crate) fn show_last_request(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let text = last_request_text(cx);
    let project = workspace.project().clone();
    let language = workspace.app_state().languages.language_for_name("JSON");
    cx.spawn_in(window, async move |workspace, cx| {
        let language = language.await.log_err();
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(language, false, cx))
            .await?;
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        workspace.update_in(cx, |workspace, window, cx| {
            let title = "Windsurf Last Request";
            let multi_buffer =
                cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.into()));
            let editor = cx.new(|cx| {
                let mut editor =
                    Editor::for_multibuffer(multi_buffer, Some(project.clone()), window, cx);
                editor.set_read_only(true);
                editor.set_should_serialize(false, cx);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        })
    })
    .detach_and_log_err(cx);
}
