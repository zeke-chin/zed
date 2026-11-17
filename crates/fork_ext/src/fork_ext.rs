//! Entry point of the personal fork's features. Upstream code only calls into
//! this crate from a handful of hook lines marked `// FORK:` (see `fork/HOOKS.md`).

use std::sync::Arc;

use editor::{EditPredictionRequestTrigger, Editor, hover_popover::ForkHoverLinkHandler};
use gpui::{AnyWindowHandle, App, Context, Global, WeakEntity, Window};
use settings::{
    ForkEditPredictionProvider, KeybindSource, KeymapFile, KeymapFileLoadResult, RegisterSetting,
    Settings, SettingsContent, SettingsStore,
};
use workspace::{AppState, Workspace};

const DEFAULT_KEYMAP: &str = include_str!("../../../fork/keymaps/default-macos.json");

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct ForkSettings {
    pub edit_prediction_provider: ForkEditPredictionProvider,
}

impl Settings for ForkSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        Self {
            edit_prediction_provider: content
                .fork
                .as_ref()
                .and_then(|fork| fork.edit_prediction_provider)
                .unwrap_or_default(),
        }
    }
}

/// Full-mode editors the fork's features are attached to. The fork reassigns
/// edit prediction providers to these itself when its own settings change, so
/// it doesn't depend on the official registry's editor list.
#[derive(Default)]
struct ForkEditors(Vec<(WeakEntity<Editor>, AnyWindowHandle)>);

impl Global for ForkEditors {}

pub fn init(_app_state: Arc<AppState>, cx: &mut App) {
    fork_copy_anchor::init(cx);
    fork_json_explorer::init(cx);
    fork_windsurf::init(cx);
    fork_window_switcher::init(cx);
    fork_search_panel::init(cx);
    observe_edit_prediction_settings(cx);
    cx.set_global(ForkHoverLinkHandler(handle_hover_link));

    cx.observe_new(|editor: &mut Editor, window, cx: &mut Context<Editor>| {
        if !editor.mode().is_full() {
            return;
        }
        let Some(window) = window else {
            return;
        };

        log::info!(
            "fork_ext: attaching features to editor {:?}",
            cx.entity_id()
        );
        remember_editor(cx.entity().downgrade(), window.window_handle(), cx);

        fork_copy_anchor::attach_editor(editor, window, cx);
        fork_json_explorer::attach_editor(editor, window, cx);
    })
    .detach();

    cx.observe_new(
        |workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
            let Some(window) = window else {
                return;
            };
            fork_search_panel::attach_workspace(workspace, window, cx);
            fork_copy_anchor::status_items(workspace, window, cx);
            fork_json_explorer::status_items(workspace, window, cx);
            fork_windsurf::status_items(workspace, window, cx);
        },
    )
    .detach();
}

/// Registered as the hover popover's handler for `zed-fork://` links (hook H7).
/// Routes each link to the feature named by its host. Returning `false` lets
/// the popover handle the link as usual.
fn handle_hover_link(link: &str, window: &mut Window, cx: &mut App) -> bool {
    if let Some(path) = link.strip_prefix(fork_json_explorer::HOVER_LINK_PREFIX) {
        return fork_json_explorer::handle_hover_link(path, window, cx);
    }
    false
}

fn remember_editor(editor: WeakEntity<Editor>, window: AnyWindowHandle, cx: &mut App) {
    let editors = &mut cx.default_global::<ForkEditors>().0;
    editors.retain(|(editor, _)| editor.upgrade().is_some());
    editors.push((editor, window));
}

fn fork_provides_edit_predictions(cx: &App) -> bool {
    ForkSettings::get_global(cx).edit_prediction_provider == ForkEditPredictionProvider::Windsurf
        && fork_windsurf::should_provide_predictions(cx)
}

/// Called at the start of the official registry's provider assignment.
/// Returns `true` when the fork installed its own provider, in which case the
/// official assignment is skipped.
pub fn assign_edit_prediction_provider(
    editor: &mut Editor,
    trigger: EditPredictionRequestTrigger,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    fork_provides_edit_predictions(cx)
        && fork_windsurf::assign_delegate(editor, trigger, window, cx)
}

