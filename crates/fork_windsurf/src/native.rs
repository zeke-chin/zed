//! Talks to the Windsurf cloud directly, without the official language server.

pub mod auth;
pub mod connect;
pub mod get_chat_message;
pub mod get_tab;

use std::{
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use futures::{FutureExt as _, future::BoxFuture};
use http_client::HttpClient;
use sha2::{Digest as _, Sha512};

use crate::{
    backend::{FeedbackReport, TabBackend, TabExchange, TabRequestInput},
    proto::{
        exa::{
            api_server_pb::{
                FeedbackExtraInfo, FeedbackType, GetChatMessageRequest, GetTabRequest,
                ProvideFeedbackRequest, ProvideFeedbackResponse,
            },
            codeium_common_pb::{CompletionSource, Metadata, Model, ProviderSource},
        },
        google::protobuf::Timestamp,
    },
};
use auth::Auth;
use connect::{ConnectClient, ConnectError, Timer};
use get_chat_message::ChatExchange;
use get_tab::TabStream;

/// Client identity and endpoints. These mirror what the official client sends
/// (Devin 3.10.48 with language server 1.48.2, see `fork/spike/结论.md`问题 3–5, 14)
/// and are kept in one place so that a model or endpoint change on the server
/// side only needs to be applied here.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub api_server_url: String,
    pub inference_server_url: String,
    pub model: Model,
    /// `TAB_JUMP_MODEL_CONFIG`.
    pub tab_jump_model: Model,
    pub ide_name: &'static str,
    pub ide_version: &'static str,
    pub extension_name: &'static str,
    pub extension_version: &'static str,
    pub extension_path: &'static str,
    pub locale: &'static str,
    pub plan_name: &'static str,
    pub unary_timeout: Duration,
    pub get_tab_timeout: Duration,
    pub get_chat_message_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            api_server_url: "https://server.codeium.com".to_string(),
            inference_server_url: "https://inference-us-west-lb.codeium.com".to_string(),
            model: Model::TabExperimental4,
            tab_jump_model: Model::Chat20706,
            ide_name: "windsurf",
            ide_version: "3.10.48",
            extension_name: "windsurf",
            extension_version: "1.48.2",
            extension_path: "/Applications/Devin.app/Contents/Resources/app/extensions/windsurf",
            locale: "en",
            plan_name: "Unset",
            unary_timeout: Duration::from_secs(10),
            get_tab_timeout: Duration::from_millis(9300),
            get_chat_message_timeout: Duration::from_millis(9992),
        }
    }
}

pub const GET_USER_JWT_PATH: &str = "/exa.auth_pb.AuthService/GetUserJwt";
pub const GET_TAB_PATH: &str = "/exa.api_server_pb.ApiServerService/GetTab";
pub const PROVIDE_FEEDBACK_PATH: &str = "/exa.api_server_pb.ApiServerService/ProvideFeedback";

static SESSION_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().to_string());
static REQUEST_ID: AtomicU64 = AtomicU64::new(0);

/// Same construction as the official extension when no hardware serial is
/// available: sha512 over the sorted `macs`, `serial` and `username` values.
static DEVICE_FINGERPRINT: LazyLock<String> = LazyLock::new(|| {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let digest = Sha512::digest(username.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
});

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    }
}

pub fn timestamp(time: SystemTime) -> Timestamp {
    let since_epoch = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        seconds: since_epoch.as_secs() as i64,
        nanos: since_epoch.subsec_nanos() as i32,
    }
}

pub fn metadata(
    config: &ClientConfig,
    api_key: &str,
    user_jwt: Option<&str>,
    trigger_id: Option<&str>,
) -> Metadata {
    Metadata {
        ide_name: config.ide_name.to_string(),
        ide_version: config.ide_version.to_string(),
        extension_name: config.extension_name.to_string(),
        extension_version: config.extension_version.to_string(),
        extension_path: config.extension_path.to_string(),
        api_key: api_key.to_string(),
        user_jwt: user_jwt.unwrap_or_default().to_string(),
        locale: config.locale.to_string(),
        os: os_name().to_string(),
        session_id: SESSION_ID.clone(),
        request_id: REQUEST_ID.fetch_add(1, Ordering::Relaxed) + 1,
        ls_timestamp: Some(timestamp(SystemTime::now())),
        device_fingerprint: DEVICE_FINGERPRINT.clone(),
        trigger_id: trigger_id.unwrap_or_default().to_string(),
        plan_name: config.plan_name.to_string(),
        ..Default::default()
    }
}

/// The direct-to-cloud implementation of [`TabBackend`].
#[derive(Clone)]
pub struct NativeBackend {
    connect: Arc<ConnectClient>,
    config: Arc<ClientConfig>,
    auth: Arc<Auth>,
}

impl NativeBackend {
    pub fn new(http: Arc<dyn HttpClient>, timer: Timer, config: ClientConfig) -> Self {
        Self {
            connect: Arc::new(ConnectClient::new(http, timer)),
            config: Arc::new(config),
            auth: Arc::new(Auth::default()),
        }
    }

