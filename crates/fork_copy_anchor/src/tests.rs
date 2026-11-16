use std::{sync::Arc, time::Duration};

use editor::{Editor, SelectionEffects};
use fs::{FakeFs, Fs};
use gpui::{
    AppContext as _,
    Action as _, Entity, TestAppContext, UpdateGlobal as _, VisualTestContext,
};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::{Project, ProjectPath};
use serde_json::json;
use settings::SettingsStore;
use util::{path, rel_path::rel_path};
use workspace::{MultiWorkspace, Workspace};

use crate::{Copy, ToggleContextMode, state::CopyAnchorModel};

const SOURCE: &str = "fn main() {\n    let a = 1;\n\n    let b = 2;\n}\n";

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        cx.set_global(db::AppDatabase::test_new());
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        editor::init(cx);
        crate::init(cx);
        cx.observe_new(|editor: &mut Editor, window, cx| {
            if editor.mode().is_full()
                && let Some(window) = window
            {
                crate::attach_editor(editor, window, cx);
            }
        })
        .detach();
        cx.observe_new(|workspace: &mut Workspace, window, cx| {
            if let Some(window) = window {
                crate::status_items(workspace, window, cx);
            }
        })
        .detach();
    });
}

struct Fixture {
    fs: Arc<FakeFs>,
    workspace: Entity<Workspace>,
    project: Entity<Project>,
    editor: Entity<Editor>,
}

async fn setup(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "main.rs": SOURCE, "other.rs": "fn other() {}\n" } }),
    )
    .await;
    fs.create_dir(paths::config_dir())
        .await
        .expect("config dir should be created");
    fs.insert_file(paths::settings_file(), b"{}".to_vec()).await;
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    let editor = open_file(&workspace, &project, "src/main.rs", cx).await;
    (
        Fixture {
            fs,
            workspace,
            project,
            editor,
        },
        cx,
    )
}

async fn open_file(
    workspace: &Entity<Workspace>,
    project: &Entity<Project>,
    relative_path: &str,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id())
    });
    let worktree_id = worktree_id.expect("project should have a worktree");
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(
                ProjectPath {
                    worktree_id,
                    path: rel_path(relative_path).into(),
                },
                None,
                true,
                window,
                cx,
            )
        })
        .await
        .expect("file should open")
        .downcast::<Editor>()
        .expect("item should be an editor");
    cx.run_until_parked();
    editor
}

fn select(editor: &Entity<Editor>, ranges: Vec<std::ops::Range<Point>>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges(ranges)
        });
    });
}

fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

fn write_clipboard(text: &str, cx: &mut VisualTestContext) {
    cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string())));
}

fn dispatch_copy(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(Copy.boxed_clone(), cx));
    cx.run_until_parked();
}

fn dispatch_toggle(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.dispatch_action(ToggleContextMode.boxed_clone(), cx));
    cx.run_until_parked();
}

fn set_settings(json: &str, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(json, cx)
                .result()
                .expect("settings should parse");
        });
    });
    cx.run_until_parked();
}

fn preview(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| {
        CopyAnchorModel::try_global(cx)
            .and_then(|model| model.read(cx).preview().map(str::to_string))
    })
}

fn main_path() -> String {
    path!("/root/src/main.rs").to_string()
}

#[gpui::test]
async fn test_context_copy_of_a_fragment(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(1, 14)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(
        clipboard_text(cx),
        Some(format!(
            "```{}:1-2\nfn main() {{\n    let a = 1;\n```",
            main_path()
        ))
    );
}

#[gpui::test]
async fn test_boundary_blank_lines_are_trimmed(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(1, 0)..Point::new(4, 0)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(
        clipboard_text(cx),
        Some(format!(
            "```{}:2-4\n    let a = 1;\n\n    let b = 2;\n```",
            main_path()
        ))
    );
}

#[gpui::test]
async fn test_whole_file_copies_only_the_path(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(5, 0)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(clipboard_text(cx), Some(main_path()));
}

