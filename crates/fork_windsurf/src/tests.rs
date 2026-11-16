//! End-to-end tests of the delegate against a fake Windsurf server.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate as _, EditPredictionDiscardReason,
    EditPredictionRequestTrigger, SuggestionDisplayType,
};
use fs::{FakeFs, Fs};
use futures::{AsyncReadExt as _, FutureExt as _, channel::oneshot};
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use http_client::{AsyncBody, FakeHttpClient, Request, Response};
use language::{Anchor, Buffer, Point, ToOffset as _, ToPoint as _};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use project::Project;
use serde_json::json;
use settings::{Settings as _, SettingsStore};
use util::path;

use crate::{
    ErrorStatus, Stats, Status, Windsurf, WindsurfDelegate, WindsurfSettings,
    delegate::DEBOUNCE,
    native::{
        ClientConfig, NativeBackend,
        auth::fake_token,
        connect::{FrameDecoder, encode_frame, gunzip, gzip},
    },
    proto::exa::{
        api_server_pb::{
            DeltaTab, GetTabRequest, GetTabResponse, ProvideFeedbackRequest,
            ProvideFeedbackResponse, TrajectoryCacheInfoV2, delta_tab, get_tab_response,
        },
        api_server_pb::{FeedbackExtraInfo, FeedbackType},
        auth_pb::GetUserJwtResponse,
        codeium_common_pb::{CompletionSource, StopReason},
        cortex_pb::{CortexStepType, CortexTrajectoryStep, cortex_trajectory_step},
    },
};

const API_KEY: &str = "devin-session-token$test-api-key";
const SOURCE: &str =
    "def area(width, height):\n    return width * height\n\n\ndef main():\n    print(area(2, 3))\n";

enum Reply {
    /// Rewrites the request's selection with the given function.
    Rewrite(Box<dyn Fn(&str) -> String + Send>),
    Noop,
    HttpStatus(u16),
    EndStreamError(&'static str),
    TransportError,
}

#[derive(Default)]
struct ServerState {
    jwt_calls: usize,
    /// Requests sent for triggers.
    tab_requests: Vec<GetTabRequest>,
    /// Predictive requests (they set `input_document.cursor_position`).
    predictive_requests: Vec<GetTabRequest>,
    feedback_requests: Vec<ProvideFeedbackRequest>,
    replies: Vec<Reply>,
    predictive_replies: Vec<Reply>,
    gate: Option<oneshot::Receiver<()>>,
    predictive_gate: Option<oneshot::Receiver<()>>,
    /// Answer the next requests that send cached hashes with `MissingInformation`.
    forget_hashes: usize,
    /// Answer the next requests with `MissingInformation`, hashes or not.
    always_missing: usize,
}

#[derive(Clone)]
struct FakeServer(Arc<Mutex<ServerState>>);

impl FakeServer {
    fn reply(&self, reply: Reply) {
        self.0.lock().replies.push(reply);
    }

    fn reply_to_predictive(&self, reply: Reply) {
        self.0.lock().predictive_replies.push(reply);
    }

    /// Holds the next `GetTab` response until the returned sender fires.
    fn hold_next_response(&self) -> oneshot::Sender<()> {
        let (sender, receiver) = oneshot::channel();
        self.0.lock().gate = Some(receiver);
        sender
    }

    fn hold_next_predictive_response(&self) -> oneshot::Sender<()> {
        let (sender, receiver) = oneshot::channel();
        self.0.lock().predictive_gate = Some(receiver);
        sender
    }

    fn tab_request_count(&self) -> usize {
        self.0.lock().tab_requests.len()
    }

    fn predictive_request_count(&self) -> usize {
        self.0.lock().predictive_requests.len()
    }

    fn last_predictive_request(&self) -> GetTabRequest {
        self.0
            .lock()
            .predictive_requests
            .last()
            .cloned()
            .expect("a predictive request should have been sent")
    }

    fn last_tab_request(&self) -> GetTabRequest {
        self.0
            .lock()
            .tab_requests
            .last()
            .cloned()
            .expect("a GetTab request should have been sent")
    }

