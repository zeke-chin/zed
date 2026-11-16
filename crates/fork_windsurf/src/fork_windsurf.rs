use gpui::App;
use settings::{RegisterSetting, Settings, SettingsContent, WindsurfBackend};

#[derive(Clone, PartialEq, RegisterSetting)]
pub struct WindsurfSettings {
    pub enabled: bool,
    pub backend: WindsurfBackend,
    /// Only ever comes from user settings: project settings files don't accept the
    /// `fork` key, so a key committed to a project repository is never read.
    pub api_key: Option<String>,
    pub tab_jump: bool,
    pub language_server_path: Option<String>,
}

impl Settings for WindsurfSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let windsurf = content
            .fork
            .as_ref()
            .and_then(|fork| fork.windsurf.as_ref());
        Self {
            enabled: windsurf
                .and_then(|windsurf| windsurf.enabled)
                .unwrap_or(true),
            backend: windsurf
                .and_then(|windsurf| windsurf.backend)
                .unwrap_or_default(),
            api_key: windsurf
                .and_then(|windsurf| windsurf.api_key.clone())
                .filter(|api_key| !api_key.trim().is_empty()),
            tab_jump: windsurf
                .and_then(|windsurf| windsurf.tab_jump)
                .unwrap_or(true),
            language_server_path: windsurf
                .and_then(|windsurf| windsurf.language_server_path.clone())
                .filter(|path| !path.trim().is_empty()),
        }
    }
}

impl std::fmt::Debug for WindsurfSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindsurfSettings")
            .field("enabled", &self.enabled)
            .field("backend", &self.backend)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("tab_jump", &self.tab_jump)
            .field("language_server_path", &self.language_server_path)
            .finish()
    }
}

pub fn init(_cx: &mut App) {}