#[gpui::test]
async fn test_multiple_selections_become_separate_blocks_in_file_order(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![
            Point::new(3, 4)..Point::new(3, 14),
            Point::new(0, 0)..Point::new(0, 2),
        ],
        cx,
    );
    dispatch_copy(cx);
    let path = main_path();
    assert_eq!(
        clipboard_text(cx),
        Some(format!(
            "```{path}:1\nfn\n```\n\n```{path}:4\nlet b = 2;\n```"
        ))
    );
}

#[gpui::test]
async fn test_unsaved_edits_are_copied(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    fixture.editor.update_in(cx, |editor, window, cx| {
        editor.set_text("changed line\nsecond line\n", window, cx);
    });
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(
        clipboard_text(cx),
        Some(format!("```{}:1\nchanged\n```", main_path()))
    );
}

#[gpui::test]
async fn test_buffer_without_a_disk_path_copies_natively(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let buffer = cx.update(|_, cx| cx.new(|cx| Buffer::local("scratch text", cx)));
    let project = fixture.project.clone();
    let editor = fixture.workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    cx.run_until_parked();
    select(&editor, vec![Point::new(0, 0)..Point::new(0, 7)], cx);
    dispatch_copy(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("scratch"));
}

#[gpui::test]
async fn test_multi_buffer_view_copies_natively(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    let buffer = fixture
        .project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/src/other.rs"), cx)
        })
        .await
        .expect("buffer should open");
    let multi_buffer = cx.update(|_, cx| {
        cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::new(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                buffer,
                [Point::new(0, 0)..Point::new(0, 13)],
                0,
                cx,
            );
            multi_buffer
        })
    });
    let project = fixture.project.clone();
    let editor = fixture.workspace.update_in(cx, |workspace, window, cx| {
        let editor = cx.new(|cx| Editor::for_multibuffer(multi_buffer, Some(project), window, cx));
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        editor
    });
    cx.run_until_parked();
    select(&editor, vec![Point::new(0, 0)..Point::new(0, 8)], cx);
    dispatch_copy(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("fn other"));
}

#[gpui::test]
async fn test_plain_mode_copies_natively(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    set_settings(r#"{ "fork": { "copy_anchor": { "context_mode": false } } }"#, cx);
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("fn main"));
}

#[gpui::test]
async fn test_empty_selection_copies_natively_and_has_no_marker(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(1, 0)..Point::new(1, 0)],
        cx,
    );
    let markers = key_context_markers(&fixture.editor, cx);
    assert_eq!(markers, (true, false));
    dispatch_copy(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("    let a = 1;\n"));
}

