use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{App, AppContext as _, Context, PromptLevel, TaskExt as _, Window, actions};
use language::Point;
use settings::Settings as _;
use sqlez::connection::Connection;
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    notifications::NotificationId,
    {OpenOptions, OpenVisible},
};

use crate::WindsurfSettings;

actions!(
    windsurf,
    [
        /// Imports the Windsurf API key from the local Devin or Windsurf login data
        /// and saves it to the user settings.
        ImportApiKey,
        /// Opens the user settings file at the `fork.windsurf.api_key` field.
        OpenApiKeySetting
    ]
);

const AUTH_STATUS_KEY: &str = "windsurfAuthStatus";

const CANDIDATE_RELATIVE_PATHS: [&str; 4] = [
    "Library/Application Support/Devin/User/globalStorage/state.vscdb",
    "Library/Application Support/Windsurf/User/globalStorage/state.vscdb",
    ".config/Devin/User/globalStorage/state.vscdb",
    ".config/Windsurf/User/globalStorage/state.vscdb",
];

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ImportApiKey, window, cx| {
            import_api_key(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenApiKeySetting, window, cx| {
            open_api_key_setting(workspace, window, cx);
        });
    })
    .detach();
}

struct FoundApiKey {
    source: PathBuf,
    api_key: String,
}

impl std::fmt::Debug for FoundApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FoundApiKey")
            .field("source", &self.source)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

fn candidate_paths(home: &Path) -> Vec<PathBuf> {
    CANDIDATE_RELATIVE_PATHS
        .iter()
        .map(|relative| home.join(relative))
        .collect()
}

/// Returns the first non-empty key found, or one explanatory line per candidate.
fn find_api_key(candidates: &[PathBuf]) -> Result<FoundApiKey, Vec<String>> {
    let mut attempts = Vec::new();
    for candidate in candidates {
        match read_api_key(candidate) {
            Ok(api_key) => {
                return Ok(FoundApiKey {
                    source: candidate.clone(),
                    api_key,
                });
            }
            Err(error) => attempts.push(format!("{}（{error:#}）", candidate.display())),
        }
    }
    Err(attempts)
}

/// `sqlez` only opens databases read-write, so the file is read from a private
/// copy (with its write-ahead log, so a running editor's latest state is seen)
/// that is deleted before returning. The original is never opened.
fn read_api_key(path: &Path) -> Result<String> {
    if !path.is_file() {
        bail!("文件不存在");
    }
    let directory = tempfile::tempdir().context("无法创建临时目录")?;
    let copy_path = directory.path().join("state.vscdb");
    std::fs::copy(path, &copy_path).context("无法复制登录数据")?;
    if let Some(file_name) = path.file_name() {
        let mut log_name = file_name.to_os_string();
        log_name.push("-wal");
        let log_path = path.with_file_name(log_name);
        if log_path.is_file() {
            std::fs::copy(&log_path, directory.path().join("state.vscdb-wal"))
                .context("无法复制登录数据的日志文件")?;
        }
    }
    let copy_path = copy_path.to_str().context("临时路径不是有效的 UTF-8")?;

    let connection = Connection::open_file(copy_path);
    let mut select = connection
        .select_row_bound::<String, String>("SELECT value FROM ItemTable WHERE key = ?")
        .context("不是有效的登录数据库")?;
    let value = select(AUTH_STATUS_KEY.to_string())
        .context("读取失败")?
        .with_context(|| format!("没有 {AUTH_STATUS_KEY}，可能未登录"))?;
    parse_api_key(&value)
}

fn parse_api_key(value: &str) -> Result<String> {
    let parsed: serde_json::Value = serde_json::from_str(value)
        .map_err(|_| anyhow::anyhow!("{AUTH_STATUS_KEY} 不是有效的 JSON"))?;
    match parsed.get("apiKey").and_then(|api_key| api_key.as_str()) {
        Some(api_key) if !api_key.trim().is_empty() => Ok(api_key.trim().to_string()),
        Some(_) => bail!("{AUTH_STATUS_KEY}.apiKey 为空，可能已登出"),
        None => bail!("{AUTH_STATUS_KEY} 里没有 apiKey 字段"),
    }
}

fn mask_api_key(api_key: &str) -> String {
    let characters: Vec<char> = api_key.chars().collect();
    if characters.len() < 16 {
        return "*".repeat(characters.len().min(8));
    }
    let head: String = characters[..6].iter().collect();
    let tail: String = characters[characters.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<ImportApiKey>(), message),
        cx,
    );
}

