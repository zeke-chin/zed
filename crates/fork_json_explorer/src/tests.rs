use std::sync::Arc;

use editor::{
    Editor,
    actions::{Paste, SelectAll, Undo},
};
use gpui::{
    Action as _, AppContext as _, ClipboardItem, Entity, TestAppContext, UpdateGlobal as _,
    VisualContext as _, VisualTestContext,
};
use language::{Buffer, Language, LanguageConfig, LanguageRegistry, PLAIN_TEXT};
use settings::{Settings as _, SettingsStore};

use crate::{
    JsonExplorerSettings, ToggleCurrentDocument,
    attach::{CONVERTIBLE_CONTEXT, ENABLED_CONTEXT},
    core::json_utils::format_json_or_json_string,
    state::{ConvertHintKind, ConvertState},
};

const COMPACT_JSON: &str = r#"{"name":"zed","items":[1,2,{"nested":true}]}"#;
const PYTHON_REPR: &str = "{'name': 'zed', 'count': 2, 'ok': True, 'none': None}";
const BROKEN_LINES: &str = "{\"text\": \"first line\nsecond line\", \"n\": 1}";

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        JsonExplorerSettings::register(cx);
        crate::init(cx);
    });
}

fn language_named(name: &'static str) -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: name.into(),
            ..Default::default()
        },
        None,
    ))
}

fn set_settings(json: &str, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(json, cx)
                .expect("user settings should load");
        });
    });
}

struct Fixture {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
}

fn setup<'a>(
    initial_text: &str,
    language: Arc<Language>,
    cx: &'a mut TestAppContext,
) -> (Fixture, &'a mut VisualTestContext) {
    init_test(cx);
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    registry.add(language_named("JSON"));
    let initial_text = initial_text.to_string();
    let buffer = cx.new(|cx| Buffer::local(initial_text, cx));
    buffer.update(cx, |buffer, cx| {
        buffer.set_language_registry(registry);
        buffer.set_language(Some(language), cx);
    });
    let (editor, cx) = cx.add_window_view({
        let buffer = buffer.clone();
        move |window, cx| {
            let mut editor = Editor::for_buffer(buffer, None, window, cx);
            crate::attach_editor(&mut editor, window, cx);
            editor
        }
    });
    cx.focus(&editor);
    (Fixture { editor, buffer }, cx)
}

fn paste(fixture: &Fixture, text: &str, cx: &mut VisualTestContext) {
    cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    fixture
        .editor
        .update_in(cx, |editor, window, cx| editor.paste(&Paste, window, cx));
    cx.run_until_parked();
}

fn paste_over_everything(fixture: &Fixture, text: &str, cx: &mut VisualTestContext) {
    fixture.editor.update_in(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx)
    });
    paste(fixture, text, cx);
}

fn undo(fixture: &Fixture, cx: &mut VisualTestContext) {
    fixture
        .editor
        .update_in(cx, |editor, window, cx| editor.undo(&Undo, window, cx));
    cx.run_until_parked();
}

fn toggle(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(ToggleCurrentDocument.boxed_clone(), cx));
    cx.run_until_parked();
}

fn text(fixture: &Fixture, cx: &mut VisualTestContext) -> String {
    fixture.buffer.read_with(cx, |buffer, _| buffer.text())
}

fn language_name(fixture: &Fixture, cx: &mut VisualTestContext) -> String {
    fixture.buffer.read_with(cx, |buffer, _| {
        buffer
            .language()
            .map(|language| language.name().to_string())
            .unwrap_or_default()
    })
}

fn hint(fixture: &Fixture, cx: &mut VisualTestContext) -> Option<ConvertHintKind> {
    let state = cx.update(|_, cx| ConvertState::global(cx));
    fixture
        .buffer
        .read_with(cx, |buffer, cx| state.read(cx).hint_for(buffer))
}

fn formatted(input: &str) -> String {
    format_json_or_json_string(input)
        .expect("input should be convertible")
        .formatted
}

#[gpui::test]
fn test_pasted_json_is_formatted_and_undone_in_two_steps(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);

    paste(&fixture, COMPACT_JSON, cx);
    assert_eq!(text(&fixture, cx), formatted(COMPACT_JSON));
    assert_eq!(language_name(&fixture, cx), "JSON");

    undo(&fixture, cx);
    assert_eq!(text(&fixture, cx), COMPACT_JSON);

    undo(&fixture, cx);
    assert_eq!(text(&fixture, cx), "");
}

