// FORK: Terminal selection precedes upstream default-behavior prompting and never writes settings.
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result};
use cli::{CliOpenTarget, CliRequest, CliResponse, CliResponseSink, CliWindowEntry, OpenBehavior};
use futures::{StreamExt, channel::mpsc};
use gpui::{AnyWindowHandle, App, AsyncApp, WindowHandle};
use project::ProjectGroupKey;
use settings::Settings;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use util::ResultExt;
use workspace::{AppState, MultiWorkspace};

pub(crate) enum ResolvedOpenTarget {
    NewWindow,
    Window(WindowHandle<MultiWorkspace>),
    Cancel,
}

pub(crate) fn ordered_windows(
    mut stack: Vec<AnyWindowHandle>,
    windows: Vec<AnyWindowHandle>,
) -> Vec<AnyWindowHandle> {
    for window in windows {
        if !stack.contains(&window) {
            stack.push(window);
        }
    }
    stack
}

fn project_title(key: &ProjectGroupKey, details: &HashMap<PathBuf, usize>) -> String {
    let name = key.display_name(details);
    match key.host() {
        Some(host) => format!(
            "{}: {} {}",
            host.connection_type(),
            host.display_name(),
            name
        ),
        None => name.to_string(),
    }
}

pub(crate) fn collect_windows(cx: &App) -> Vec<CliWindowEntry> {
    ordered_windows(cx.window_stack().unwrap_or_default(), cx.windows())
        .into_iter()
        .filter_map(|handle| {
            let handle = handle.downcast::<MultiWorkspace>()?;
            let multi_workspace = handle.read(cx).log_err()?;
            if !multi_workspace.multi_workspace_enabled(cx) {
                return None;
            }
            let active_key = multi_workspace.workspace().read(cx).project_group_key(cx);
            let mut keys = multi_workspace.project_group_keys();
            // Single-project windows can have no retained sidebar group yet.
            if !keys.contains(&active_key) {
                keys.push(active_key.clone());
            }
            let mut paths: Vec<_> = keys
                .iter()
                .flat_map(|key| key.path_list().paths().iter().cloned())
                .collect();
            paths.sort_unstable();
            paths.dedup();
            let details =
                util::disambiguate::compute_disambiguation_details(&paths, |path, detail| {
                    project::path_suffix(path, detail)
                });
            let details = paths.into_iter().zip(details).collect();
            Some(CliWindowEntry {
                window_id: handle.window_id().as_u64(),
                title: project_title(&active_key, &details),
                other_projects: keys
                    .iter()
                    .filter(|key| **key != active_key)
                    .map(|key| project_title(key, &details))
                    .collect(),
            })
        })
        .collect()
}

pub(crate) async fn resolve_open_target(
    paths: &[String],
    interactive: bool,
    open_behavior: OpenBehavior,
    app_state: &Arc<AppState>,
    responses: &dyn CliResponseSink,
    requests: &mut mpsc::UnboundedReceiver<CliRequest>,
    cx: &mut AsyncApp,
) -> Result<Option<ResolvedOpenTarget>> {
    if !interactive || open_behavior != OpenBehavior::Default || paths.is_empty() {
        return Ok(None);
    }
    let paths =
        super::open_listener::derive_paths_with_position(app_state.fs.as_ref(), paths.to_vec())
            .await
            .into_iter()
            .map(|path| path.path)
            .collect::<Vec<_>>();
    let mut first_directory = None;
    for path in &paths {
        if app_state.fs.is_dir(path).await {
            first_directory = Some(path);
            break;
        }
    }
    let Some(first_directory) = first_directory else {
        return Ok(None);
    };
    let windows = cx.update(|cx| {
        // Local CLI paths must not match identical paths on a remote host.
        let contained = cx.windows().into_iter().any(|window| {
            window
                .downcast::<MultiWorkspace>()
                .and_then(|handle| handle.read(cx).log_err())
                .is_some_and(|multi_workspace| {
                    multi_workspace.workspaces().any(|workspace| {
                        let project = workspace.read(cx).project().read(cx);
                        project.is_local()
                            && project.visibility_for_paths(&paths, false, cx).is_some()
                    })
                })
        });
        if contained {
            Vec::new()
        } else {
            collect_windows(cx)
        }
    });
    if windows.is_empty() {
        return Ok(None);
    }
    let target_name = first_directory
        .file_name()
        .unwrap_or_else(|| first_directory.as_os_str())
        .to_string_lossy()
        .into_owned();
    let offered_ids: Vec<_> = windows.iter().map(|entry| entry.window_id).collect();
    responses.send(CliResponse::PromptOpenTarget {
        target_name,
        windows,
    })?;
    match requests.next().await {
        None
        | Some(CliRequest::SetOpenTarget {
            target: CliOpenTarget::Cancel,
        }) => Ok(Some(ResolvedOpenTarget::Cancel)),
        Some(CliRequest::SetOpenTarget {
            target: CliOpenTarget::NewWindow,
        }) => Ok(Some(ResolvedOpenTarget::NewWindow)),
        Some(CliRequest::SetOpenTarget {
            target: CliOpenTarget::Window(window_id),
        }) => {
            anyhow::ensure!(
                offered_ids.contains(&window_id),
                "Selected Zed window {window_id} is no longer available"
            );
            let handle = cx
                .update(|cx| {
                    cx.windows()
                        .into_iter()
                        .find(|handle| handle.window_id().as_u64() == window_id)
                        .and_then(|handle| handle.downcast::<MultiWorkspace>())
                        .filter(|handle| {
                            handle.read(cx).is_ok_and(|multi_workspace| {
                                multi_workspace.multi_workspace_enabled(cx)
                            })
                        })
                })
                .with_context(|| {
                    format!("Selected Zed window {window_id} is no longer available")
                })?;
            Ok(Some(ResolvedOpenTarget::Window(handle)))
        }
        Some(request) => anyhow::bail!("Unexpected response to Zed window selection: {request:?}"),
    }
}

pub(crate) fn prepare_window(
    handle: WindowHandle<MultiWorkspace>,
    cx: &mut AsyncApp,
) -> Result<()> {
    handle
        .update(cx, |multi_workspace, window, cx| {
            anyhow::ensure!(
                multi_workspace.multi_workspace_enabled(cx),
                "Selected Zed window can no longer add projects"
            );
            // Supplying requesting_window bypasses the retention logic in workspace::open_paths.
            if AgentSettings::get_global(cx).threads_sidebar.auto_open {
                multi_workspace.open_sidebar(cx);
            } else {
                multi_workspace.retain_active_workspace(cx);
            }
            window.activate_window();
            Ok(())
        })
        .context("Selected Zed window is no longer available")?
}
