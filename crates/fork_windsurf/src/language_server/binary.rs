//! Finding the official language server binary and the versions of the app
//! it ships with. The binary is never downloaded: either the user points
//! `fork.windsurf.language_server_path` at it, or an installed Devin or
//! Windsurf app provides it (the same places `fork/spike/lib/common.py`
//! looks in).

use std::path::{Path, PathBuf};

const BINARY_PREFIX: &str = "language_server_";

/// Where the apps keep the binary, relative to their `app` directory.
const BIN_DIR_IN_APP_ROOT: &str = "extensions/windsurf/bin";

/// The `app` directories of installed apps, in order of preference.
fn app_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if cfg!(target_os = "macos") {
        for app in ["Devin", "Windsurf"] {
            roots.push(PathBuf::from(format!(
                "/Applications/{app}.app/Contents/Resources/app"
            )));
            roots.push(
                util::paths::home_dir()
                    .join(format!("Applications/{app}.app/Contents/Resources/app")),
            );
        }
    } else if cfg!(target_os = "linux") {
        for root in [
            "/usr/share/devin/resources/app",
            "/usr/share/windsurf/resources/app",
            "/opt/Devin/resources/app",
            "/opt/Windsurf/resources/app",
        ] {
            roots.push(PathBuf::from(root));
        }
    }
    roots
}

/// Binary names for this platform, the native architecture first.
fn binary_names() -> Vec<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => vec!["language_server_macos_arm", "language_server_macos_x64"],
        ("macos", _) => vec!["language_server_macos_x64"],
        ("linux", "aarch64") => vec!["language_server_linux_arm"],
        ("linux", _) => vec!["language_server_linux_x64"],
        ("windows", _) => vec!["language_server_windows_x64.exe"],
        _ => Vec::new(),
    }
}

/// The binaries to try when no path is configured, in order.
pub fn candidates() -> Vec<PathBuf> {
    let names = binary_names();
    let mut candidates = Vec::new();
    for root in app_roots() {
        for name in &names {
            candidates.push(root.join(BIN_DIR_IN_APP_ROOT).join(name));
        }
    }
    let codeium_bin = util::paths::home_dir().join(".codeium/windsurf/bin");
    for name in &names {
        candidates.push(codeium_bin.join(name));
    }
    candidates
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocateError {
    /// The configured path doesn't point at a file.
    ConfiguredPathMissing(PathBuf),
    /// Nothing configured and no installed app has the binary.
    NotInstalled,
}

impl std::fmt::Display for LocateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfiguredPathMissing(path) => write!(
                formatter,
                "`fork.windsurf.language_server_path` points at {}, which isn't a file",
                path.display()
            ),
            Self::NotInstalled => write!(
                formatter,
                "no Windsurf language server found; install Devin or Windsurf, or set `fork.windsurf.language_server_path`"
            ),
        }
    }
}

/// The configured binary, or the first installed one.
pub fn locate(configured: Option<&str>) -> Result<PathBuf, LocateError> {
    locate_in(configured, &candidates())
}

pub fn locate_in(configured: Option<&str>, candidates: &[PathBuf]) -> Result<PathBuf, LocateError> {
    if let Some(configured) = configured.map(str::trim).filter(|path| !path.is_empty()) {
        let path = expand_home(configured);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(LocateError::ConfiguredPathMissing(path))
        };
    }
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .ok_or(LocateError::NotInstalled)
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => util::paths::home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

/// The versions of the app a binary came with, read from its `product.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProductInfo {
    /// The `app` directory (`CODEIUM_EDITOR_APP_ROOT`), when the binary lives in an app.
    pub app_root: Option<PathBuf>,
    /// `windsurfVersion`, e.g. 3.10.48.
    pub windsurf_version: Option<String>,
    /// `codeiumVersion`, the language server version, e.g. 1.48.2.
    pub codeium_version: Option<String>,
}

pub fn product_info(binary: &Path) -> ProductInfo {
    let Some(app_root) = app_root_of(binary) else {
        return ProductInfo::default();
    };
    let product = std::fs::read_to_string(app_root.join("product.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let field = |name: &str| {
        product
            .as_ref()
            .and_then(|product| product.get(name))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    ProductInfo {
        windsurf_version: field("windsurfVersion"),
        codeium_version: field("codeiumVersion"),
        app_root: Some(app_root),
    }
}

/// `<app>/extensions/windsurf/bin/language_server_*` → `<app>`.
fn app_root_of(binary: &Path) -> Option<PathBuf> {
    let name = binary.file_name()?.to_str()?;
    if !name.starts_with(BINARY_PREFIX) {
        return None;
    }
    let bin = binary.parent()?;
    let extension = bin.parent()?;
    let extensions = extension.parent()?;
    let app_root = extensions.parent()?;
    let matches = bin.file_name()? == "bin"
        && extension.file_name()? == "windsurf"
        && extensions.file_name()? == "extensions";
    matches.then(|| app_root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_locate_prefers_the_configured_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let configured = directory.path().join("language_server_macos_arm");
        std::fs::write(&configured, b"").expect("write");
        let installed = directory.path().join("installed");
        std::fs::write(&installed, b"").expect("write");

        let configured_text = configured.to_string_lossy().into_owned();
        assert_eq!(
            locate_in(Some(&configured_text), std::slice::from_ref(&installed)),
            Ok(configured)
        );
        assert_eq!(
            locate_in(None, std::slice::from_ref(&installed)),
            Ok(installed.clone())
        );
        assert_eq!(
            locate_in(Some("  "), std::slice::from_ref(&installed)),
            Ok(installed)
        );

        let missing = directory.path().join("missing");
        let missing_text = missing.to_string_lossy().into_owned();
        assert_eq!(
            locate_in(Some(&missing_text), &[]),
            Err(LocateError::ConfiguredPathMissing(missing))
        );
        assert_eq!(locate_in(None, &[]), Err(LocateError::NotInstalled));
    }

    #[test]
    fn test_product_info_reads_the_app_versions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let app_root = directory.path().join("app");
        let bin = app_root.join("extensions/windsurf/bin");
        std::fs::create_dir_all(&bin).expect("create");
        std::fs::write(
            app_root.join("product.json"),
            r#"{"windsurfVersion": "3.10.48", "codeiumVersion": "1.48.2"}"#,
        )
        .expect("write");
        let binary = bin.join("language_server_macos_arm");
        assert_eq!(
            product_info(&binary),
            ProductInfo {
                app_root: Some(app_root),
                windsurf_version: Some("3.10.48".to_string()),
                codeium_version: Some("1.48.2".to_string()),
            }
        );
        assert_eq!(
            product_info(&directory.path().join("language_server_macos_arm")),
            ProductInfo::default()
        );
    }
}
