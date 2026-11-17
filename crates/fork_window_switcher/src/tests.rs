use super::*;
use gpui::{TestAppContext, VisualTestContext};
use project::Project;
use remote::{RemoteConnectionOptions, SshConnectionOptions};
use serde_json::json;
use std::sync::Arc;
use util::path;
use workspace::{AppState, SerializedProjectGroupState};

async fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
    let state = cx.update(|cx| {
        let state = AppState::test(cx);
        super::init(cx);
        editor::init(cx);
        cx.bind_keys([
            gpui::KeyBinding::new("enter", menu::Confirm, Some("Picker")),
            gpui::KeyBinding::new("escape", menu::Cancel, Some("Picker")),
            gpui::KeyBinding::new("ctrl-cmd-w", Toggle, Some("Workspace")),
        ]);
        state
    });
    state
        .fs
        .as_fake()
        .insert_tree(
            path!("/projects"),
            json!({ "alpha": {}, "bravo": {}, "charlie": {}, "delta": {}, "项目": {} }),
        )
        .await;
    state
}

async fn project(state: &Arc<AppState>, name: &str, cx: &mut TestAppContext) -> Entity<Project> {
    Project::test(
        state.fs.clone(),
        [std::path::Path::new(path!("/projects"))
            .join(name)
            .as_path()],
        cx,
    )
    .await
}

fn add_window(
    project: Entity<Project>,
    cx: &mut TestAppContext,
) -> (WindowHandle<MultiWorkspace>, Entity<Workspace>) {
    let (multi_workspace, visual) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let handle = visual.update(|window, _| {
        window
            .window_handle()
            .downcast::<MultiWorkspace>()
            .expect("test window should have a MultiWorkspace root")
    });
    let workspace = multi_workspace.read_with(visual, |window, _| window.workspace().clone());
    (handle, workspace)
}

fn open_picker(
    handle: WindowHandle<MultiWorkspace>,
    cx: &mut TestAppContext,
) -> Entity<Picker<WindowSwitcherDelegate>> {
    let mut visual = VisualTestContext::from_window(handle.into(), cx);
    visual.dispatch_action(Toggle);
    visual.run_until_parked();
    cx.update(|cx| {
        let workspace = handle
            .read(cx)
            .expect("source window should be open")
            .workspace();
        workspace
            .read(cx)
            .active_modal::<WindowSwitcher>(cx)
            .expect("switcher should be open")
            .read(cx)
            .picker
            .clone()
    })
}

fn set_query(
    picker: &Entity<Picker<WindowSwitcherDelegate>>,
    handle: WindowHandle<MultiWorkspace>,
    query: &str,
    cx: &mut TestAppContext,
) {
    let mut visual = VisualTestContext::from_window(handle.into(), cx);
    picker.update_in(&mut visual, |picker, window, cx| {
        picker.set_query(query, window, cx)
    });
    visual.run_until_parked();
}

fn confirm(handle: WindowHandle<MultiWorkspace>, cx: &mut TestAppContext) {
    let mut visual = VisualTestContext::from_window(handle.into(), cx);
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
}

fn assert_closed(workspace: &Entity<Workspace>, cx: &TestAppContext) {
    workspace.read_with(cx, |workspace, cx| {
        assert!(workspace.active_modal::<WindowSwitcher>(cx).is_none());
    });
}

#[gpui::test]
async fn test_list_ordering_markers_and_search(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let alpha = project(&state, "alpha", cx).await;
    let bravo = project(&state, "bravo", cx).await;
    let charlie = project(&state, "charlie", cx).await;
    let (source, alpha_workspace) = add_window(alpha, cx);
    cx.update(|cx| {
        source
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.open_sidebar(cx);
                multi_workspace.test_add_workspace(bravo, window, cx);
                multi_workspace.activate(alpha_workspace.clone(), None, window, cx);
            })
            .expect("source window should be open");
    });
    let (target, _) = add_window(charlie, cx);
    let picker = open_picker(source, cx);
    picker.read_with(cx, |picker, _| {
        let delegate = &picker.delegate;
        assert_eq!(
            delegate
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "bravo", "charlie"]
        );
        assert_eq!(
            delegate
                .entries
                .iter()
                .map(ProjectEntry::marker)
                .collect::<Vec<_>>(),
            vec![Some("Current".into()), None, Some("Window 2".into())]
        );
        assert_eq!(delegate.selected_index(), 1);
        assert_eq!(
            delegate.entries.last().map(|entry| entry.window),
            Some(target)
        );
    });
    set_query(&picker, source, "charlie", cx);
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.match_count(), 1);
        assert_eq!(picker.delegate.selected_index(), 0);
        assert_eq!(
            picker
                .delegate
                .matches
                .first()
                .map(|matched| matched.candidate_id),
            Some(2)
        );
        assert!(
            !picker
                .delegate
                .matches
                .first()
                .expect("charlie should match")
                .positions
                .is_empty()
        );
    });
    set_query(&picker, source, "", cx);
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.selected_index(), 1)
    });
    set_query(&picker, source, "does-not-exist", cx);
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 0));
    confirm(source, cx);
    assert_closed(&alpha_workspace, cx);
}