fn import_api_key(_workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let candidates = candidate_paths(paths::home_dir());
    let lookup = cx.background_spawn(async move { find_api_key(&candidates) });
    cx.spawn_in(window, async move |workspace, cx| {
        let found = match lookup.await {
            Ok(found) => found,
            Err(attempts) => {
                let message = format!(
                    "没有在本机找到可导入的 Windsurf 登录数据：\n{}\n请先在 Devin 或 Windsurf 里登录，或运行「windsurf: open api key setting」手动填写。",
                    attempts.join("\n")
                );
                workspace
                    .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                    .log_err();
                return Ok(());
            }
        };

        let existing = cx.update(|_, cx| WindsurfSettings::get_global(cx).api_key.clone())?;
        if existing.as_deref() == Some(found.api_key.as_str()) {
            workspace.update(cx, |workspace, cx| {
                show_toast(workspace, "设置里已经是同一个 Windsurf 密钥，无需改动。".into(), cx)
            })?;
            return Ok(());
        }
        if let Some(existing) = existing {
            let detail = format!(
                "当前：{}\n本机：{}（来自 {}）",
                mask_api_key(&existing),
                mask_api_key(&found.api_key),
                found.source.display()
            );
            let answer = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    "覆盖设置里已有的 Windsurf 密钥？",
                    Some(&detail),
                    &["覆盖", "取消"],
                    cx,
                )
            })?;
            if answer.await != Ok(0) {
                return Ok(());
            }
        }

        let completion = cx.update(|_, cx| write_api_key(found.api_key.clone(), cx))?;
        let message = match completion.await {
            Ok(Ok(())) => format!(
                "已导入 Windsurf 密钥 {}\n来源：{}",
                mask_api_key(&found.api_key),
                found.source.display()
            ),
            Ok(Err(error)) => format!("写入用户设置失败：{error:#}"),
            Err(_) => "写入用户设置被中断".to_string(),
        };
        workspace.update(cx, |workspace, cx| show_toast(workspace, message, cx))?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn write_api_key(api_key: String, cx: &App) -> futures::channel::oneshot::Receiver<Result<()>> {
    let fs = <dyn fs::Fs>::global(cx);
    settings::update_settings_file_with_completion(fs, cx, move |content, _| {
        content
            .fork
            .get_or_insert_default()
            .windsurf
            .get_or_insert_default()
            .api_key = Some(api_key);
    })
}

fn open_api_key_setting(
    _workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let fs = <dyn fs::Fs>::global(cx);
    let completion = settings::update_settings_file_with_completion(fs, cx, |content, _| {
        let windsurf = content
            .fork
            .get_or_insert_default()
            .windsurf
            .get_or_insert_default();
        if windsurf.api_key.is_none() {
            windsurf.api_key = Some(String::new());
        }
    });
    cx.spawn_in(window, async move |workspace, cx| {
        match completion.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::error!("windsurf: failed to prepare the api_key setting: {error:#}");
            }
            Err(_) => log::error!("windsurf: preparing the api_key setting was cancelled"),
        }
        let open = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(
                paths::settings_file().clone(),
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    ..Default::default()
                },
                window,
                cx,
            )
        })?;
        let item = open.await?;
        if let Some(editor) = item.downcast::<Editor>() {
            editor.update_in(cx, |editor, window, cx| {
                let text = editor.buffer().read(cx).snapshot(cx).text();
                if let Some(position) = api_key_value_position(&text) {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([position..position]),
                    );
                }
            })?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