/// The official registry only reassigns providers when the official setting
/// changes, so the fork does the same for its own settings: editors get the
/// Windsurf delegate when it becomes active and lose it when it stops being
/// active. The official provider is only reinstalled the next time the
/// official registry assigns one (a new editor, or a change to
/// `edit_predictions.provider`).
fn observe_edit_prediction_settings(cx: &mut App) {
    let windsurf_selected = |cx: &App| {
        ForkSettings::get_global(cx).edit_prediction_provider
            == ForkEditPredictionProvider::Windsurf
    };
    fork_windsurf::set_selected(windsurf_selected(cx), cx);
    let mut provided = fork_provides_edit_predictions(cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        fork_windsurf::set_selected(windsurf_selected(cx), cx);
        let provides = fork_provides_edit_predictions(cx);
        if provides == provided {
            return;
        }
        provided = provides;
        reassign_edit_prediction_providers(provides, cx);
    })
    .detach();
}

fn reassign_edit_prediction_providers(provides: bool, cx: &mut App) {
    let editors = cx
        .try_global::<ForkEditors>()
        .map(|editors| editors.0.clone())
        .unwrap_or_default();
    let trigger = EditPredictionRequestTrigger::SettingsChanged;
    for (editor, window) in editors {
        let Some(editor) = editor.upgrade() else {
            continue;
        };
        let result = window.update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                if !provides {
                    fork_windsurf::remove_delegate(editor, trigger, window, cx);
                } else if !fork_windsurf::has_delegate(editor) {
                    fork_windsurf::assign_delegate(editor, trigger, window, cx);
                }
            })
        });
        if let Err(error) = result {
            log::debug!("fork_ext: skipping an editor whose window is gone: {error:#}");
        }
    }
}

/// Called from Zed's `load_default_keymap`, which runs after every
/// `clear_key_bindings`, so the fork's bindings survive keymap reloads. Bindings
/// added here come after Zed's defaults and before the user's keymap, so they
/// override the defaults and user bindings override them.
pub fn load_default_keymap(cx: &mut App) {
    let mut key_bindings = match KeymapFile::load(DEFAULT_KEYMAP, cx) {
        KeymapFileLoadResult::Success { key_bindings } => key_bindings,
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } => {
            log::error!("fork_ext: some fork default key bindings failed to load: {error_message}");
            key_bindings
        }
        KeymapFileLoadResult::JsonParseFailure { error } => {
            log::error!("fork_ext: failed to parse fork default keymap: {error:#}");
            return;
        }
    };
    for key_binding in &mut key_bindings {
        key_binding.set_meta(KeybindSource::Default.meta());
    }
    cx.bind_keys(key_bindings);
}

#[cfg(test)]
mod tests {
    use editor::{EditPredictionRequestTrigger, Editor};
    use fork_copy_anchor::CopyAnchorSettings;
    use fork_windsurf::WindsurfSettings;
    use gpui::{App, AppContext as _, TestAppContext, UpdateGlobal as _};
    use language::Buffer;
    use settings::{
        ForkEditPredictionProvider, KeymapFile, KeymapFileLoadResult, Settings, SettingsStore,
        WindsurfBackend,
    };

    use super::{
        ForkSettings, assign_edit_prediction_provider, handle_hover_link,
        observe_edit_prediction_settings, remember_editor,
    };

    fn init_settings(cx: &mut App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        ForkSettings::register(cx);
        CopyAnchorSettings::register(cx);
        WindsurfSettings::register(cx);
    }

    #[gpui::test]
    fn test_defaults_when_fork_key_is_missing(cx: &mut App) {
        init_settings(cx);

        assert_eq!(
            ForkSettings::get_global(cx).edit_prediction_provider,
            ForkEditPredictionProvider::Windsurf
        );
        let copy_anchor = CopyAnchorSettings::get_global(cx);
        assert!(copy_anchor.enabled);
        assert!(copy_anchor.context_mode);
        assert_eq!(copy_anchor.preview_max_length, 30);
        let windsurf = WindsurfSettings::get_global(cx);
        assert_eq!(windsurf.backend, WindsurfBackend::Native);
        assert_eq!(windsurf.api_key, None);
    }

