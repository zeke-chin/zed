//! Settings for the personal fork's features (the top-level `fork` key).
//!
//! This file only exists in the fork, so it never conflicts with upstream.
//! Defaults live in each feature crate's `Settings::from_settings`, not in
//! `assets/settings/default.json`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ForkSettingsContent {
    /// Which fork-provided edit prediction provider to use. Takes precedence over
    /// `edit_predictions.provider`, which should be set to `"none"` while a fork
    /// provider is active.
    ///
    /// Default: windsurf
    pub edit_prediction_provider: Option<ForkEditPredictionProvider>,
    pub copy_anchor: Option<CopyAnchorSettingsContent>,
    pub json_explorer: Option<JsonExplorerSettingsContent>,
    pub windsurf: Option<WindsurfSettingsContent>,
    pub git_panel: Option<ForkGitPanelSettingsContent>,
    pub markdown_preview: Option<ForkMarkdownPreviewSettingsContent>,
    pub blame_popover: Option<ForkBlamePopoverSettingsContent>,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ForkMarkdownPreviewSettingsContent {
    /// Which typography the markdown preview uses.
    ///
    /// Default: zed
    pub style: Option<ForkMarkdownPreviewStyle>,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ForkMarkdownPreviewStyle {
    /// Upstream typography.
    #[default]
    Zed,
    /// Vditor's typography, as used by the cweijan/vscode-office extension.
    Vditor,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ForkEditPredictionProvider {
    /// Don't use a fork provider; `edit_predictions.provider` applies.
    None,
    /// Windsurf (Devin) Tab completions.
    #[default]
    Windsurf,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct CopyAnchorSettingsContent {
    /// Whether Copy Anchor is enabled at all. When disabled, the status bar items
    /// are hidden and copy falls back to the native behavior.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Whether copying includes the file path and line numbers (`true`) or is a
    /// plain copy (`false`).
    ///
    /// Default: true
    pub context_mode: Option<bool>,
    /// Maximum number of characters shown in the copy preview in the status bar.
    ///
    /// Default: 30
    pub preview_max_length: Option<usize>,
    /// How long the copy preview stays in the status bar, in seconds. 0 disables it.
    ///
    /// Default: 1
    pub preview_duration: Option<f32>,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct JsonExplorerSettingsContent {
    /// Whether JSON Explorer is enabled.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Whether pasted Python repr text (dicts, lists, ...) is converted to JSON
    /// immediately instead of only showing a conversion hint.
    ///
    /// Default: false
    pub python_repr_auto_convert: Option<bool>,
    /// Whether pasted JSON containing raw line breaks inside strings is repaired
    /// immediately instead of only showing a conversion hint.
    ///
    /// Default: false
    pub line_break_repair_auto_convert: Option<bool>,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct WindsurfSettingsContent {
    /// Whether Windsurf edit predictions are enabled.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// How to reach the Windsurf service.
    ///
    /// Default: native
    pub backend: Option<WindsurfBackend>,
    /// The Windsurf (Devin) API key. Stored in plain text: remove it before
    /// sharing this file. Only read from user settings.
    ///
    /// Default: null
    pub api_key: Option<String>,
    /// Whether to show predictions that jump to a different location.
    ///
    /// Default: true
    pub tab_jump: Option<bool>,
    /// Path to the official language server binary, used by the
    /// `language_server` backend. When null, common install locations are searched.
    ///
    /// Default: null
    pub language_server_path: Option<String>,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum WindsurfBackend {
    /// Talk to the Windsurf cloud directly from Zed.
    #[default]
    Native,
    /// Go through the official Windsurf language server binary.
    LanguageServer,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ForkGitPanelSettingsContent {
    /// How the Git panel arranges the commit editor, the changes list and the
    /// commit history.
    ///
    /// Default: tabs
    pub layout: Option<ForkGitPanelLayout>,
    /// Whether the history in the VS Code layout shows tag badges.
    ///
    /// Default: true
    pub show_tags: Option<bool>,
    /// Whether the history in the VS Code layout shows remote branch badges.
    ///
    /// Default: true
    pub show_remote_branches: Option<bool>,
    /// How many commits the history in the VS Code layout shows at first, and
    /// how many more each "Show More" adds.
    ///
    /// Default: 50
    pub graph_page_size: Option<usize>,
    /// Whether the history in the VS Code layout loads the next page when it
    /// is scrolled to the end, instead of showing a "Show More" button.
    ///
    /// Default: false
    pub graph_page_on_scroll: Option<bool>,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ForkGitPanelLayout {
    /// Upstream layout: Changes and History tabs, commit editor at the bottom.
    #[default]
    Tabs,
    /// VS Code style: commit editor at the top, changes in the middle and the
    /// commit history below them, all visible at once.
    Vscode,
}

#[with_fallible_options]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ForkBlamePopoverSettingsContent {
    /// Which layout the inline blame hover uses.
    ///
    /// Default: zed
    pub style: Option<ForkBlamePopoverStyle>,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, MergeFrom,
)]
#[serde(rename_all = "snake_case")]
pub enum ForkBlamePopoverStyle {
    #[default]
    Zed,
    Gitlens,
}
