//! What the delegate needs from a way of reaching Windsurf: the collected
//! context goes in, the accumulated `GetTab` result comes out. The native
//! client implements it; the language server bridge (stage 8) would too.

use std::time::Duration;

use futures::{FutureExt as _, future::BoxFuture};

use crate::{
    native::{connect::ConnectError, get_chat_message::ChatExchange, get_tab::TabStream},
    proto::exa::{
        api_server_pb::{GetChatMessageRequest, GetTabRequest},
        codeium_common_pb::{CodeDiagnostic, Language},
        cortex_pb::CortexTrajectoryStep,
        prompt_pb::SupercompleteFeedback,
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
    /// Row and column (in characters) of the cursor. Only set for predictive
    /// requests, like the official language server does.
    pub cursor_position: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub struct TabRequestInput {
    pub document: DocumentContext,
    pub diagnostics: Vec<CodeDiagnostic>,
    pub steps: Vec<CortexTrajectoryStep>,
    /// The session's `USER_MAINLINE` trajectory id; the server keys its cache on it.
    pub mainline_trajectory_id: String,
    /// Server hashes of `steps[..cached_hashes.len()]` from an earlier
    /// response. When not empty, only the steps after them are sent.
    pub cached_hashes: Vec<u64>,
    /// `supercomplete_feedback_context.feedback`.
    pub feedback: Vec<SupercompleteFeedback>,
    /// `metadata.trigger_id`; a predictive request reuses its trigger's id.
    pub trigger_id: String,
}

pub struct TabExchange {
    /// The request as sent, for the debug view. `None` when no request was
    /// sent, e.g. because the token couldn't be fetched.
    pub request: Option<GetTabRequest>,
    pub result: Result<TabStream, ConnectError>,
    pub elapsed: Duration,
    pub first_delta_after: Option<Duration>,
    /// Encoded size of the request that produced `result`.
    pub request_bytes: usize,
    /// The server had lost the trajectory cache and the full trajectory was sent again.
    pub resent_full_trajectory: bool,
}

/// What `ProvideFeedback` reports about a shown prediction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedbackReport {
    pub prompt_id: String,
    pub completion_id: String,
    pub accepted: bool,
    /// The prediction came from a predictive request rather than the
    /// request of the trigger that showed it.
    pub from_cache: bool,
    pub feedback_delay_ms: i64,
    pub timestamp_unix_ms: i64,
}

pub trait TabBackend: Send + Sync {
    fn get_tab(&self, api_key: String, input: TabRequestInput) -> BoxFuture<'static, TabExchange>;

    /// Sends a Tab Jump request, see `tab_jump.rs`.
    fn get_chat_message(
        &self,
        _api_key: String,
        _request: GetChatMessageRequest,
    ) -> BoxFuture<'static, ChatExchange> {
        futures::future::ready(ChatExchange {
            request: None,
            result: Err(ConnectError::Protocol(
                "this backend doesn't support Tab Jump".to_string(),
            )),
            elapsed: Duration::ZERO,
            first_delta_after: None,
        })
        .boxed()
    }

    /// Drops cached tokens, e.g. after the API key changed.
    fn forget_credentials(&self) -> BoxFuture<'static, ()>;

    fn provide_feedback(
        &self,
        api_key: String,
        report: FeedbackReport,
    ) -> BoxFuture<'static, Result<(), ConnectError>>;
}
