//! `CLIPBOARD` trajectory steps. The official client adds the clipboard to a
//! request when the text was copied within the last five seconds, is at most
//! 1000 characters and doesn't look like a password (`fork/spike/结论.md`
//! 问题 9).
//!
//! Copies are observed with `cx.observe_keystrokes`, which runs after the
//! keystroke's action was handled, so the clipboard already holds the copied
//! text: any action named `Copy`, `Cut` or `CopyAndTrim` (editor, terminal,
//! …) records the clipboard at that moment. Copies that don't go through a
//! key binding (menu clicks, other applications) are picked up when the text
//! is pasted into Zed: on a `Paste` action, a clipboard that differs from the
//! last recorded copy counts as copied then. The system clipboard is never
//! polled per request.

use std::time::{Duration, Instant, SystemTime};

use gpui::{App, Entity, Subscription};

use crate::{
    Windsurf,
    native::timestamp,
    proto::exa::cortex_pb::{
        CortexStepClipboard, CortexStepStatus, CortexStepType, CortexTrajectoryStep,
        cortex_trajectory_step,
    },
    trajectory::step_metadata,
};

/// How long after a copy the clipboard is still added to a request.
pub const CLIPBOARD_WINDOW: Duration = Duration::from_secs(5);
pub const MAX_CLIPBOARD_CHARS: usize = 1000;

const COPY_ACTIONS: &[&str] = &["Copy", "Cut", "CopyAndTrim"];
const PASTE_ACTIONS: &[&str] = &["Paste"];

/// A copy that hasn't been sent yet.
pub struct PendingCopy {
    pub content: String,
    pub copied_at: Instant,
    /// Where in the trajectory the step goes: after the steps that were
    /// committed when the text was copied.
    pub insert_at: usize,
}

pub enum CopyKind {
    Copy,
    Paste,
}

fn action_kind(action_name: &str) -> Option<CopyKind> {
    let short_name = action_name.rsplit("::").next().unwrap_or(action_name);
    if COPY_ACTIONS.contains(&short_name) {
        Some(CopyKind::Copy)
    } else if PASTE_ACTIONS.contains(&short_name) {
        Some(CopyKind::Paste)
    } else {
        None
    }
}

pub fn observe_copies(windsurf: Entity<Windsurf>, cx: &mut App) -> Subscription {
    cx.observe_keystrokes(move |event, _window, cx| {
        let Some(kind) = event
            .action
            .as_ref()
            .and_then(|action| action_kind(action.name()))
        else {
            return;
        };
        if !windsurf.read(cx).can_request(cx) {
            return;
        }
        let Some(content) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        windsurf.update(cx, |windsurf, cx| match kind {
            CopyKind::Copy => windsurf.record_copy(content, cx),
            CopyKind::Paste => windsurf.record_paste(content, cx),
        });
    })
}

/// Whether the clipboard may be sent: not empty, not too long and not
/// something that looks like a password or a key.
pub fn is_sendable(content: &str) -> bool {
    !content.trim().is_empty()
        && content.chars().count() <= MAX_CLIPBOARD_CHARS
        && !looks_like_secret(content)
}

/// A single token without whitespace that mixes at least three character
/// classes, or that starts like a well-known key or token format.
pub fn looks_like_secret(content: &str) -> bool {
    let trimmed = content.trim();
    const PREFIXES: &[&str] = &[
        "-----BEGIN",
        "sk-",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "eyJ",
        "devin-session-token",
        "cog_",
    ];
    if PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix)) {
        return true;
    }
    if trimmed.chars().any(char::is_whitespace) {
        return false;
    }
    let length = trimmed.chars().count();
    if !(8..=128).contains(&length) {
        return false;
    }
    let classes = [
        trimmed
            .chars()
            .any(|character| character.is_ascii_lowercase()),
        trimmed
            .chars()
            .any(|character| character.is_ascii_uppercase()),
        trimmed.chars().any(|character| character.is_ascii_digit()),
        trimmed
            .chars()
            .any(|character| !character.is_alphanumeric() && character != '_'),
    ];
    classes.iter().filter(|present| **present).count() >= 3
}

pub fn clipboard_step(content: &str, created_at: SystemTime) -> CortexTrajectoryStep {
    let mut metadata = step_metadata(created_at);
    metadata.completed_at = Some(timestamp(created_at));
    CortexTrajectoryStep {
        r#type: CortexStepType::Clipboard as i32,
        status: CortexStepStatus::Done as i32,
        metadata: Some(metadata),
        step: Some(cortex_trajectory_step::Step::Clipboard(
            CortexStepClipboard {
                content: content.to_string(),
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_kinds() {
        assert!(matches!(action_kind("editor::Copy"), Some(CopyKind::Copy)));
        assert!(matches!(action_kind("editor::Cut"), Some(CopyKind::Copy)));
        assert!(matches!(
            action_kind("terminal::Copy"),
            Some(CopyKind::Copy)
        ));
        assert!(matches!(
            action_kind("editor::Paste"),
            Some(CopyKind::Paste)
        ));
        assert!(action_kind("editor::CopyPath").is_none());
        assert!(action_kind("editor::Newline").is_none());
    }

    #[test]
    fn test_sendable_clipboard() {
        assert!(is_sendable("    print(w.length())"));
        assert!(is_sendable("order_total"));
        assert!(is_sendable("fooBarBaz"));
        assert!(is_sendable(&"x".repeat(MAX_CLIPBOARD_CHARS)));
        assert!(!is_sendable(&"x".repeat(MAX_CLIPBOARD_CHARS + 1)));
        assert!(!is_sendable("   \n"));
        assert!(!is_sendable("Tr0ub4dor&3"));
        assert!(!is_sendable("hunter2Password!"));
        assert!(!is_sendable("sk-abc"));
        assert!(!is_sendable("-----BEGIN OPENSSH PRIVATE KEY-----\nabc"));
        assert!(!is_sendable("eyJhbGciOiJIUzI1NiJ9.e30.sig"));
        // Code with spaces is fine even with mixed classes.
        assert!(is_sendable("let x = Foo::new(1);"));
    }
}