/// Finds where to put the cursor for `fork.windsurf.api_key`: inside the value
/// string when there is one, otherwise right after the key.
fn api_key_value_position(text: &str) -> Option<Point> {
    let windsurf_start = text.find("\"windsurf\"")?;
    let key_start = windsurf_start + text[windsurf_start..].find("\"api_key\"")?;
    let key_end = key_start + "\"api_key\"".len();
    let after_key = &text[key_end..];
    let colon = after_key.find(':')?;
    let value_start = key_end + colon + 1;
    let value_prefix = &text[value_start..];
    let offset = match value_prefix.trim_start().strip_prefix('"') {
        Some(_) => {
            let quote = value_prefix.len() - value_prefix.trim_start().len();
            value_start + quote + 1
        }
        None => key_end,
    };
    let before = &text[..offset];
    let row = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    Some(Point::new(row, (offset - line_start) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use gpui::TestAppContext;
    use settings::SettingsStore;
    use std::sync::Arc;

    const SAMPLE_KEY: &str = "devin-sk-0123456789abcdef0123456789abcdef";

    fn create_database(path: &Path, rows: &[(&str, &str)]) -> Result<()> {
        let connection = Connection::open_file(path.to_str().context("path")?);
        connection
            .exec("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")?(
        )?;
        for (key, value) in rows {
            connection.exec_bound::<(String, String)>(
                "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
            )?((key.to_string(), value.to_string()))?;
        }
        Ok(())
    }

    fn database_in(directory: &tempfile::TempDir, rows: &[(&str, &str)]) -> Result<PathBuf> {
        let path = directory.path().join("state.vscdb");
        create_database(&path, rows)?;
        Ok(path)
    }

    #[test]
    fn reads_api_key_from_database() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let value = format!("{{\"apiKey\":\"{SAMPLE_KEY}\",\"name\":\"x\"}}");
        let path = database_in(&directory, &[("other", "1"), (AUTH_STATUS_KEY, &value)])?;
        assert_eq!(read_api_key(&path)?, SAMPLE_KEY);
        Ok(())
    }

    #[test]
    fn reports_missing_key_without_leaking() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = database_in(&directory, &[("other", "1")])?;
        let error = format!("{:#}", read_api_key(&path).err().context("should fail")?);
        assert!(error.contains("可能未登录"), "{error}");
        Ok(())
    }

    #[test]
    fn reports_missing_or_empty_field() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = database_in(&directory, &[(AUTH_STATUS_KEY, "{\"name\":\"x\"}")])?;
        let error = format!("{:#}", read_api_key(&path).err().context("should fail")?);
        assert!(error.contains("没有 apiKey"), "{error}");

        let directory = tempfile::tempdir()?;
        let path = database_in(&directory, &[(AUTH_STATUS_KEY, "{\"apiKey\":\"\"}")])?;
        let error = format!("{:#}", read_api_key(&path).err().context("should fail")?);
        assert!(error.contains("已登出"), "{error}");
        Ok(())
    }

    #[test]
    fn reports_invalid_json_without_leaking() {
        let error = format!(
            "{:#}",
            parse_api_key("secret-not-json")
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        );
        assert!(error.contains("不是有效的 JSON"), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn reports_missing_file() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let error = format!(
            "{:#}",
            read_api_key(&directory.path().join("missing.vscdb"))
                .err()
                .context("should fail")?
        );
        assert!(error.contains("文件不存在"), "{error}");
        Ok(())
    }

    #[test]
    fn does_not_modify_original_database() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let value = format!("{{\"apiKey\":\"{SAMPLE_KEY}\"}}");
        let path = database_in(&directory, &[(AUTH_STATUS_KEY, &value)])?;
        let before = std::fs::read(&path)?;
        read_api_key(&path)?;
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn falls_back_to_later_candidates() -> Result<()> {
        let home = tempfile::tempdir()?;
        let devin = home.path().join(CANDIDATE_RELATIVE_PATHS[0]);
        let windsurf = home.path().join(CANDIDATE_RELATIVE_PATHS[1]);
        std::fs::create_dir_all(windsurf.parent().context("parent")?)?;
        let value = format!("{{\"apiKey\":\"{SAMPLE_KEY}\"}}");
        create_database(&windsurf, &[(AUTH_STATUS_KEY, &value)])?;

        let found =
            find_api_key(&candidate_paths(home.path())).map_err(|a| anyhow::anyhow!("{a:?}"))?;
        assert_eq!(found.source, windsurf);
        assert_eq!(found.api_key, SAMPLE_KEY);
        assert!(!format!("{found:?}").contains(SAMPLE_KEY));

        let missing = tempfile::tempdir()?;
        let attempts = find_api_key(&candidate_paths(missing.path()))
            .err()
            .context("should fail")?;
        assert_eq!(attempts.len(), CANDIDATE_RELATIVE_PATHS.len());
        assert!(
            attempts[0].contains(
                &devin
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            )
        );
        Ok(())
    }

    #[test]
    fn masks_api_key() {
        assert_eq!(mask_api_key(SAMPLE_KEY), "devin-…cdef");
        assert_eq!(mask_api_key("short"), "*****");
        assert_eq!(mask_api_key(""), "");
        assert!(!mask_api_key(SAMPLE_KEY).contains("0123456789abcdef0123"));
    }

    #[test]
    fn locates_cursor_in_value() {
        let text = "{\n  \"fork\": {\n    \"windsurf\": {\n      \"api_key\": \"\"\n    }\n  }\n}";
        assert_eq!(api_key_value_position(text), Some(Point::new(3, 18)));
        let null_value = "{\"fork\":{\"windsurf\":{\"api_key\": null}}}";
        assert_eq!(api_key_value_position(null_value), Some(Point::new(0, 30)));
        assert_eq!(api_key_value_position("{}"), None);
    }

    #[gpui::test]
    async fn import_keeps_comments_and_other_fields(cx: &mut TestAppContext) {
        check_import_keeps_comments(cx)
            .await
            .expect("import should succeed");
    }

    async fn check_import_keeps_comments(cx: &mut TestAppContext) -> Result<()> {
        let fs = fs::FakeFs::new(cx.executor());
        fs.create_dir(paths::config_dir()).await?;
        let original = "// my settings\n{\n  // keep this\n  \"tab_size\": 2\n}\n";
        fs.insert_file(paths::settings_file(), original.as_bytes().to_vec())
            .await;
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            <dyn fs::Fs>::set_global(fs.clone() as Arc<dyn fs::Fs>, cx);
        });

        let completion = cx.update(|cx| write_api_key(SAMPLE_KEY.to_string(), cx));
        cx.run_until_parked();
        completion.await??;

        let saved = fs.load(paths::settings_file()).await?;
        assert!(saved.contains("// my settings"), "{saved}");
        assert!(saved.contains("// keep this"), "{saved}");
        assert!(saved.contains("\"tab_size\": 2"), "{saved}");
        assert!(
            saved.contains(&format!("\"api_key\": \"{SAMPLE_KEY}\"")),
            "{saved}"
        );
        Ok(())
    }
}