    async fn handle(&self, request: Request<AsyncBody>) -> anyhow::Result<Response<AsyncBody>> {
        let path = request.uri().path().to_string();
        let mut body = Vec::new();
        request.into_body().read_to_end(&mut body).await?;
        if path.ends_with("/ProvideFeedback") {
            let request = <ProvideFeedbackRequest as prost::Message>::decode(body.as_slice())?;
            self.0.lock().feedback_requests.push(request);
            return Ok(Response::builder().status(200).body(AsyncBody::from(
                prost::Message::encode_to_vec(&ProvideFeedbackResponse::default()),
            ))?);
        }
        if path.ends_with("/GetUserJwt") {
            self.0.lock().jwt_calls += 1;
            let response = GetUserJwtResponse {
                user_jwt: fake_token(SystemTime::now() + Duration::from_secs(900)),
                ..Default::default()
            };
            return Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from(prost::Message::encode_to_vec(&response)))?);
        }
        anyhow::ensure!(path.ends_with("/GetTab"), "unexpected path {path}");
        let mut decoder = FrameDecoder::default();
        decoder.push(&body);
        let frame = decoder.next_frame()?.expect("request frame");
        let request =
            <GetTabRequest as prost::Message>::decode(gunzip(&frame.payload).await?.as_slice())?;
        let selection = request
            .unified_prompt_components
            .as_ref()
            .and_then(|components| components.command_intent_info.as_ref())
            .map(|intent| intent.selection.clone())
            .unwrap_or_default();
        let path = request
            .unified_prompt_components
            .as_ref()
            .and_then(|components| components.command_intent_info.as_ref())
            .and_then(|intent| intent.input_document.as_ref())
            .map(|document| {
                url::Url::parse(&document.absolute_uri)
                    .ok()
                    .and_then(|url| url.to_file_path().ok())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let predictive = request
            .unified_prompt_components
            .as_ref()
            .and_then(|components| components.command_intent_info.as_ref())
            .and_then(|intent| intent.input_document.as_ref())
            .is_some_and(|document| document.cursor_position.is_some());
        let sends_hashes = request
            .trajectory_steps_v2
            .as_ref()
            .is_some_and(|steps| !steps.hashes.is_empty());
        let step_count = request
            .unified_prompt_components
            .as_ref()
            .and_then(|components| components.cortex_trajectory.as_ref())
            .map(|trajectory| trajectory.steps.len())
            .unwrap_or_default()
            + request
                .trajectory_steps_v2
                .as_ref()
                .map(|steps| steps.hashes.len() + steps.steps_after_hashes.len())
                .unwrap_or_default();
        let (reply, gate, forget) = {
            let mut state = self.0.lock();
            let forget = if state.always_missing > 0 {
                state.always_missing -= 1;
                true
            } else if sends_hashes && state.forget_hashes > 0 {
                state.forget_hashes -= 1;
                true
            } else {
                false
            };
            if predictive {
                state.predictive_requests.push(request);
                let reply = if state.predictive_replies.is_empty() {
                    Reply::Noop
                } else {
                    state.predictive_replies.remove(0)
                };
                (reply, state.predictive_gate.take(), forget)
            } else {
                state.tab_requests.push(request);
                let reply = if forget || state.replies.is_empty() {
                    Reply::Noop
                } else {
                    state.replies.remove(0)
                };
                (reply, state.gate.take(), forget)
            }
        };
        if forget {
            return Ok(Response::builder().status(200).body(AsyncBody::from(
                [
                    message_frame(GetTabResponse {
                        message: Some(get_tab_response::Message::MissingInformation(
                            get_tab_response::MissingInformation {
                                uris: Vec::new(),
                                start_step: Some(0),
                            },
                        )),
                    })
                    .await?,
                    encode_frame(b"{}", false, true),
                ]
                .concat(),
            ))?);
        }
        // Like the real server: one hash per step that was sent.
        let pre_response = message_frame(GetTabResponse {
            message: Some(get_tab_response::Message::PreResponse(
                get_tab_response::PreResponse {
                    cache_info_v2: Some(TrajectoryCacheInfoV2 {
                        cache_key: "mainline".to_string(),
                        hashes: (1..=step_count as u64).map(|index| index * 1000).collect(),
                    }),
                    ..Default::default()
                },
            )),
        })
        .await?;
        if let Some(gate) = gate {
            gate.await?;
        }

        let frames = match reply {
            Reply::HttpStatus(status) => {
                return Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(String::new()))?);
            }
            Reply::TransportError => anyhow::bail!("connection reset"),
            Reply::Noop => vec![
                pre_response,
                message_frame(delta(
                    &path,
                    delta_tab::Delta::DeltaNoop(delta_tab::DeltaNoop::default()),
                ))
                .await?,
                encode_frame(b"{}", false, true),
            ],
            Reply::EndStreamError(code) => vec![encode_frame(
                format!(r#"{{"error":{{"code":"{code}","message":"from the fake server"}}}}"#)
                    .as_bytes(),
                false,
                true,
            )],
            Reply::Rewrite(rewrite) => {
                let new_text = rewrite(&selection);
                let middle = new_text.len() / 2;
                let split = (0..=middle)
                    .rev()
                    .find(|index| new_text.is_char_boundary(*index))
                    .unwrap_or(0);
                let mut frames = vec![
                    message_frame(supercomplete(&path, &selection, "")).await?,
                    pre_response,
                    message_frame(supercomplete(&path, "", &new_text[..split])).await?,
                    message_frame(supercomplete(&path, "", &new_text[split..])).await?,
                ];
                frames.push(
                    message_frame(GetTabResponse {
                        message: Some(get_tab_response::Message::StreamingCompletionV2(DeltaTab {
                            is_complete: true,
                            stop_reason: Some(StopReason::StopPattern as i32),
                            ..Default::default()
                        })),
                    })
                    .await?,
                );
                frames.push(encode_frame(b"{}", false, true));
                frames
            }
        };
        Ok(Response::builder()
            .status(200)
            .body(AsyncBody::from(frames.concat()))?)
    }
}

fn delta(path: &str, delta: delta_tab::Delta) -> GetTabResponse {
    GetTabResponse {
        message: Some(get_tab_response::Message::StreamingCompletionV2(DeltaTab {
            path: path.to_string(),
            delta: Some(delta),
            ..Default::default()
        })),
    }
}

fn supercomplete(path: &str, old_text: &str, new_text: &str) -> GetTabResponse {
    delta(
        path,
        delta_tab::Delta::DeltaSupercomplete(delta_tab::DeltaSupercomplete {
            delta_old_str: old_text.to_string(),
            delta_new_str: new_text.to_string(),
            ..Default::default()
        }),
    )
}

async fn message_frame(message: GetTabResponse) -> anyhow::Result<Vec<u8>> {
    let bytes = prost::Message::encode_to_vec(&message);
    Ok(encode_frame(&gzip(&bytes).await?, true, false))
}