#[gpui::test]
fn test_select_all_paste_replaces_existing_text(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("old text", PLAIN_TEXT.clone(), cx);

    paste_over_everything(&fixture, COMPACT_JSON, cx);

    assert_eq!(text(&fixture, cx), formatted(COMPACT_JSON));
    assert_eq!(language_name(&fixture, cx), "JSON");
}

#[gpui::test]
fn test_partial_paste_is_not_detected(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("// ", PLAIN_TEXT.clone(), cx);
    fixture.editor.update_in(cx, |editor, window, cx| {
        editor.move_to_end(&editor::actions::MoveToEnd, window, cx)
    });

    paste(&fixture, COMPACT_JSON, cx);

    assert_eq!(text(&fixture, cx), format!("// {COMPACT_JSON}"));
    assert_eq!(language_name(&fixture, cx), "Plain Text");
}

#[gpui::test]
fn test_python_repr_shows_hint_then_converts_on_toggle(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);

    paste(&fixture, PYTHON_REPR, cx);
    assert_eq!(text(&fixture, cx), PYTHON_REPR);
    assert_eq!(language_name(&fixture, cx), "Plain Text");
    assert_eq!(hint(&fixture, cx), Some(ConvertHintKind::PythonRepr));

    toggle(cx);
    assert_eq!(text(&fixture, cx), formatted(PYTHON_REPR));
    assert!(text(&fixture, cx).contains("\"ok\": true"));
    assert_eq!(language_name(&fixture, cx), "JSON");
    assert_eq!(hint(&fixture, cx), None);

    undo(&fixture, cx);
    assert_eq!(text(&fixture, cx), PYTHON_REPR);
}

#[gpui::test]
fn test_python_repr_auto_convert_on_paste(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);
    set_settings(
        r#"{ "fork": { "json_explorer": { "python_repr_auto_convert": true } } }"#,
        cx,
    );

    paste(&fixture, PYTHON_REPR, cx);

    assert_eq!(text(&fixture, cx), formatted(PYTHON_REPR));
    assert_eq!(language_name(&fixture, cx), "JSON");
    assert_eq!(hint(&fixture, cx), None);
}

#[gpui::test]
fn test_line_break_repair_shows_hint_then_converts(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);

    paste(&fixture, BROKEN_LINES, cx);
    assert_eq!(text(&fixture, cx), BROKEN_LINES);
    assert_eq!(language_name(&fixture, cx), "Plain Text");
    assert_eq!(hint(&fixture, cx), Some(ConvertHintKind::LineBreakRepair));

    toggle(cx);
    assert_eq!(text(&fixture, cx), formatted(BROKEN_LINES));
    assert!(text(&fixture, cx).contains(r"first line\nsecond line"));
    assert_eq!(language_name(&fixture, cx), "JSON");
}

#[gpui::test]
fn test_line_break_repair_auto_convert_on_paste(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);
    set_settings(
        r#"{ "fork": { "json_explorer": { "line_break_repair_auto_convert": true } } }"#,
        cx,
    );

    paste(&fixture, BROKEN_LINES, cx);

    assert_eq!(text(&fixture, cx), formatted(BROKEN_LINES));
    assert_eq!(language_name(&fixture, cx), "JSON");
}

#[gpui::test]
fn test_unrelated_text_is_left_untouched(cx: &mut TestAppContext) {
    for input in [
        "hello world",
        "def total(items):\n    return [item.price for item in items]\n",
        "[item.price for item in items]",
        "{'value': compute()}",
        "{'blob': b'abc'}",
        "{1, 2, 3}",
        "\"just a string\"",
    ] {
        let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);
        paste(&fixture, input, cx);
        assert_eq!(text(&fixture, cx), input);
        assert_eq!(language_name(&fixture, cx), "Plain Text");
        assert_eq!(hint(&fixture, cx), None);
        toggle(cx);
        assert_eq!(text(&fixture, cx), input);
        assert_eq!(language_name(&fixture, cx), "Plain Text");
    }
}

#[gpui::test]
fn test_disabled_setting_turns_everything_off(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);
    set_settings(
        r#"{ "fork": { "json_explorer": { "enabled": false } } }"#,
        cx,
    );

    paste(&fixture, COMPACT_JSON, cx);
    assert_eq!(text(&fixture, cx), COMPACT_JSON);
    assert_eq!(language_name(&fixture, cx), "Plain Text");

    paste_over_everything(&fixture, PYTHON_REPR, cx);
    assert_eq!(hint(&fixture, cx), None);
    toggle(cx);
    assert_eq!(text(&fixture, cx), PYTHON_REPR);

    let context = fixture
        .editor
        .update_in(cx, |editor, window, cx| editor.key_context(window, cx));
    assert!(!context.contains(ENABLED_CONTEXT));
    assert!(!context.contains(CONVERTIBLE_CONTEXT));
}