    async fn exchange(self, api_key: String, input: TabRequestInput) -> TabExchange {
        let started_at = Instant::now();
        let user_jwt = match self
            .auth
            .user_jwt(&self.connect, &self.config, &api_key)
            .await
        {
            Ok(user_jwt) => user_jwt,
            Err(error) => {
                return TabExchange {
                    request: None,
                    result: Err(error),
                    elapsed: started_at.elapsed(),
                    first_delta_after: None,
                    request_bytes: 0,
                    resent_full_trajectory: false,
                    described_request: None,
                };
            }
        };
        let full_steps = (!input.cached_hashes.is_empty()).then(|| input.steps.clone());
        let metadata = metadata(
            &self.config,
            &api_key,
            Some(&user_jwt),
            Some(&input.trigger_id),
        );
        let mut request = get_tab::build_request(&self.config, metadata, input);
        let mut resent_full_trajectory = false;
        loop {
            let (result, first_delta_after) = self.send_get_tab(&request).await;
            let missing_information = matches!(&result, Ok(stream) if stream.missing_information);
            if missing_information
                && !resent_full_trajectory
                && get_tab::uses_cached_hashes(&request)
                && let Some(steps) = full_steps.clone()
            {
                log::info!(
                    "windsurf: the server doesn't know the cached trajectory hashes; resending the full trajectory"
                );
                request = get_tab::with_full_trajectory(request, steps);
                resent_full_trajectory = true;
                continue;
            }
            let result = match result {
                Ok(_) if missing_information => {
                    log::warn!(
                        "windsurf: the server reported missing trajectory information for a full trajectory; giving up on this request"
                    );
                    Err(ConnectError::Protocol(
                        "server reported missing trajectory information for a full trajectory"
                            .to_string(),
                    ))
                }
                Ok(stream) => Ok(stream),
                Err(error) => {
                    if matches!(error, ConnectError::Unauthenticated(_)) {
                        self.auth.clear().await;
                    }
                    Err(error)
                }
            };
            let request_bytes = prost::Message::encoded_len(&request);
            return TabExchange {
                request: Some(request),
                result,
                elapsed: started_at.elapsed(),
                first_delta_after,
                request_bytes,
                resent_full_trajectory,
                described_request: None,
            };
        }
    }

    async fn send_get_tab(
        &self,
        request: &GetTabRequest,
    ) -> (Result<TabStream, ConnectError>, Option<Duration>) {
        let url = format!("{}{}", self.config.inference_server_url, GET_TAB_PATH);
        let sent_at = Instant::now();
        let mut stream = TabStream::default();
        let mut first_delta_after = None;
        let result = self
            .connect
            .server_stream(&url, request, self.config.get_tab_timeout, |response| {
                if stream.push(response) && first_delta_after.is_none() {
                    first_delta_after = Some(sent_at.elapsed());
                }
            })
            .await;
        (result.map(|()| stream), first_delta_after)
    }

    async fn send_feedback(
        self,
        api_key: String,
        report: FeedbackReport,
    ) -> Result<(), ConnectError> {
        let user_jwt = self
            .auth
            .user_jwt(&self.connect, &self.config, &api_key)
            .await?;
        let request = ProvideFeedbackRequest {
            metadata: Some(metadata(&self.config, &api_key, Some(&user_jwt), None)),
            prompt_id: report.prompt_id,
            completion_id: report.completion_id,
            source: if report.from_cache {
                CompletionSource::Cache
            } else {
                CompletionSource::Network
            } as i32,
            feedback: if report.accepted {
                FeedbackType::Accept
            } else {
                FeedbackType::Reject
            } as i32,
            extra_info: if report.accepted {
                FeedbackExtraInfo::Unspecified
            } else {
                FeedbackExtraInfo::IntentionalReject
            } as i32,
            timestamp_unix_ms: report.timestamp_unix_ms,
            feedback_delay_ms: report.feedback_delay_ms,
            provider_source: ProviderSource::Supercomplete as i32,
            ..Default::default()
        };
        let url = format!("{}{}", self.config.api_server_url, PROVIDE_FEEDBACK_PATH);
        self.connect
            .unary::<_, ProvideFeedbackResponse>(&url, &request, self.config.unary_timeout)
            .await
            .map(|_response| ())
    }
}

impl TabBackend for NativeBackend {
    fn get_tab(&self, api_key: String, input: TabRequestInput) -> BoxFuture<'static, TabExchange> {
        self.clone().exchange(api_key, input).boxed()
    }

    fn get_chat_message(
        &self,
        api_key: String,
        request: GetChatMessageRequest,
    ) -> BoxFuture<'static, ChatExchange> {
        self.clone().chat_exchange(api_key, request).boxed()
    }

    fn forget_credentials(&self) -> BoxFuture<'static, ()> {
        let auth = self.auth.clone();
        async move { auth.clear().await }.boxed()
    }

    fn provide_feedback(
        &self,
        api_key: String,
        report: FeedbackReport,
    ) -> BoxFuture<'static, Result<(), ConnectError>> {
        self.clone().send_feedback(api_key, report).boxed()
    }
}