#[gpui::test]
async fn test_same_window_switch_and_toggle(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let alpha = project(&state, "alpha", cx).await;
    let bravo = project(&state, "bravo", cx).await;
    let (source, alpha_workspace) = add_window(alpha, cx);
    let bravo_workspace = cx.update(|cx| {
        source
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.open_sidebar(cx);
                let bravo_workspace = multi_workspace.test_add_workspace(bravo, window, cx);
                multi_workspace.activate(alpha_workspace.clone(), None, window, cx);
                bravo_workspace
            })
            .expect("source window should be open")
    });
    open_picker(source, cx);
    confirm(source, cx);
    cx.update(|cx| {
        assert_eq!(
            source
                .read(cx)
                .expect("source window should be open")
                .workspace(),
            &bravo_workspace
        )
    });
    assert_closed(&alpha_workspace, cx);
    let picker = open_picker(source, cx);
    picker.read_with(cx, |picker, _| {
        let matched = picker
            .delegate
            .matches
            .get(picker.delegate.selected_index())
            .expect("second entry should be selected");
        assert_eq!(
            picker
                .delegate
                .entries
                .get(matched.candidate_id)
                .map(|entry| entry.name.as_str()),
            Some("alpha")
        );
    });
    let mut visual = VisualTestContext::from_window(source.into(), cx);
    visual.dispatch_action(Toggle);
    visual.run_until_parked();
    assert_closed(&bravo_workspace, cx);
}

#[gpui::test]
async fn test_cross_window_switch_activates_target_group(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let (source, source_workspace) = add_window(project(&state, "alpha", cx).await, cx);
    let (target, target_workspace) = add_window(project(&state, "bravo", cx).await, cx);
    let charlie = project(&state, "charlie", cx).await;
    let charlie_workspace = cx.update(|cx| {
        target
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.open_sidebar(cx);
                let workspace = multi_workspace.test_add_workspace(charlie, window, cx);
                multi_workspace.activate(target_workspace, None, window, cx);
                workspace
            })
            .expect("target window should be open")
    });
    let picker = open_picker(source, cx);
    set_query(&picker, source, "charlie", cx);
    confirm(source, cx);
    cx.update(|cx| {
        assert_eq!(
            target
                .read(cx)
                .expect("target window should be open")
                .workspace(),
            &charlie_workspace
        );
        assert_eq!(cx.active_window(), Some(target.into()));
        assert_eq!(
            source
                .read(cx)
                .expect("source window should be open")
                .workspace(),
            &source_workspace
        );
    });
    assert_closed(&source_workspace, cx);
}

#[gpui::test]
async fn test_current_project_and_escape_only_dismiss(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let (source, workspace) = add_window(project(&state, "alpha", cx).await, cx);
    let picker = open_picker(source, cx);
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.match_count(), 1);
        assert_eq!(picker.delegate.selected_index(), 0);
        assert_eq!(
            picker
                .delegate
                .entries
                .first()
                .map(|entry| entry.is_current),
            Some(true)
        );
    });
    confirm(source, cx);
    assert_closed(&workspace, cx);
    open_picker(source, cx);
    let mut visual = VisualTestContext::from_window(source.into(), cx);
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    assert_closed(&workspace, cx);
    cx.update(|cx| {
        assert_eq!(
            source
                .read(cx)
                .expect("source window should be open")
                .workspace(),
            &workspace
        )
    });
}

#[gpui::test]
async fn test_restore_unopened_project_group(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let (source, workspace) = add_window(project(&state, "alpha", cx).await, cx);
    let key = ProjectGroupKey::new(None, workspace::PathList::new(&[path!("/projects/delta")]));
    cx.update(|cx| {
        source
            .update(cx, |multi_workspace, _, cx| {
                multi_workspace.restore_project_groups(
                    vec![SerializedProjectGroupState {
                        key: key.clone(),
                        expanded: false,
                    }],
                    cx,
                );
                assert!(
                    multi_workspace
                        .last_active_workspace_for_group(&key, cx)
                        .is_none()
                );
            })
            .expect("source window should be open");
    });
    let picker = open_picker(source, cx);
    set_query(&picker, source, "delta", cx);
    confirm(source, cx);
    cx.update(|cx| {
        let multi_workspace = source.read(cx).expect("source window should be open");
        assert_ne!(multi_workspace.workspace(), &workspace);
        assert_eq!(
            multi_workspace.workspace().read(cx).project_group_key(cx),
            key
        );
    });
    assert_closed(&workspace, cx);
}

#[gpui::test]
async fn test_closed_target_reports_error_and_dismisses(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let (source, workspace) = add_window(project(&state, "alpha", cx).await, cx);
    let (target, _) = add_window(project(&state, "bravo", cx).await, cx);
    let picker = open_picker(source, cx);
    set_query(&picker, source, "bravo", cx);
    cx.update(|cx| {
        target
            .update(cx, |_, window, _| window.remove_window())
            .expect("target window should be open")
    });
    confirm(source, cx);
    assert_closed(&workspace, cx);
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(workspace.notification_ids().len(), 1)
    });
}