fn key_context_markers(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (bool, bool) {
    editor.update_in(cx, |editor, window, cx| {
        let context = editor.key_context(window, cx);
        (
            context.contains(crate::copy::ENABLED_CONTEXT),
            context.contains(crate::copy::HAS_SELECTION_CONTEXT),
        )
    })
}

#[gpui::test]
async fn test_key_context_follows_selection_and_settings(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 2)],
        cx,
    );
    assert_eq!(key_context_markers(&fixture.editor, cx), (true, true));

    select(
        &fixture.editor,
        vec![Point::new(0, 1)..Point::new(0, 1)],
        cx,
    );
    assert_eq!(key_context_markers(&fixture.editor, cx), (true, false));

    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 2)],
        cx,
    );
    set_settings(r#"{ "fork": { "copy_anchor": { "enabled": false } } }"#, cx);
    assert_eq!(key_context_markers(&fixture.editor, cx), (false, false));

    set_settings(r#"{ "fork": { "copy_anchor": { "enabled": true } } }"#, cx);
    assert_eq!(key_context_markers(&fixture.editor, cx), (true, true));
}

#[gpui::test]
async fn test_keybinding_uses_anchor_copy_and_falls_back_to_native(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    cx.update(|_, cx| {
        let bindings = settings::KeymapFile::load(
            include_str!("../../../fork/keymaps/default-macos.json"),
            cx,
        );
        let settings::KeymapFileLoadResult::Success { key_bindings } = bindings else {
            panic!("fork keymap should load without errors");
        };
        cx.bind_keys([gpui::KeyBinding::new(
            "cmd-c",
            editor::actions::Copy,
            Some("Editor"),
        )]);
        cx.bind_keys(key_bindings);
    });
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    cx.simulate_keystrokes("cmd-c");
    assert_eq!(
        clipboard_text(cx),
        Some(format!("```{}:1\nfn main\n```", main_path()))
    );

    cx.simulate_keystrokes("ctrl-\\ ctrl-\\");
    assert_eq!(clipboard_text(cx).as_deref(), Some("fn main"));

    set_settings(r#"{ "fork": { "copy_anchor": { "enabled": false } } }"#, cx);
    write_clipboard("untouched", cx);
    cx.simulate_keystrokes("cmd-c");
    assert_eq!(clipboard_text(cx).as_deref(), Some("fn main"));
}

#[gpui::test]
async fn test_toggle_swaps_clipboard_between_both_forms(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    let anchored = format!("```{}:1\nfn main\n```", main_path());
    assert_eq!(clipboard_text(cx), Some(anchored.clone()));

    dispatch_toggle(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("fn main"));
    let saved = fixture
        .fs
        .load(paths::settings_file())
        .await
        .expect("settings file should exist");
    assert!(saved.contains("\"context_mode\": false"), "{saved}");

    set_settings(&saved, cx);
    select(
        &fixture.editor,
        vec![Point::new(1, 0)..Point::new(1, 5)],
        cx,
    );
    dispatch_toggle(cx);
    assert_eq!(clipboard_text(cx), Some(anchored));
}

#[gpui::test]
async fn test_toggle_does_not_restore_stale_content_after_external_copy(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    write_clipboard("from elsewhere", cx);
    select(
        &fixture.editor,
        vec![Point::new(1, 4)..Point::new(1, 7)],
        cx,
    );
    dispatch_toggle(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("let"));
}

#[gpui::test]
async fn test_toggle_without_selection_keeps_external_clipboard(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    write_clipboard("from elsewhere", cx);
    select(
        &fixture.editor,
        vec![Point::new(1, 2)..Point::new(1, 2)],
        cx,
    );
    dispatch_toggle(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("from elsewhere"));
}

#[gpui::test]
async fn test_toggle_without_prior_copy_copies_current_selection(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    write_clipboard("old", cx);
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_toggle(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("fn main"));
}

#[gpui::test]
async fn test_disabled_feature_ignores_actions_and_clears_memory(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    assert!(preview(cx).is_some());

    set_settings(r#"{ "fork": { "copy_anchor": { "enabled": false } } }"#, cx);
    assert_eq!(preview(cx), None);

    write_clipboard("keep", cx);
    dispatch_toggle(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("keep"));
    let saved = fixture
        .fs
        .load(paths::settings_file())
        .await
        .expect("settings file should exist");
    assert!(!saved.contains("context_mode"), "{saved}");

    dispatch_copy(cx);
    assert_eq!(clipboard_text(cx).as_deref(), Some("fn main"));
    assert_eq!(preview(cx), None);

    set_settings(r#"{ "fork": { "copy_anchor": { "enabled": true } } }"#, cx);
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 2)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(
        clipboard_text(cx),
        Some(format!("```{}:1\nfn\n```", main_path()))
    );
}

#[gpui::test]
async fn test_preview_disappears_after_duration_and_restarts_on_copy(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    set_settings(
        r#"{ "fork": { "copy_anchor": { "preview_duration": 2, "preview_max_length": 100 } } }"#,
        cx,
    );
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(preview(cx).as_deref(), Some("../main.rs:1\\nfn main"));

    cx.executor().advance_clock(Duration::from_millis(1500));
    cx.run_until_parked();
    assert!(preview(cx).is_some());

    dispatch_copy(cx);
    cx.executor().advance_clock(Duration::from_millis(1500));
    cx.run_until_parked();
    assert!(preview(cx).is_some());

    cx.executor().advance_clock(Duration::from_millis(600));
    cx.run_until_parked();
    assert_eq!(preview(cx), None);
}

#[gpui::test]
async fn test_zero_duration_never_shows_a_preview(cx: &mut TestAppContext) {
    let (fixture, cx) = setup(cx).await;
    set_settings(r#"{ "fork": { "copy_anchor": { "preview_duration": 0 } } }"#, cx);
    select(
        &fixture.editor,
        vec![Point::new(0, 0)..Point::new(0, 7)],
        cx,
    );
    dispatch_copy(cx);
    assert_eq!(preview(cx), None);
}

