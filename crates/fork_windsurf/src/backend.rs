//! What the delegate needs from a way of reaching Windsurf: the collected
//! context goes in, the accumulated `GetTab` result comes out. The native
//! client implements it; the language server bridge (stage 8) would too.

use std::time::Duration;

use futures::future::BoxFuture;

use crate::{
    native::{connect::ConnectError, get_tab::TabStream},
    proto::exa::{
        api_server_pb::GetTabRequest,
        codeium_common_pb::{CodeDiagnostic, Language},
        cortex_pb::CortexTrajectoryStep,
    },
};

/// The current document and cursor, see `context.rs`.
#[derive(Clone, Debug)]
pub struct DocumentContext {
    pub text: String,
    pub absolute_uri: String,
    pub workspace_uri: String,
    pub language: Language,
    pub editor_language: String,
    /// UTF-8 byte offset of the cursor in `text`.
    pub cursor_offset: usize,
    pub selection: String,
    /// 0-based, end exclusive.
    pub selection_start_line: u32,
    pub selection_end_line: u32,
    pub is_dirty: bool,
}

#[derive(Clone, Debug)]
pub struct TabRequestInput {
    pub document: DocumentContext,
    pub diagnostics: Vec<CodeDiagnostic>,
    pub steps: Vec<CortexTrajectoryStep>,
    /// The session's `USER_MAINLINE` trajectory id; the server keys its cache on it.
    pub mainline_trajectory_id: String,
}

pub struct TabExchange {
    /// The request as sent, for the debug view. `None` when no request was
    /// sent, e.g. because the token couldn't be fetched.
    pub request: Option<GetTabRequest>,
    pub result: Result<TabStream, ConnectError>,
    pub elapsed: Duration,
    pub first_delta_after: Option<Duration>,
}

pub trait TabBackend: Send + Sync {
    fn get_tab(&self, api_key: String, input: TabRequestInput) -> BoxFuture<'static, TabExchange>;

    /// Drops cached tokens, e.g. after the API key changed.
    fn forget_credentials(&self) -> BoxFuture<'static, ()>;
}