#[gpui::test]
fn test_setting_changes_apply_without_restart(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", PLAIN_TEXT.clone(), cx);
    set_settings(
        r#"{ "fork": { "json_explorer": { "enabled": false } } }"#,
        cx,
    );
    paste(&fixture, COMPACT_JSON, cx);
    assert_eq!(language_name(&fixture, cx), "Plain Text");

    set_settings("{}", cx);
    paste_over_everything(&fixture, COMPACT_JSON, cx);
    assert_eq!(text(&fixture, cx), formatted(COMPACT_JSON));
    assert_eq!(language_name(&fixture, cx), "JSON");
}

#[gpui::test]
fn test_other_languages_are_not_touched(cx: &mut TestAppContext) {
    let (fixture, cx) = setup("", language_named("Rust"), cx);

    paste(&fixture, COMPACT_JSON, cx);
    assert_eq!(text(&fixture, cx), COMPACT_JSON);
    assert_eq!(language_name(&fixture, cx), "Rust");
    toggle(cx);
    assert_eq!(text(&fixture, cx), COMPACT_JSON);

    let context = fixture
        .editor
        .update_in(cx, |editor, window, cx| editor.key_context(window, cx));
    assert!(!context.contains(CONVERTIBLE_CONTEXT));
}

#[gpui::test]
fn test_key_context_for_json_and_plain_text(cx: &mut TestAppContext) {
    for language in [
        PLAIN_TEXT.clone(),
        language_named("JSON"),
        language_named("JSONC"),
    ] {
        let (fixture, cx) = setup("", language, cx);
        let context = fixture
            .editor
            .update_in(cx, |editor, window, cx| editor.key_context(window, cx));
        assert!(context.contains(ENABLED_CONTEXT));
        assert!(context.contains(CONVERTIBLE_CONTEXT));
    }
}

#[gpui::test]
fn test_toggle_round_trip_is_byte_identical(cx: &mut TestAppContext) {
    let original = formatted(
        r#"{"big":12345678901234567890,"precise":0.1000000000000000055,"text":"a\nb","list":[1,2]}"#,
    );
    let (fixture, cx) = setup(&original, language_named("JSON"), cx);

    toggle(cx);
    let as_string = text(&fixture, cx);
    assert!(as_string.starts_with('"'));
    assert!(as_string.contains("12345678901234567890"));
    assert!(as_string.contains("0.1000000000000000055"));

    toggle(cx);
    assert_eq!(text(&fixture, cx), original);
    assert_eq!(language_name(&fixture, cx), "JSON");
}

#[gpui::test]
fn test_toggle_undo_restores_previous_text(cx: &mut TestAppContext) {
    let original = formatted(COMPACT_JSON);
    let (fixture, cx) = setup(&original, language_named("JSON"), cx);

    toggle(cx);
    assert_ne!(text(&fixture, cx), original);
    undo(&fixture, cx);
    assert_eq!(text(&fixture, cx), original);
}

#[gpui::test]
fn test_toggle_via_keystroke_with_fork_binding(cx: &mut TestAppContext) {
    let original = formatted(COMPACT_JSON);
    let (fixture, cx) = setup(&original, language_named("JSON"), cx);
    cx.update(|_, cx| {
        // The shared keymap also binds other fork crates' actions, which aren't
        // linked into this test binary; fork_ext checks that the whole file loads.
        let bindings = match settings::KeymapFile::load(
            include_str!("../../../fork/keymaps/default-macos.json"),
            cx,
        ) {
            settings::KeymapFileLoadResult::Success { key_bindings }
            | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => key_bindings
                .into_iter()
                .filter(|binding| binding.action().name().starts_with("json_explorer::"))
                .collect::<Vec<_>>(),
            settings::KeymapFileLoadResult::JsonParseFailure { error } => {
                panic!("fork keymap should parse: {error:#}")
            }
        };
        assert!(!bindings.is_empty());
        cx.bind_keys(bindings);
    });

    cx.simulate_keystrokes("cmd-;");
    cx.run_until_parked();
    assert!(text(&fixture, cx).starts_with('"'));
}