struct Fixture {
    server: FakeServer,
    windsurf: Entity<Windsurf>,
    buffer: Entity<Buffer>,
    delegate: Entity<WindsurfDelegate>,
    project: Entity<Project>,
}

fn set_user_settings(settings: serde_json::Value, cx: &mut TestAppContext) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(&settings.to_string(), cx)
                .expect("user settings should load");
        });
    });
}

async fn setup(cx: &mut TestAppContext, files: serde_json::Value) -> Fixture {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });
    set_user_settings(
        json!({ "fork": { "windsurf": { "api_key": API_KEY } } }),
        cx,
    );

    let server = FakeServer(Arc::new(Mutex::new(ServerState::default())));
    let http = FakeHttpClient::create({
        let server = server.clone();
        move |request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }
    });
    let windsurf = cx.update(|cx| {
        let executor = cx.background_executor().clone();
        let backend = NativeBackend::new(
            http,
            Arc::new(move |duration| executor.timer(duration).boxed()),
            ClientConfig::default(),
        );
        crate::init_with_backend(Arc::new(backend), cx);
        Windsurf::global(cx).expect("windsurf should be initialized")
    });

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), files).await;
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/src/shapes.py"), cx)
        })
        .await
        .expect("buffer should open");
    windsurf.update(cx, |windsurf, cx| windsurf.track_buffer(&buffer, cx));
    let delegate = cx.new(|cx| WindsurfDelegate::new(windsurf.clone(), cx));
    Fixture {
        server,
        windsurf,
        buffer,
        delegate,
        project,
    }
}

fn default_files() -> serde_json::Value {
    json!({ "src": { "shapes.py": SOURCE } })
}