#[gpui::test]
async fn test_remote_hostname_and_unicode_highlights(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let (source, workspace) = add_window(project(&state, "项目", cx).await, cx);
    let host = RemoteConnectionOptions::Ssh(SshConnectionOptions {
        host: "dev-box".to_string().into(),
        username: None,
        port: None,
        password: None,
        args: None,
        port_forwards: None,
        connection_timeout: None,
        nickname: None,
        upload_binary_over_ssh: false,
    });
    let key = ProjectGroupKey::new(Some(host), workspace::PathList::new(&[path!("/srv/infra")]));
    cx.update(|cx| {
        source
            .update(cx, |multi_workspace, _, cx| {
                multi_workspace.restore_project_groups(
                    vec![SerializedProjectGroupState {
                        key,
                        expanded: false,
                    }],
                    cx,
                );
            })
            .expect("source window should be open");
    });
    let picker = open_picker(source, cx);
    set_query(&picker, source, "dev-box", cx);
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.match_count(), 1);
        let matched = picker.delegate.matches.first().expect("host should match");
        let entry = picker
            .delegate
            .entries
            .get(matched.candidate_id)
            .expect("matched entry should exist");
        assert_eq!(entry.path.as_str(), "ssh: dev-box /srv/infra");
    });
    set_query(&picker, source, "项目", cx);
    let mut visual = VisualTestContext::from_window(source.into(), cx);
    picker.update_in(&mut visual, |picker, window, cx| {
        let matched = picker
            .delegate
            .matches
            .first()
            .expect("unicode project should match");
        assert!(
            matched
                .positions
                .iter()
                .all(|position| matched.string.is_char_boundary(*position))
        );
        assert!(picker.delegate.render_match(0, true, window, cx).is_some());
    });
    visual.simulate_keystrokes("escape");
    visual.run_until_parked();
    assert_closed(&workspace, cx);
}

#[gpui::test]
async fn test_group_switch_preserves_last_active_workspace(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let alpha = project(&state, "alpha", cx).await;
    let bravo = project(&state, "bravo", cx).await;
    let other_alpha = project(&state, "alpha", cx).await;
    let (source, original_workspace) = add_window(alpha, cx);
    let last_active_workspace = cx.update(|cx| {
        source
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.open_sidebar(cx);
                let workspace = multi_workspace.test_add_workspace(other_alpha, window, cx);
                multi_workspace.test_add_workspace(bravo, window, cx);
                workspace
            })
            .expect("source window should be open")
    });
    let picker = open_picker(source, cx);
    set_query(&picker, source, "alpha", cx);
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 1));
    confirm(source, cx);
    cx.update(|cx| {
        assert_eq!(
            source
                .read(cx)
                .expect("source window should be open")
                .workspace(),
            &last_active_workspace
        );
        assert_ne!(last_active_workspace, original_workspace);
    });
    let mut visual = VisualTestContext::from_window(source.into(), cx);
    visual.update(|window, cx| {
        assert!(
            last_active_workspace
                .focus_handle(cx)
                .contains_focused(window, cx)
        );
    });
}

#[gpui::test]
async fn test_removed_group_is_not_reopened(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let (source, workspace) = add_window(project(&state, "alpha", cx).await, cx);
    let key = ProjectGroupKey::new(None, workspace::PathList::new(&[path!("/projects/delta")]));
    cx.update(|cx| {
        source
            .update(cx, |multi_workspace, _, cx| {
                multi_workspace.restore_project_groups(
                    vec![SerializedProjectGroupState {
                        key: key.clone(),
                        expanded: false,
                    }],
                    cx,
                );
            })
            .expect("source window should be open");
    });
    let picker = open_picker(source, cx);
    set_query(&picker, source, "delta", cx);
    let task = cx.update(|cx| {
        source
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.remove_project_group(&key, window, cx)
            })
            .expect("source window should be open")
    });
    assert!(matches!(task.await, Ok(true)));
    confirm(source, cx);
    cx.update(|cx| {
        let multi_workspace = source.read(cx).expect("source window should be open");
        assert_eq!(multi_workspace.workspace(), &workspace);
        assert!(multi_workspace.group_state_by_key(&key).is_none());
    });
    assert_closed(&workspace, cx);
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(workspace.notification_ids().len(), 1)
    });
}

#[gpui::test]
async fn test_empty_workspace_is_listed(cx: &mut TestAppContext) {
    let state = init_test(cx).await;
    let project = Project::test(state.fs.clone(), [], cx).await;
    let (source, workspace) = add_window(project, cx);
    let picker = open_picker(source, cx);
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.match_count(), 1);
        assert_eq!(
            picker
                .delegate
                .entries
                .first()
                .map(|entry| entry.name.as_str()),
            Some("Empty Workspace")
        );
        assert_eq!(
            picker
                .delegate
                .entries
                .first()
                .map(|entry| entry.path.as_str()),
            Some("")
        );
    });
    confirm(source, cx);
    assert_closed(&workspace, cx);
}