    #[gpui::test]
    fn test_user_settings_override_defaults(cx: &mut App) {
        init_settings(cx);

        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{
                        "fork": {
                            "edit_prediction_provider": "none",
                            "copy_anchor": { "enabled": false },
                            "windsurf": { "backend": "language_server", "api_key": "secret" }
                        }
                    }"#,
                    cx,
                )
                .expect("user settings should load");
        });

        assert_eq!(
            ForkSettings::get_global(cx).edit_prediction_provider,
            ForkEditPredictionProvider::None
        );
        let copy_anchor = CopyAnchorSettings::get_global(cx);
        assert!(!copy_anchor.enabled);
        assert!(copy_anchor.context_mode);
        let windsurf = WindsurfSettings::get_global(cx);
        assert_eq!(windsurf.backend, WindsurfBackend::LanguageServer);
        assert_eq!(windsurf.api_key.as_deref(), Some("secret"));
        assert!(!format!("{windsurf:?}").contains("secret"));
    }

    #[gpui::test]
    fn test_default_keymap_loads_without_errors(cx: &mut App) {
        init_settings(cx);

        match KeymapFile::load(super::DEFAULT_KEYMAP, cx) {
            KeymapFileLoadResult::Success { key_bindings } => assert!(!key_bindings.is_empty()),
            KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
                panic!("fork default keymap has invalid bindings: {error_message}")
            }
            KeymapFileLoadResult::JsonParseFailure { error } => {
                panic!("fork default keymap is not valid JSON: {error:#}")
            }
        }
    }

    #[gpui::test]
    fn test_wrong_field_type_is_reported(cx: &mut App) {
        init_settings(cx);

        let result = SettingsStore::update_global(cx, |store, cx| {
            store.set_user_settings(r#"{ "fork": { "copy_anchor": { "enabled": "yes" } } }"#, cx)
        });

        assert!(result.result().is_err());
        assert!(CopyAnchorSettings::get_global(cx).enabled);
    }

    fn set_user_settings(settings: &str, cx: &mut TestAppContext) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(settings, cx)
                    .expect("user settings should load");
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn test_windsurf_delegate_follows_fork_settings(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            init_settings(cx);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
            fork_windsurf::init(cx);
            observe_edit_prediction_settings(cx);
        });
        let window = cx.add_window(|window, cx| {
            let buffer = cx.new(|cx| Buffer::local("fn main() {}\n", cx));
            Editor::for_buffer(buffer, None, window, cx)
        });
        let has_delegate = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |editor, _| fork_windsurf::has_delegate(editor))
                .expect("window should be open")
        };
        window
            .update(cx, |editor, window, cx| {
                remember_editor(cx.entity().downgrade(), window.window_handle(), cx);
                // Without an API key the official assignment proceeds.
                assert!(!assign_edit_prediction_provider(
                    editor,
                    EditPredictionRequestTrigger::EditorCreated,
                    window,
                    cx
                ));
            })
            .expect("window should be open");
        assert!(!has_delegate(cx));

        set_user_settings(r#"{ "fork": { "windsurf": { "api_key": "secret" } } }"#, cx);
        assert!(has_delegate(cx));

        set_user_settings(
            r#"{ "fork": { "edit_prediction_provider": "none", "windsurf": { "api_key": "secret" } } }"#,
            cx,
        );
        assert!(!has_delegate(cx));

        set_user_settings(r#"{ "fork": { "windsurf": { "api_key": "secret" } } }"#, cx);
        assert!(has_delegate(cx));

        set_user_settings(
            r#"{ "fork": { "windsurf": { "api_key": "secret", "enabled": false } } }"#,
            cx,
        );
        assert!(!has_delegate(cx));

        set_user_settings(r#"{ "fork": { "windsurf": { "api_key": "secret" } } }"#, cx);
        window
            .update(cx, |editor, window, cx| {
                assert!(assign_edit_prediction_provider(
                    editor,
                    EditPredictionRequestTrigger::EditorCreated,
                    window,
                    cx
                ));
            })
            .expect("window should be open");
        assert!(has_delegate(cx));
    }

    #[gpui::test]
    fn test_hover_links_are_routed_by_feature(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            assert!(!handle_hover_link("https://zed.dev", window, cx));
            assert!(!handle_hover_link(
                "zed-fork://unknown/open?token=1",
                window,
                cx
            ));
            assert!(!handle_hover_link(
                "zed-fork://json-explorer/unknown",
                window,
                cx
            ));
            assert!(handle_hover_link(
                "zed-fork://json-explorer/open?token=12345",
                window,
                cx
            ));
        });
    }
}