impl Fixture {
    fn edit(&self, offset: usize, text: &str, cx: &mut TestAppContext) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.edit([(offset..offset, text)], None, cx);
        });
    }

    fn cursor_after(&self, needle: &str, cx: &mut TestAppContext) -> Anchor {
        self.buffer.read_with(cx, |buffer, _| {
            let text = buffer.text();
            let offset = text.find(needle).expect("needle should be in the buffer") + needle.len();
            buffer.anchor_before(offset)
        })
    }

    fn refresh(&self, cursor: Anchor, cx: &mut TestAppContext) {
        self.refresh_for(cursor, EditPredictionRequestTrigger::BufferEdit, cx);
    }

    fn refresh_for(
        &self,
        cursor: Anchor,
        trigger: EditPredictionRequestTrigger,
        cx: &mut TestAppContext,
    ) {
        let buffer = self.buffer.clone();
        self.delegate.update(cx, |delegate, cx| {
            delegate.refresh(buffer, cursor, Duration::ZERO, trigger, cx)
        });
    }

    /// Does what the editor does when Tab is pressed: show the prediction,
    /// apply it, tell the delegate and ask for the next one.
    fn accept(&self, cx: &mut TestAppContext) {
        let buffer = self.buffer.clone();
        let cursor = self.delegate.update(cx, |delegate, cx| {
            let start = buffer.read(cx).anchor_before(0);
            let Some(EditPrediction::Local { edits, .. }) = delegate.suggest(&buffer, start, cx)
            else {
                panic!("there should be a prediction to accept");
            };
            delegate.did_show(SuggestionDisplayType::DiffPopover, cx);
            let end = edits.last().map(|(range, _)| range.end).expect("edits");
            buffer.update(cx, |buffer, cx| buffer.edit(edits, None, cx));
            delegate.accept(cx);
            end
        });
        self.refresh_for(cursor, EditPredictionRequestTrigger::PredictionAccepted, cx);
    }

    /// The buffer text with the shown prediction applied.
    fn predicted_text(&self, cx: &mut TestAppContext) -> Option<String> {
        let buffer = self.buffer.clone();
        self.delegate.update(cx, |delegate, cx| {
            let start = buffer.read(cx).anchor_before(0);
            let EditPrediction::Local { edits, .. } = delegate.suggest(&buffer, start, cx)? else {
                panic!("expected a local prediction");
            };
            let snapshot = buffer.read(cx).snapshot();
            let mut text = snapshot.text();
            for (range, new_text) in edits.iter().rev() {
                text.replace_range(
                    range.start.to_offset(&snapshot)..range.end.to_offset(&snapshot),
                    new_text,
                );
            }
            Some(text)
        })
    }

    fn ignore_prediction(&self, cx: &mut TestAppContext) {
        self.delegate.update(cx, |delegate, cx| {
            delegate.discard(EditPredictionDiscardReason::Ignored, cx)
        });
    }

    fn trajectory_types(&self, cx: &mut TestAppContext) -> Vec<CortexStepType> {
        self.windsurf
            .read_with(cx, |windsurf, _| step_types(&windsurf.trajectory().steps()))
    }

    fn stats(&self, cx: &mut TestAppContext) -> Stats {
        self.windsurf.read_with(cx, |windsurf, _| windsurf.stats())
    }

    fn last_response(&self, cx: &mut TestAppContext) -> serde_json::Value {
        self.windsurf.read_with(cx, |windsurf, _| {
            windsurf
                .last_request()
                .map(|last_request| last_request.response.clone())
                .expect("a request should have finished")
        })
    }

    fn suggest(&self, cx: &mut TestAppContext) -> Option<Vec<(Point, Point, String)>> {
        let buffer = self.buffer.clone();
        self.delegate.update(cx, |delegate, cx| {
            let cursor = buffer.read(cx).anchor_before(0);
            let prediction = delegate.suggest(&buffer, cursor, cx)?;
            let EditPrediction::Local { edits, .. } = prediction else {
                panic!("expected a local prediction");
            };
            let snapshot = buffer.read(cx).snapshot();
            Some(
                edits
                    .into_iter()
                    .map(|(range, text)| {
                        (
                            range.start.to_point(&snapshot),
                            range.end.to_point(&snapshot),
                            text.to_string(),
                        )
                    })
                    .collect(),
            )
        })
    }

    fn status(&self, cx: &mut TestAppContext) -> Status {
        self.windsurf
            .read_with(cx, |windsurf, cx| windsurf.status(cx))
    }

    fn settle(&self, cx: &mut TestAppContext) {
        cx.executor().advance_clock(DEBOUNCE);
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_prediction_round_trip(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    // The user renames `width` in the signature; the server predicts the body.
    let offset = SOURCE.find("width,").expect("width") + "width".len();
    fixture.edit(offset, "_px", cx);
    fixture.server.reply(Reply::Rewrite(Box::new(|selection| {
        selection.replace("return width *", "return width_px *")
    })));
    let cursor = fixture.cursor_after("width_px", cx);
    fixture.refresh(cursor, cx);
    assert_eq!(
        fixture.server.tab_request_count(),
        0,
        "requests are debounced"
    );
    fixture.settle(cx);

    let request = fixture.server.last_tab_request();
    let metadata = request.metadata.as_ref().expect("metadata");
    assert_eq!(metadata.api_key, API_KEY);
    assert!(!metadata.user_jwt.is_empty());
    let components = request
        .unified_prompt_components
        .as_ref()
        .expect("components");
    let intent = components
        .command_intent_info
        .as_ref()
        .expect("command intent");
    let document = intent.input_document.as_ref().expect("document");
    let text = SOURCE.replacen("width,", "width_px,", 1);
    assert_eq!(document.text, text);
    assert_eq!(
        document.cursor_offset as usize,
        text.find("width_px").expect("cursor") + "width_px".len()
    );
    assert_eq!(
        (intent.selection_start_line, intent.selection_end_line),
        (0, 6)
    );
    let steps = &components
        .cortex_trajectory
        .as_ref()
        .expect("trajectory")
        .steps;
    assert_eq!(steps.len(), 1);
    let Some(cortex_trajectory_step::Step::CodeAction(action)) = &steps[0].step else {
        panic!("expected a code action step");
    };
    let Some(crate::proto::exa::cortex_pb::action_result::Result::Edit(edit)) = action
        .action_result
        .as_ref()
        .and_then(|result| result.result.as_ref())
    else {
        panic!("expected an edit");
    };
    assert_eq!(edit.original_content, SOURCE);

    assert_eq!(
        fixture.suggest(cx),
        Some(vec![(
            Point::new(1, 16),
            Point::new(1, 16),
            "_px".to_string()
        )])
    );
    assert_eq!(fixture.status(cx), Status::Ready);

    // Typing along the prediction keeps it, minus what was typed.
    fixture.edit(text.find("width *").expect("body") + "width".len(), "_", cx);
    assert_eq!(
        fixture.suggest(cx),
        Some(vec![(
            Point::new(1, 17),
            Point::new(1, 17),
            "px".to_string()
        )])
    );

    fixture
        .delegate
        .update(cx, |delegate, cx| delegate.accept(cx));
    assert_eq!(fixture.suggest(cx), None);
}

#[gpui::test]
async fn test_no_request_without_edits(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    let cursor = fixture.cursor_after("def main", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 0);
    assert_eq!(fixture.server.0.lock().jwt_calls, 0);
    assert_eq!(fixture.status(cx), Status::Ready);
}

#[gpui::test]
async fn test_noop_and_unchanged_responses_show_nothing(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    fixture.edit(SOURCE.len(), "x", cx);
    let cursor = fixture.cursor_after("print(area(2, 3))\nx", cx);

    fixture.server.reply(Reply::Noop);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 1);
    assert_eq!(fixture.suggest(cx), None);

    fixture.edit(SOURCE.len() + 1, "y", cx);
    fixture
        .server
        .reply(Reply::Rewrite(Box::new(|selection| selection.to_string())));
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 2);
    assert_eq!(fixture.suggest(cx), None);

    // The second request carries both edits as separate steps: the first
    // as the hash the server returned for it, the second in full.
    let request = fixture.server.last_tab_request();
    let trajectory_steps = request.trajectory_steps_v2.clone().expect("cached hashes");
    assert_eq!(trajectory_steps.hashes, vec![1000]);
    assert_eq!(trajectory_steps.steps_after_hashes.len(), 1);
    let steps = request
        .unified_prompt_components
        .and_then(|components| components.cortex_trajectory)
        .map(|trajectory| trajectory.steps.len());
    assert_eq!(steps, Some(0));
}

