//! Sends the Tab Jump `GetChatMessage` (a Connect server stream, like
//! `GetTab`) and accumulates the streamed text. The prompt is built in
//! `tab_jump/prompt.rs`; this fills in what the client decides: metadata,
//! model and the experiments the official language server attaches.

use std::time::{Duration, Instant};

use super::{ClientConfig, NativeBackend, metadata};
use crate::{
    native::connect::ConnectError,
    proto::exa::{
        api_server_pb::{GetChatMessageRequest, GetChatMessageResponse},
        codeium_common_pb::{
            ExperimentConfig, ExperimentKey, ExperimentSource, ExperimentWithVariant, StopReason,
            experiment_with_variant::Payload,
        },
    },
    tab_jump::prompt::CONTEXT_TOKENS,
};

pub const GET_CHAT_MESSAGE_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";

const CUMULATIVE_PROMPT_CONFIG: &str = r#"{"persistent_context_multiplier": 0.25, "persistent_active_document_multiplier": 0.45, "persistent_open_docs_multiplier": 0.25, "persistent_max_tokens_per_open_doc": 2048, "persistent_max_ccis_considered": 25, "trajectory_context_multiplier": 0.5, "trajectory_refresh_threshold_multiplier": 0.9, "trajectory_truncation_multiplier": 0.5, "ephemeral_context_multiplier": 0.25, "intent_reservation_tokens": 512, "ephemeral_active_document_multiplier": 1.0, "ephemeral_max_ccis_considered": 0}"#;

/// The accumulated `GetChatMessage` stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatStream {
    pub text: String,
    pub stop_reason: Option<String>,
    pub message_id: String,
    pub deltas: usize,
}

impl ChatStream {
    /// Returns whether the message carried text.
    pub fn push(&mut self, response: GetChatMessageResponse) -> bool {
        if !response.message_id.is_empty() {
            self.message_id = response.message_id;
        }
        if response.stop_reason != 0 {
            self.stop_reason = Some(
                StopReason::try_from(response.stop_reason)
                    .map(|reason| reason.as_str_name().to_string())
                    .unwrap_or_else(|_| response.stop_reason.to_string()),
            );
        }
        if response.delta_text.is_empty() {
            return false;
        }
        self.text.push_str(&response.delta_text);
        self.deltas += 1;
        true
    }
}

pub struct ChatExchange {
    /// The request as sent; `None` when nothing was sent.
    pub request: Option<GetChatMessageRequest>,
    pub result: Result<ChatStream, ConnectError>,
    pub elapsed: Duration,
    pub first_delta_after: Option<Duration>,
}

fn experiment(key_string: String, payload: Payload) -> ExperimentWithVariant {
    ExperimentWithVariant {
        key_string,
        source: ExperimentSource::LanguageServer as i32,
        payload: Some(payload),
        ..Default::default()
    }
}

/// Adds metadata, the model and the experiments the official client sends
/// with Tab Jump requests.
pub fn finish_request(
    config: &ClientConfig,
    metadata: crate::proto::exa::codeium_common_pb::Metadata,
    mut request: GetChatMessageRequest,
) -> GetChatMessageRequest {
    let model_name = config.tab_jump_model.as_str_name();
    request.metadata = Some(metadata);
    request.internal_chat_model = config.tab_jump_model as i32;
    request.experiment_config = Some(ExperimentConfig {
        experiments: vec![
            experiment(
                format!("{model_name}_TOKENS"),
                Payload::String(CONTEXT_TOKENS.to_string()),
            ),
            experiment(
                "TAB_JUMP_CUMULATIVE_PROMPT_CONFIG".to_string(),
                Payload::Json(CUMULATIVE_PROMPT_CONFIG.to_string()),
            ),
            ExperimentWithVariant {
                key: ExperimentKey::TabJumpModelConfig as i32,
                ..experiment(
                    ExperimentKey::TabJumpModelConfig.as_str_name().to_string(),
                    Payload::Json(format!("{{\n  \"model_name\": \"{model_name}\"\n}}")),
                )
            },
        ],
        ..Default::default()
    });
    request
}

impl NativeBackend {
    pub(crate) async fn chat_exchange(
        self,
        api_key: String,
        request: GetChatMessageRequest,
    ) -> ChatExchange {
        let started_at = Instant::now();
        let user_jwt = match self
            .auth
            .user_jwt(&self.connect, &self.config, &api_key)
            .await
        {
            Ok(user_jwt) => user_jwt,
            Err(error) => {
                return ChatExchange {
                    request: None,
                    result: Err(error),
                    elapsed: started_at.elapsed(),
                    first_delta_after: None,
                };
            }
        };
        let trigger_id = uuid::Uuid::new_v4().to_string();
        let metadata = metadata(&self.config, &api_key, Some(&user_jwt), Some(&trigger_id));
        let request = finish_request(&self.config, metadata, request);
        let url = format!(
            "{}{}",
            self.config.inference_server_url, GET_CHAT_MESSAGE_PATH
        );
        let sent_at = Instant::now();
        let mut stream = ChatStream::default();
        let mut first_delta_after = None;
        let result = self
            .connect
            .server_stream(
                &url,
                &request,
                self.config.get_chat_message_timeout,
                |response| {
                    if stream.push(response) && first_delta_after.is_none() {
                        first_delta_after = Some(sent_at.elapsed());
                    }
                },
            )
            .await;
        let result = match result {
            Ok(()) => Ok(stream),
            Err(error) => {
                if matches!(error, ConnectError::Unauthenticated(_)) {
                    self.auth.clear().await;
                }
                Err(error)
            }
        };
        ChatExchange {
            request: Some(request),
            result,
            elapsed: started_at.elapsed(),
            first_delta_after,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::proto::exa::codeium_common_pb::{Model, ProviderSource};

    #[test]
    fn test_stream_accumulates_text_and_stop_reason() {
        let mut stream = ChatStream::default();
        for text in [" \"a", "<|cursor|>b\"", ""] {
            stream.push(GetChatMessageResponse {
                message_id: "bot-1".to_string(),
                delta_text: text.to_string(),
                ..Default::default()
            });
        }
        stream.push(GetChatMessageResponse {
            stop_reason: StopReason::StopPattern as i32,
            ..Default::default()
        });
        assert_eq!(
            stream,
            ChatStream {
                text: " \"a<|cursor|>b\"".to_string(),
                stop_reason: Some("STOP_REASON_STOP_PATTERN".to_string()),
                message_id: "bot-1".to_string(),
                deltas: 2,
            }
        );
    }

    #[test]
    fn test_request_gets_model_metadata_and_experiments() {
        let config = ClientConfig::default();
        let metadata = metadata(&config, "key", Some("jwt"), Some("trigger"));
        let request = finish_request(
            &config,
            metadata,
            GetChatMessageRequest {
                provider_source: ProviderSource::TabJump as i32,
                ..Default::default()
            },
        );
        assert_eq!(request.internal_chat_model, Model::Chat20706 as i32);
        assert_eq!(
            request
                .metadata
                .as_ref()
                .map(|metadata| metadata.f.as_str()),
            Some("")
        );
        let keys: Vec<&str> = request
            .experiment_config
            .as_ref()
            .expect("experiments")
            .experiments
            .iter()
            .map(|experiment| experiment.key_string.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![
                "MODEL_CHAT_20706_TOKENS",
                "TAB_JUMP_CUMULATIVE_PROMPT_CONFIG",
                "TAB_JUMP_MODEL_CONFIG"
            ]
        );
    }
}
