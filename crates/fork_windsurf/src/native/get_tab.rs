//! Assembles `GetTabRequest` from the collected context and accumulates the
//! streamed `GetTabResponse` messages. Field values follow the official
//! language server, see `fork/spike/结论.md` 问题 7–9, 16–17.

use std::time::SystemTime;

use super::{ClientConfig, timestamp};
use crate::{
    backend::TabRequestInput,
    proto::exa::{
        api_server_pb::{GetTabRequest, GetTabResponse, delta_tab, get_tab_response},
        codeium_common_pb::{CommandRequestSource, Document, Metadata, ProviderSource, StopReason},
        cortex_pb::{
            CortexTrajectory, CortexTrajectoryMetadata, CortexTrajectoryReference,
            CortexTrajectoryType,
        },
        prompt_pb::{CommandIntentInfo, SupercompleteFeedbackContext, UnifiedPromptComponents},
    },
};

pub const INSTRUCTION: &str = "Please modify the following mentioned code block with the logical next edit. The <|cursor|> in the code block represents where my cursor is.";
const STREAMING_COMPLETION_VERSION: i32 = 2;

pub fn build_request(
    config: &ClientConfig,
    metadata: Metadata,
    input: TabRequestInput,
) -> GetTabRequest {
    let now = timestamp(SystemTime::now());
    let document = input.document;
    let input_document = Document {
        absolute_uri: document.absolute_uri,
        workspace_uri: document.workspace_uri,
        text: document.text,
        editor_language: document.editor_language,
        language: document.language as i32,
        cursor_offset: document.cursor_offset as u64,
        line_ending: "\n".to_string(),
        timestamp: Some(now),
        is_dirty: document.is_dirty,
        ..Default::default()
    };
    GetTabRequest {
        metadata: Some(metadata),
        unified_prompt_components: Some(UnifiedPromptComponents {
            cortex_trajectory: Some(CortexTrajectory {
                trajectory_id: uuid::Uuid::new_v4().to_string(),
                trajectory_type: CortexTrajectoryType::Supercomplete as i32,
                steps: input.steps,
                parent_references: vec![CortexTrajectoryReference {
                    trajectory_id: input.mainline_trajectory_id,
                    trajectory_type: CortexTrajectoryType::UserMainline as i32,
                    step_index: -1,
                    ..Default::default()
                }],
                metadata: Some(CortexTrajectoryMetadata {
                    created_at: Some(now),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            command_intent_info: Some(CommandIntentInfo {
                selection: document.selection,
                instruction: INSTRUCTION.to_string(),
                input_document: Some(input_document),
                selection_start_line: u64::from(document.selection_start_line),
                selection_end_line: u64::from(document.selection_end_line),
                request_source: CommandRequestSource::Supercomplete as i32,
                ..Default::default()
            }),
            code_diagnostics: input.diagnostics,
            supercomplete_feedback_context: Some(SupercompleteFeedbackContext::default()),
            ..Default::default()
        }),
        provider_source: ProviderSource::Supercomplete as i32,
        prompt_id: uuid::Uuid::new_v4().to_string(),
        language: document.language as i32,
        model: config.model as i32,
        streaming_completion_version: Some(STREAMING_COMPLETION_VERSION),
        ..Default::default()
    }
}

/// The streamed `GetTab` result, accumulated in arrival order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStream {
    /// Absolute path the deltas refer to (no `file://`).
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub stop_reason: Option<String>,
    pub noop: bool,
    pub tab_jump: bool,
    pub missing_information: bool,
    pub request_id: String,
    pub deltas: usize,
}

impl TabStream {
    /// Returns whether the message carried a text delta.
    pub fn push(&mut self, response: GetTabResponse) -> bool {
        match response.message {
            Some(get_tab_response::Message::PreResponse(pre_response)) => {
                self.request_id = pre_response.request_id;
                false
            }
            Some(get_tab_response::Message::MissingInformation(_)) => {
                self.missing_information = true;
                false
            }
            Some(get_tab_response::Message::StreamingCompletionV2(delta)) => {
                if !delta.path.is_empty() {
                    self.path = delta.path;
                }
                if let Some(stop_reason) = delta.stop_reason {
                    self.stop_reason = Some(
                        StopReason::try_from(stop_reason)
                            .map(|reason| reason.as_str_name().to_string())
                            .unwrap_or_else(|_| stop_reason.to_string()),
                    );
                }
                match delta.delta {
                    Some(delta_tab::Delta::DeltaSupercomplete(supercomplete)) => {
                        let has_text = !supercomplete.delta_old_str.is_empty()
                            || !supercomplete.delta_new_str.is_empty();
                        self.old_text.push_str(&supercomplete.delta_old_str);
                        self.new_text.push_str(&supercomplete.delta_new_str);
                        if has_text {
                            self.deltas += 1;
                        }
                        has_text
                    }
                    Some(delta_tab::Delta::DeltaTabJump(_)) => {
                        self.tab_jump = true;
                        false
                    }
                    Some(delta_tab::Delta::DeltaNoop(_)) => {
                        self.noop = true;
                        false
                    }
                    None => false,
                }
            }
            Some(get_tab_response::Message::StreamingCompletion(_)) | None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{
        backend::DocumentContext,
        proto::exa::{
            api_server_pb::{DeltaTab, TrajectoryCacheInfoV2},
            codeium_common_pb::{Language, Model},
        },
    };

    fn delta(delta: delta_tab::Delta) -> GetTabResponse {
        GetTabResponse {
            message: Some(get_tab_response::Message::StreamingCompletionV2(DeltaTab {
                path: "/work/a.py".to_string(),
                delta: Some(delta),
                ..Default::default()
            })),
        }
    }

    fn supercomplete(old_text: &str, new_text: &str) -> GetTabResponse {
        delta(delta_tab::Delta::DeltaSupercomplete(
            delta_tab::DeltaSupercomplete {
                delta_old_str: old_text.to_string(),
                delta_new_str: new_text.to_string(),
                ..Default::default()
            },
        ))
    }

    #[test]
    fn test_stream_accumulates_deltas_in_order() {
        let mut stream = TabStream::default();
        assert!(stream.push(supercomplete("x = 1\ny", "")));
        assert!(!stream.push(GetTabResponse {
            message: Some(get_tab_response::Message::PreResponse(
                get_tab_response::PreResponse {
                    request_id: "request".to_string(),
                    cache_info_v2: Some(TrajectoryCacheInfoV2 {
                        cache_key: "mainline".to_string(),
                        hashes: vec![1, 2],
                    }),
                    ..Default::default()
                }
            )),
        }));
        assert!(stream.push(supercomplete("", "x = 1\n")));
        assert!(stream.push(supercomplete("", "y = 2")));
        let mut complete = supercomplete("", "");
        if let Some(get_tab_response::Message::StreamingCompletionV2(delta)) = &mut complete.message
        {
            delta.is_complete = true;
        }
        assert!(!stream.push(complete));
        assert!(!stream.push(GetTabResponse {
            message: Some(get_tab_response::Message::StreamingCompletionV2(DeltaTab {
                is_complete: true,
                stop_reason: Some(StopReason::StopPattern as i32),
                ..Default::default()
            })),
        }));
        assert_eq!(
            stream,
            TabStream {
                path: "/work/a.py".to_string(),
                old_text: "x = 1\ny".to_string(),
                new_text: "x = 1\ny = 2".to_string(),
                stop_reason: Some("STOP_REASON_STOP_PATTERN".to_string()),
                request_id: "request".to_string(),
                deltas: 3,
                ..Default::default()
            }
        );
    }

    #[test]
    fn test_stream_records_noop_jump_and_missing_information() {
        let mut stream = TabStream::default();
        stream.push(delta(delta_tab::Delta::DeltaNoop(
            delta_tab::DeltaNoop::default(),
        )));
        stream.push(delta(delta_tab::Delta::DeltaTabJump(
            delta_tab::DeltaTabJump::default(),
        )));
        stream.push(GetTabResponse {
            message: Some(get_tab_response::Message::MissingInformation(
                get_tab_response::MissingInformation::default(),
            )),
        });
        assert!(stream.noop && stream.tab_jump && stream.missing_information);
        assert_eq!(stream.new_text, "");
    }

    #[test]
    fn test_request_has_required_fields() {
        let config = ClientConfig::default();
        let input = TabRequestInput {
            document: DocumentContext {
                text: "a\nb".to_string(),
                absolute_uri: "file:///work/a.py".to_string(),
                workspace_uri: "file:///work".to_string(),
                language: Language::Python,
                editor_language: "python".to_string(),
                cursor_offset: 3,
                selection: "a\nb".to_string(),
                selection_start_line: 0,
                selection_end_line: 2,
                is_dirty: true,
            },
            diagnostics: Vec::new(),
            steps: vec![Default::default()],
            mainline_trajectory_id: "mainline".to_string(),
        };
        let metadata = super::super::metadata(&config, "key", Some("jwt"), Some("trigger"));
        let request = build_request(&config, metadata, input);
        assert_eq!(request.model, Model::TabExperimental4 as i32);
        assert_eq!(
            request.provider_source,
            ProviderSource::Supercomplete as i32
        );
        assert_eq!(request.streaming_completion_version, Some(2));
        assert!(request.experiment_config.is_none());
        assert!(!request.prompt_id.is_empty());
        let metadata = request.metadata.as_ref().expect("metadata");
        assert_eq!(
            (
                metadata.ide_name.as_str(),
                metadata.api_key.as_str(),
                metadata.user_jwt.as_str(),
                metadata.f.as_str()
            ),
            ("windsurf", "key", "jwt", "")
        );
        let components = request.unified_prompt_components.expect("components");
        assert!(components.chat_messages.is_empty());
        let trajectory = components.cortex_trajectory.expect("trajectory");
        assert_eq!(trajectory.steps.len(), 1);
        assert_eq!(
            trajectory.trajectory_type,
            CortexTrajectoryType::Supercomplete as i32
        );
        assert_eq!(trajectory.parent_references[0].trajectory_id, "mainline");
        assert_eq!(trajectory.parent_references[0].step_index, -1);
        let intent = components.command_intent_info.expect("intent");
        assert_eq!(
            intent.request_source,
            CommandRequestSource::Supercomplete as i32
        );
        assert_eq!(
            (intent.selection_start_line, intent.selection_end_line),
            (0, 2)
        );
        let document = intent.input_document.expect("document");
        assert_eq!(
            (document.cursor_offset, document.line_ending.as_str()),
            (3, "\n")
        );
    }
}