#[gpui::test]
async fn test_prediction_is_dropped_when_buffer_changes_during_request(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    fixture.edit(SOURCE.find("print").expect("print"), "  ", cx);
    let release = fixture.server.hold_next_response();
    fixture.server.reply(Reply::Rewrite(Box::new(|selection| {
        selection.replace("area(2, 3)", "area(4, 5)")
    })));
    let cursor = fixture.cursor_after("  print", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 1);
    assert_eq!(fixture.status(cx), Status::Requesting);

    // Another editor changes the window while the response is pending.
    let offset = fixture
        .buffer
        .read_with(cx, |buffer, _| buffer.text().find("area(2").expect("call"));
    fixture.edit(offset, "big_", cx);
    release.send(()).expect("server should be waiting");
    cx.run_until_parked();

    assert_eq!(fixture.suggest(cx), None);
    let outcome = fixture.windsurf.read_with(cx, |windsurf, _| {
        windsurf
            .last_request()
            .map(|last_request| last_request.response["outcome"].clone())
    });
    assert_eq!(
        outcome,
        Some(json!(
            "discarded: the buffer changed while the request was in flight"
        ))
    );
}

#[gpui::test]
async fn test_rapid_typing_cancels_stale_requests(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    let end = SOURCE.len();
    fixture.edit(end, "r", cx);
    let _first_release = fixture.server.hold_next_response();
    fixture.server.reply(Reply::Rewrite(Box::new(|selection| {
        format!("{selection}eturn_stale()")
    })));
    let cursor = fixture.cursor_after("\nr", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 1);

    // Typing again before the first response arrives starts a new request.
    fixture.edit(end + 1, "e", cx);
    fixture.server.reply(Reply::Rewrite(Box::new(|selection| {
        format!("{selection}turn 1")
    })));
    let cursor = fixture.cursor_after("\nre", cx);
    fixture.refresh(cursor, cx);
    // Within the debounce interval nothing new is sent.
    cx.run_until_parked();
    assert_eq!(fixture.server.tab_request_count(), 1);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 2);

    let suggestion = fixture
        .suggest(cx)
        .expect("the second request should predict");
    assert_eq!(suggestion.len(), 1);
    assert_eq!(suggestion[0].2, "turn 1");
    assert_eq!(fixture.status(cx), Status::Ready);
}

#[gpui::test]
async fn test_errors_are_reported_and_retried(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    fixture.edit(SOURCE.len(), "x", cx);
    let cursor = fixture.cursor_after("\nx", cx);

    // Network failures back off and retry by themselves.
    fixture.server.reply(Reply::TransportError);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 1);
    assert!(
        matches!(
            fixture.status(cx),
            Status::Error(ErrorStatus::Network { .. })
        ),
        "{:?}",
        fixture.status(cx)
    );
    fixture.server.reply(Reply::HttpStatus(429));
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(fixture.server.tab_request_count(), 2);
    let Status::Error(ErrorStatus::RateLimited { retry_in, .. }) = fixture.status(cx) else {
        panic!("expected rate limiting, got {:?}", fixture.status(cx));
    };
    assert_eq!(retry_in, Duration::from_secs(2));
    // While backing off, new triggers don't send anything.
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 2);
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(fixture.server.tab_request_count(), 3);
    assert_eq!(fixture.status(cx), Status::Ready);

    // A rejected key stops all requests until the key changes.
    fixture.edit(SOURCE.len() + 1, "y", cx);
    fixture
        .server
        .reply(Reply::EndStreamError("unauthenticated"));
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 4);
    assert!(matches!(
        fixture.status(cx),
        Status::Error(ErrorStatus::Unauthenticated(_))
    ));
    cx.executor().advance_clock(Duration::from_secs(120));
    fixture.edit(SOURCE.len() + 2, "z", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 4);

    let jwt_calls_before = fixture.server.0.lock().jwt_calls;
    set_user_settings(
        json!({ "fork": { "windsurf": { "api_key": "devin-session-token$new" } } }),
        cx,
    );
    cx.run_until_parked();
    assert_eq!(fixture.status(cx), Status::Ready);
    fixture.server.reply(Reply::Noop);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 5);
    assert_eq!(fixture.server.0.lock().jwt_calls, jwt_calls_before + 1);
    assert_eq!(
        fixture
            .server
            .last_tab_request()
            .metadata
            .map(|metadata| metadata.api_key),
        Some("devin-session-token$new".to_string())
    );
}

#[gpui::test]
async fn test_disabled_sends_nothing(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    set_user_settings(
        json!({ "fork": { "windsurf": { "api_key": API_KEY, "enabled": false } } }),
        cx,
    );
    fixture.edit(0, "# hi\n", cx);
    let cursor = fixture.cursor_after("# hi", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 0);
    assert_eq!(fixture.status(cx), Status::Disabled);
    let buffer = fixture.buffer.clone();
    let enabled = fixture
        .delegate
        .read_with(cx, |delegate, cx| delegate.is_enabled(&buffer, cursor, cx));
    assert!(!enabled);
}

#[gpui::test]
async fn test_project_settings_cannot_supply_the_api_key(cx: &mut TestAppContext) {
    let fixture = setup(
        cx,
        json!({
            ".zed": { "settings.json": r#"{ "fork": { "windsurf": { "api_key": "from-project" } } }"# },
            "src": { "shapes.py": SOURCE },
        }),
    )
    .await;
    set_user_settings(json!({}), cx);
    cx.run_until_parked();
    let worktree_id = fixture.project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("worktree")
            .read(cx)
            .id()
    });
    cx.update(|cx| {
        assert_eq!(WindsurfSettings::get_global(cx).api_key, None);
        let location = settings::SettingsLocation {
            worktree_id,
            path: util::rel_path::rel_path("src/shapes.py"),
        };
        assert_eq!(WindsurfSettings::get(Some(location), cx).api_key, None);
    });
    fixture.edit(0, "# hi\n", cx);
    let cursor = fixture.cursor_after("# hi", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 0);
    assert_eq!(fixture.status(cx), Status::MissingApiKey);
}

#[gpui::test]
async fn test_secrets_stay_out_of_debug_output(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    fixture.edit(SOURCE.len(), "x", cx);
    fixture.server.reply(Reply::Noop);
    let cursor = fixture.cursor_after("\nx", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    let user_jwt = fixture
        .server
        .last_tab_request()
        .metadata
        .map(|metadata| metadata.user_jwt)
        .expect("metadata");
    let text = cx.update(|cx| crate::status::last_request_text(cx));
    assert!(text.contains("\"api_key\": \"<redacted, "), "{text}");
    assert!(!text.contains("test-api-key"));
    assert!(!text.contains(&user_jwt));
    assert!(text.contains("\"outcome\": \"no prediction\""));
    let settings_debug = cx.update(|cx| format!("{:?}", WindsurfSettings::get_global(cx)));
    assert!(!settings_debug.contains("test-api-key"));

    fixture.delegate.update(cx, |delegate, cx| {
        delegate.discard(EditPredictionDiscardReason::Rejected, cx)
    });
}

/// All steps a request carries, in order, whether sent in full or after hashes.
fn sent_steps(request: &GetTabRequest) -> Vec<CortexTrajectoryStep> {
    let mut steps = request
        .unified_prompt_components
        .as_ref()
        .and_then(|components| components.cortex_trajectory.as_ref())
        .map(|trajectory| trajectory.steps.clone())
        .unwrap_or_default();
    if let Some(trajectory_steps) = &request.trajectory_steps_v2 {
        steps.extend(trajectory_steps.steps_after_hashes.iter().cloned());
    }
    steps
}

fn step_types(steps: &[CortexTrajectoryStep]) -> Vec<CortexStepType> {
    steps
        .iter()
        .map(|step| CortexStepType::try_from(step.r#type).expect("known step type"))
        .collect()
}

fn hash_count(request: &GetTabRequest) -> usize {
    request
        .trajectory_steps_v2
        .as_ref()
        .map(|steps| steps.hashes.len())
        .unwrap_or_default()
}

fn big_files() -> serde_json::Value {
    let mut source = SOURCE.to_string();
    for index in 0..200 {
        source.push_str(&format!(
            "\n\ndef helper_{index}(value):\n    return value * {index}\n"
        ));
    }
    json!({ "src": { "shapes.py": source } })
}

#[gpui::test]
async fn test_trajectory_cache_shrinks_requests_and_recovers(cx: &mut TestAppContext) {
    let fixture = setup(cx, big_files()).await;
    let mut sizes = Vec::new();
    for (index, text) in ["a", "b", "c"].into_iter().enumerate() {
        fixture.edit(index, text, cx);
        let cursor = fixture.cursor_after(text, cx);
        fixture.server.reply(Reply::Noop);
        fixture.refresh(cursor, cx);
        fixture.settle(cx);
        sizes.push(
            fixture.last_response(cx)["request_bytes"]
                .as_u64()
                .expect("size"),
        );
    }
    assert_eq!(fixture.server.tab_request_count(), 3);
    // The first request carries the whole file as `original_content`; later
    // ones send it as a hash.
    assert_eq!(hash_count(&fixture.server.last_tab_request()), 2);
    // Every request carries the current document; the first one also the
    // whole file as `original_content`.
    assert!(
        sizes[2] * 3 < sizes[0] * 2,
        "incremental requests should be much smaller: {sizes:?}"
    );

    // The server lost its cache: the request is sent again in full and the
    // cache is rebuilt from the new response.
    fixture.server.0.lock().forget_hashes = 1;
    fixture.edit(3, "d", cx);
    let cursor = fixture.cursor_after("d", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 5);
    let resent = fixture.server.last_tab_request();
    assert_eq!(hash_count(&resent), 0);
    assert_eq!(sent_steps(&resent).len(), 4);
    assert_eq!(fixture.status(cx), Status::Ready);
    assert_eq!(fixture.stats(cx).full_resends, 1);
    assert_eq!(
        fixture.last_response(cx)["resent_full_trajectory"],
        json!(true)
    );

    fixture.edit(4, "e", cx);
    let cursor = fixture.cursor_after("e", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(hash_count(&fixture.server.last_tab_request()), 4);

    // Missing information even for the full trajectory: give up on this
    // request, then start over with a full trajectory.
    fixture.server.0.lock().always_missing = 2;
    fixture.edit(5, "f", cx);
    let cursor = fixture.cursor_after("f", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 8);
    assert!(matches!(
        fixture.status(cx),
        Status::Error(ErrorStatus::Protocol(_))
    ));
    fixture.edit(6, "g", cx);
    let cursor = fixture.cursor_after("g", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(hash_count(&fixture.server.last_tab_request()), 0);
    assert_eq!(sent_steps(&fixture.server.last_tab_request()).len(), 7);
    assert_eq!(fixture.status(cx), Status::Ready);
}

fn rename_width(fixture: &Fixture, cx: &mut TestAppContext) {
    let offset = SOURCE.find("width,").expect("width") + "width".len();
    fixture.edit(offset, "_px", cx);
    fixture.server.reply(Reply::Rewrite(Box::new(|selection| {
        selection.replace("return width *", "return width_px *")
    })));
    fixture
        .server
        .reply_to_predictive(Reply::Rewrite(Box::new(|selection| {
            selection.replace("area(2, 3)", "area(width_px=2, height=3)")
        })));
    let cursor = fixture.cursor_after("width_px", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
}

#[gpui::test]
async fn test_accepting_shows_the_predictive_response_without_a_request(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    rename_width(&fixture, cx);
    assert_eq!(fixture.server.tab_request_count(), 1);
    assert_eq!(fixture.server.predictive_request_count(), 1);

    let trigger_request = fixture.server.last_tab_request();
    let predictive = fixture.server.last_predictive_request();
    assert_eq!(
        predictive
            .metadata
            .as_ref()
            .map(|metadata| &metadata.trigger_id),
        trigger_request
            .metadata
            .as_ref()
            .map(|metadata| &metadata.trigger_id),
        "a predictive request belongs to its trigger"
    );
    let document = predictive
        .unified_prompt_components
        .as_ref()
        .and_then(|components| components.command_intent_info.as_ref())
        .and_then(|intent| intent.input_document.as_ref())
        .expect("document");
    let accepted_text = SOURCE.replace("width", "width_px");
    assert_eq!(document.text, accepted_text);
    let cursor_offset = accepted_text.find("width_px *").expect("body") + "width_px".len();
    assert_eq!(document.cursor_offset as usize, cursor_offset);
    let cursor_position = document.cursor_position.expect("cursor position");
    assert_eq!((cursor_position.row, cursor_position.col), (1, 19));
    // The trajectory has the accepting edit as one more step, and the
    // trigger's step is sent as a hash.
    assert_eq!(hash_count(&predictive), 1);
    assert_eq!(sent_steps(&predictive).len(), 1);

    fixture.accept(cx);
    cx.run_until_parked();
    assert_eq!(
        fixture.server.tab_request_count(),
        1,
        "no request after accepting"
    );
    assert_eq!(
        fixture.predicted_text(cx),
        Some(accepted_text.replace("area(2, 3)", "area(width_px=2, height=3)"))
    );
    let stats = fixture.stats(cx);
    assert_eq!((stats.requests, stats.predictive_hits), (1, 1));
    // The shown prefetched prediction gets its own predictive request.
    assert_eq!(fixture.server.predictive_request_count(), 2);

    // Feedback: reported once, and in the next request's trajectory and
    // feedback context.
    let feedback = fixture.server.0.lock().feedback_requests.clone();
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].feedback, FeedbackType::Accept as i32);
    assert_eq!(feedback[0].source, CompletionSource::Network as i32);
    assert_eq!(feedback[0].prompt_id, trigger_request.prompt_id);
    assert!(
        feedback[0]
            .metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.user_jwt.is_empty())
    );

    fixture.ignore_prediction(cx);
    fixture.edit(0, "#", cx);
    fixture.server.reply(Reply::Noop);
    let cursor = fixture.cursor_after("#", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 2);
    assert_eq!(
        fixture.trajectory_types(cx),
        vec![
            CortexStepType::CodeAction,
            CortexStepType::SupercompleteFeedback,
            CortexStepType::CodeAction,
            CortexStepType::CodeAction,
        ],
        "the typed edit, the feedback, the accepted edit and the new edit"
    );
    let request = fixture.server.last_tab_request();
    // The predictive request after accepting already sent the first three.
    assert_eq!(hash_count(&request), 3);
    assert_eq!(sent_steps(&request).len(), 1);
    let context = request
        .unified_prompt_components
        .and_then(|components| components.supercomplete_feedback_context)
        .expect("feedback context");
    assert_eq!(context.feedback.len(), 1);
    assert!(context.feedback[0].accepted);
    assert_eq!(context.feedback[0].completion_id, feedback[0].completion_id);
    let diff = context.feedback[0].diff_suggestion.as_ref().expect("diff");
    assert!(diff.path.ends_with("/src/shapes.py"));
    assert!(diff.old_str.contains("return width *"));
}

#[gpui::test]
async fn test_trigger_waits_for_a_predictive_request_in_flight(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    let release = fixture.server.hold_next_predictive_response();
    rename_width(&fixture, cx);
    assert_eq!(fixture.server.predictive_request_count(), 1);

    fixture.accept(cx);
    cx.run_until_parked();
    assert_eq!(fixture.server.tab_request_count(), 1);
    assert_eq!(fixture.suggest(cx), None);

    release.send(()).expect("server should be waiting");
    cx.run_until_parked();
    assert_eq!(fixture.server.tab_request_count(), 1);
    assert!(fixture.suggest(cx).is_some());
    assert_eq!(fixture.stats(cx).predictive_hits, 1);
}

#[gpui::test]
async fn test_typing_along_and_through_a_prediction(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    rename_width(&fixture, cx);
    let body = SOURCE.replacen("width,", "width_px,", 1);
    let offset = body.find("width *").expect("body") + "width".len();
    for (index, character) in ["_", "p", "x"].into_iter().enumerate() {
        fixture.edit(offset + index, character, cx);
        let cursor = fixture.cursor_after(&format!("width{}", &"_px"[..=index]), cx);
        fixture.refresh(cursor, cx);
        fixture.settle(cx);
        if index < 2 {
            assert!(fixture.suggest(cx).is_some());
        }
    }
    // Typing out the whole prediction reaches the state the predictive
    // request assumed.
    assert_eq!(fixture.server.tab_request_count(), 1);
    let stats = fixture.stats(cx);
    assert_eq!((stats.local_cache_hits, stats.predictive_hits), (2, 1));
    assert!(fixture.suggest(cx).is_some());

    // Typing something else drops it all and asks again.
    fixture.ignore_prediction(cx);
    fixture.edit(0, "#", cx);
    let cursor = fixture.cursor_after("#", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    assert_eq!(fixture.server.tab_request_count(), 2);
}

#[gpui::test]
async fn test_rejecting_records_intentional_feedback(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    rename_width(&fixture, cx);
    fixture.suggest(cx);
    fixture.delegate.update(cx, |delegate, cx| {
        delegate.discard(EditPredictionDiscardReason::Rejected, cx)
    });
    cx.run_until_parked();
    let feedback = fixture.server.0.lock().feedback_requests.clone();
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].feedback, FeedbackType::Reject as i32);
    assert_eq!(
        feedback[0].extra_info,
        FeedbackExtraInfo::IntentionalReject as i32
    );

    // A second rejection within five seconds isn't reported, but still goes
    // into the trajectory.
    fixture.edit(0, "#", cx);
    fixture.server.reply(Reply::Rewrite(Box::new(|selection| {
        selection.replace("area(2, 3)", "area(3, 3)")
    })));
    let cursor = fixture.cursor_after("#", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    fixture.suggest(cx);
    fixture.delegate.update(cx, |delegate, cx| {
        delegate.discard(EditPredictionDiscardReason::Rejected, cx)
    });
    cx.run_until_parked();
    assert_eq!(fixture.server.0.lock().feedback_requests.len(), 1);

    // Ignored predictions aren't feedback.
    fixture.edit(1, "#", cx);
    fixture.server.reply(Reply::Rewrite(Box::new(|selection| {
        selection.replace("area(2, 3)", "area(4, 3)")
    })));
    let cursor = fixture.cursor_after("##", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    fixture.delegate.update(cx, |delegate, cx| {
        delegate.discard(EditPredictionDiscardReason::Ignored, cx)
    });

    fixture.edit(2, "#", cx);
    fixture.server.reply(Reply::Noop);
    let cursor = fixture.cursor_after("###", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    let feedback_steps: Vec<_> = fixture.windsurf.read_with(cx, |windsurf, _| {
        windsurf
            .trajectory()
            .steps()
            .into_iter()
            .filter_map(|step| match step.step {
                Some(cortex_trajectory_step::Step::SupercompleteFeedback(feedback)) => {
                    Some((feedback.feedback_type, feedback.feedback_reason))
                }
                _ => None,
            })
            .collect()
    });
    assert_eq!(
        feedback_steps,
        vec![
            ("reject".to_string(), "intentional".to_string()),
            ("reject".to_string(), "intentional".to_string()),
        ]
    );
    let context = fixture
        .server
        .last_tab_request()
        .unified_prompt_components
        .and_then(|components| components.supercomplete_feedback_context)
        .expect("feedback context");
    assert_eq!(context.feedback.len(), 2);
    assert!(
        context
            .feedback
            .iter()
            .all(|entry| entry.intentional_reject && !entry.accepted)
    );
}

#[gpui::test]
async fn test_clipboard_step_within_five_seconds(cx: &mut TestAppContext) {
    let fixture = setup(cx, default_files()).await;
    fixture.edit(0, "# one\n", cx);
    fixture.windsurf.update(cx, |windsurf, cx| {
        windsurf.record_copy("print(area(2, 3))".to_string(), cx)
    });
    fixture.edit(0, "# two\n", cx);
    let cursor = fixture.cursor_after("# two", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    let steps = sent_steps(&fixture.server.last_tab_request());
    assert_eq!(
        step_types(&steps),
        vec![
            CortexStepType::CodeAction,
            CortexStepType::Clipboard,
            CortexStepType::CodeAction,
        ]
    );
    let Some(cortex_trajectory_step::Step::Clipboard(clipboard)) = &steps[1].step else {
        panic!("expected a clipboard step");
    };
    assert_eq!(clipboard.content, "print(area(2, 3))");

    // Too old, or looks like a secret: not sent.
    for (content, wait) in [
        ("print(area(4, 5))", Duration::from_secs(6)),
        ("hunter2Password!", Duration::ZERO),
    ] {
        fixture.windsurf.update(cx, |windsurf, cx| {
            windsurf.record_copy(content.to_string(), cx)
        });
        cx.executor().advance_clock(wait);
        fixture.edit(0, "#", cx);
        let cursor = fixture.cursor_after("#", cx);
        fixture.refresh(cursor, cx);
        fixture.settle(cx);
        let steps = sent_steps(&fixture.server.last_tab_request());
        assert!(
            !steps.iter().any(|step| matches!(
                &step.step,
                Some(cortex_trajectory_step::Step::Clipboard(clipboard)) if clipboard.content == content
            )),
            "{content} shouldn't be sent"
        );
    }

    // A paste of something copied elsewhere counts as a copy; pasting what
    // was already recorded doesn't.
    fixture.windsurf.update(cx, |windsurf, cx| {
        windsurf.record_paste("from_another_app".to_string(), cx);
        windsurf.record_paste("from_another_app".to_string(), cx);
    });
    fixture.edit(0, "from_another_app", cx);
    let cursor = fixture.cursor_after("from_another_app", cx);
    fixture.refresh(cursor, cx);
    fixture.settle(cx);
    let clipboard_contents: Vec<_> = sent_steps(&fixture.server.last_tab_request())
        .into_iter()
        .filter_map(|step| match step.step {
            Some(cortex_trajectory_step::Step::Clipboard(clipboard)) => Some(clipboard.content),
            _ => None,
        })
        .collect();
    assert_eq!(clipboard_contents, vec!["from_another_app".to_string()]);
}
