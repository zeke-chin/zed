//! The delegate with Tab Jump against a fake server that answers `GetTab`
//! and `GetChatMessage`.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate as _, EditPredictionRequestTrigger,
};
use fs::{FakeFs, Fs};
use futures::{AsyncReadExt as _, FutureExt as _};
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use http_client::{AsyncBody, FakeHttpClient, Request, Response};
use language::{Anchor, Buffer, Point, ToPoint as _};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use util::path;

use crate::{
    Windsurf, WindsurfDelegate,
    delegate::DEBOUNCE,
    native::{
        ClientConfig, NativeBackend,
        auth::fake_token,
        connect::{FrameDecoder, encode_frame, gunzip, gzip},
    },
    proto::exa::{
        api_server_pb::{
            DeltaTab, GetChatMessageRequest, GetChatMessageResponse, GetTabRequest, GetTabResponse,
            delta_tab, get_tab_response,
        },
        auth_pb::GetUserJwtResponse,
        codeium_common_pb::{ChatMessageSource, Model, ProviderSource, StopReason},
    },
};

const API_KEY: &str = "devin-session-token$test-api-key";
const SHAPES: &str =
    "def area(width, height):\n    return width * height\n\n\ndef main():\n    print(area(2, 3))\n";
const UTIL: &str = "def helper():\n    return 1\n";

#[derive(Default)]
struct ServerState {
    tab_requests: Vec<GetTabRequest>,
    chat_requests: Vec<GetChatMessageRequest>,
    /// Replaces the window `GetTab` was sent; `None` answers with a no-op.
    tab_rewrite: Option<(String, String)>,
    /// The continuation `GetChatMessage` streams back.
    completion: String,
}

#[derive(Clone)]
struct FakeServer(Arc<Mutex<ServerState>>);

async fn frame<M: prost::Message>(message: &M) -> anyhow::Result<Vec<u8>> {
    Ok(encode_frame(
        &gzip(&message.encode_to_vec()).await?,
        true,
        false,
    ))
}

fn tab_delta(path: &str, delta: delta_tab::Delta) -> GetTabResponse {
    GetTabResponse {
        message: Some(get_tab_response::Message::StreamingCompletionV2(DeltaTab {
            path: path.to_string(),
            delta: Some(delta),
            ..Default::default()
        })),
    }
}

impl FakeServer {
    async fn handle(&self, request: Request<AsyncBody>) -> anyhow::Result<Response<AsyncBody>> {
        let path = request.uri().path().to_string();
        let mut body = Vec::new();
        request.into_body().read_to_end(&mut body).await?;
        if path.ends_with("/GetUserJwt") {
            let response = GetUserJwtResponse {
                user_jwt: fake_token(SystemTime::now() + Duration::from_secs(900)),
                ..Default::default()
            };
            return Ok(Response::builder()
                .status(200)
                .body(AsyncBody::from(prost::Message::encode_to_vec(&response)))?);
        }
        let mut decoder = FrameDecoder::default();
        decoder.push(&body);
        let payload = gunzip(&decoder.next_frame()?.expect("request frame").payload).await?;
        let mut frames = Vec::new();
        if path.ends_with("/GetTab") {
            let request = <GetTabRequest as prost::Message>::decode(payload.as_slice())?;
            let intent = request
                .unified_prompt_components
                .as_ref()
                .and_then(|components| components.command_intent_info.clone())
                .unwrap_or_default();
            let file_path = intent
                .input_document
                .as_ref()
                .and_then(|document| url::Url::parse(&document.absolute_uri).ok())
                .and_then(|url| url.to_file_path().ok())
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rewrite = {
                let mut state = self.0.lock();
                state.tab_requests.push(request);
                state.tab_rewrite.clone()
            };
            match rewrite {
                Some((old, new)) => {
                    let new_text = intent.selection.replace(&old, &new);
                    frames.push(
                        frame(&tab_delta(
                            &file_path,
                            delta_tab::Delta::DeltaSupercomplete(delta_tab::DeltaSupercomplete {
                                delta_old_str: intent.selection.clone(),
                                ..Default::default()
                            }),
                        ))
                        .await?,
                    );
                    frames.push(
                        frame(&tab_delta(
                            &file_path,
                            delta_tab::Delta::DeltaSupercomplete(delta_tab::DeltaSupercomplete {
                                delta_new_str: new_text,
                                ..Default::default()
                            }),
                        ))
                        .await?,
                    );
                }
                None => frames.push(
                    frame(&tab_delta(
                        &file_path,
                        delta_tab::Delta::DeltaNoop(delta_tab::DeltaNoop::default()),
                    ))
                    .await?,
                ),
            }
        } else if path.ends_with("/GetChatMessage") {
            let request = <GetChatMessageRequest as prost::Message>::decode(payload.as_slice())?;
            let completion = {
                let mut state = self.0.lock();
                state.chat_requests.push(request);
                state.completion.clone()
            };
            let mut rest = completion.as_str();
            while !rest.is_empty() {
                let mut split = rest.len().min(7);
                while !rest.is_char_boundary(split) {
                    split += 1;
                }
                frames.push(
                    frame(&GetChatMessageResponse {
                        message_id: "bot-test".to_string(),
                        delta_text: rest[..split].to_string(),
                        ..Default::default()
                    })
                    .await?,
                );
                rest = &rest[split..];
            }
            frames.push(
                frame(&GetChatMessageResponse {
                    stop_reason: StopReason::StopPattern as i32,
                    ..Default::default()
                })
                .await?,
            );
        } else {
            anyhow::bail!("unexpected path {path}");
        }
        frames.push(encode_frame(b"{}", false, true));
        Ok(Response::builder()
            .status(200)
            .body(AsyncBody::from(frames.concat()))?)
    }

    /// `GetTab` requests sent for triggers (predictive requests, which set
    /// `input_document.cursor_position`, aren't counted) and `GetChatMessage`
    /// requests.
    fn counts(&self) -> (usize, usize) {
        let state = self.0.lock();
        let trigger_requests = state
            .tab_requests
            .iter()
            .filter(|request| !is_predictive(request))
            .count();
        (trigger_requests, state.chat_requests.len())
    }

    fn predictive_count(&self) -> usize {
        let state = self.0.lock();
        state
            .tab_requests
            .iter()
            .filter(|request| is_predictive(request))
            .count()
    }
}

fn is_predictive(request: &GetTabRequest) -> bool {
    request
        .unified_prompt_components
        .as_ref()
        .and_then(|components| components.command_intent_info.as_ref())
        .and_then(|intent| intent.input_document.as_ref())
        .is_some_and(|document| document.cursor_position.is_some())
}

fn completion(target: &str, replacement: &str) -> String {
    format!(
        " {},\n\t\t\"ReplacementContent\": {}\n\t}}]\n}}\n</replace_file_content>\n",
        serde_json::to_string(target).expect("json"),
        serde_json::to_string(replacement).expect("json")
    )
}

struct Fixture {
    server: FakeServer,
    windsurf: Entity<Windsurf>,
    project: Entity<Project>,
    shapes: Entity<Buffer>,
    util: Entity<Buffer>,
    delegate: Entity<WindsurfDelegate>,
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

async fn setup(cx: &mut TestAppContext) -> Fixture {
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
    fs.insert_tree(
        path!("/root"),
        json!({ "src": { "shapes.py": SHAPES, "util.py": UTIL } }),
    )
    .await;
    cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let mut buffers = Vec::new();
    for file in [path!("/root/src/shapes.py"), path!("/root/src/util.py")] {
        let buffer = project
            .update(cx, |project, cx| project.open_local_buffer(file, cx))
            .await
            .expect("buffer should open");
        windsurf.update(cx, |windsurf, cx| windsurf.track_buffer(&buffer, cx));
        buffers.push(buffer);
    }
    let util = buffers.pop().expect("util");
    let shapes = buffers.pop().expect("shapes");
    let delegate = new_delegate(&windsurf, &project, cx);
    Fixture {
        server,
        windsurf,
        project,
        shapes,
        util,
        delegate,
    }
}

fn new_delegate(
    windsurf: &Entity<Windsurf>,
    project: &Entity<Project>,
    cx: &mut TestAppContext,
) -> Entity<WindsurfDelegate> {
    let project = project.downgrade();
    cx.new(|cx| {
        let mut delegate = WindsurfDelegate::new(windsurf.clone(), cx);
        delegate.set_project(Some(project));
        delegate
    })
}

fn append(buffer: &Entity<Buffer>, text: &str, cx: &mut TestAppContext) -> Anchor {
    buffer.update(cx, |buffer, cx| {
        let end = buffer.len();
        buffer.edit([(end..end, text)], None, cx);
        buffer.anchor_before(buffer.len())
    })
}

fn refresh(
    delegate: &Entity<WindsurfDelegate>,
    buffer: &Entity<Buffer>,
    cursor: Anchor,
    cx: &mut TestAppContext,
) {
    let buffer = buffer.clone();
    delegate.update(cx, |delegate, cx| {
        delegate.refresh(
            buffer,
            cursor,
            Duration::ZERO,
            EditPredictionRequestTrigger::BufferEdit,
            cx,
        )
    });
    cx.executor().advance_clock(DEBOUNCE);
    cx.run_until_parked();
}

enum Suggestion {
    Local(Vec<(Point, Point, String)>),
    Jump { path: String, target: Point },
}

fn suggest(
    delegate: &Entity<WindsurfDelegate>,
    buffer: &Entity<Buffer>,
    cx: &mut TestAppContext,
) -> Option<Suggestion> {
    let buffer = buffer.clone();
    delegate.update(cx, |delegate, cx| {
        let cursor = buffer.read(cx).anchor_before(0);
        match delegate.suggest(&buffer, cursor, cx)? {
            EditPrediction::Local { edits, .. } => {
                let snapshot = buffer.read(cx).snapshot();
                Some(Suggestion::Local(
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
                ))
            }
            EditPrediction::Jump {
                snapshot, target, ..
            } => Some(Suggestion::Jump {
                path: snapshot
                    .file()
                    .map(|file| file.path().as_unix_str().to_string())
                    .unwrap_or_default(),
                target: target.to_point(&snapshot),
            }),
        }
    })
}

#[gpui::test]
async fn test_jump_within_the_file(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    fixture.server.0.lock().completion = completion(
        "def area(<|cursor|>width, height):",
        "def area(width, height, depth):",
    );
    let cursor = append(&fixture.shapes, "    print(volume(1, 2, 3))\n", cx);
    refresh(&fixture.delegate, &fixture.shapes, cursor, cx);
    assert_eq!(fixture.server.counts(), (1, 1));

    let request = fixture.server.0.lock().chat_requests[0].clone();
    assert_eq!(request.provider_source, ProviderSource::TabJump as i32);
    assert_eq!(request.internal_chat_model, Model::Chat20706 as i32);
    let metadata = request.metadata.as_ref().expect("metadata");
    assert_eq!(metadata.api_key, API_KEY);
    assert!(!metadata.user_jwt.is_empty());
    let last = request.chat_message_prompts.last().expect("messages");
    assert_eq!(last.source, ChatMessageSource::System as i32);
    assert!(last.prompt.contains(&format!(
        "\"TargetFile\": {}",
        serde_json::to_string(path!("/root/src/shapes.py")).expect("json")
    )));
    let metadata_message = &request.chat_message_prompts[request.chat_message_prompts.len() - 2];
    assert!(
        metadata_message
            .prompt
            .contains("print(volume(1, 2, 3))\n<|cursor|>\n</additional_metadata>"),
        "{}",
        metadata_message.prompt
    );
    assert!(
        request
            .chat_message_prompts
            .iter()
            .any(|message| message.prompt.contains("+    print(volume(1, 2, 3))"))
    );

    let Some(Suggestion::Local(edits)) = suggest(&fixture.delegate, &fixture.shapes, cx) else {
        panic!("expected edits at the jump target");
    };
    assert_eq!(
        edits,
        vec![(Point::new(0, 22), Point::new(0, 22), ", depth".to_string())]
    );

    let text = cx.update(|cx| crate::status::last_request_text(cx));
    assert!(
        text.contains("\"outcome\": \"jump within the file\""),
        "{text}"
    );
    assert!(text.contains("\"api_key\": \"<redacted, "));
    assert!(!text.contains("test-api-key"));
    assert!(!text.contains(&metadata.user_jwt));
}

#[gpui::test]
async fn test_get_tab_prediction_takes_precedence(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    fixture.server.0.lock().completion = completion(
        "def area(<|cursor|>width, height):",
        "def area(width, height, depth):",
    );
    fixture.server.0.lock().tab_rewrite = Some((
        "print(area(2, 3))".to_string(),
        "print(area(4, 5))".to_string(),
    ));
    let cursor = append(&fixture.shapes, "x = 1\n", cx);
    refresh(&fixture.delegate, &fixture.shapes, cursor, cx);
    assert_eq!(fixture.server.counts(), (1, 1));
    // The prediction gets a predictive request; it doesn't send Tab Jump.
    assert_eq!(fixture.server.predictive_count(), 1);
    let Some(Suggestion::Local(edits)) = suggest(&fixture.delegate, &fixture.shapes, cx) else {
        panic!("expected the GetTab prediction");
    };
    assert!(
        edits.iter().all(|(start, _, _)| start.row == 5),
        "{:?}",
        edits.len()
    );
    // The Tab Jump result is still recorded for the debug view.
    let summary = cx.update(|cx| crate::tab_jump::last_summary(cx));
    assert_eq!(summary["outcome"], json!("jump within the file"));
}

#[gpui::test]
async fn test_ambiguous_or_missing_target_shows_nothing(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    fixture.server.0.lock().completion = completion("<|cursor|>width", "w");
    let cursor = append(&fixture.shapes, "y = 2\n", cx);
    refresh(&fixture.delegate, &fixture.shapes, cursor, cx);
    assert!(suggest(&fixture.delegate, &fixture.shapes, cx).is_none());
    let summary = cx.update(|cx| crate::tab_jump::last_summary(cx));
    assert_eq!(
        summary["outcome"],
        json!("the jump target occurs more than once")
    );

    fixture.server.0.lock().completion = " \"    print(are".to_string();
    let cursor = append(&fixture.shapes, "z = 3\n", cx);
    refresh(&fixture.delegate, &fixture.shapes, cursor, cx);
    assert!(suggest(&fixture.delegate, &fixture.shapes, cx).is_none());
    let summary = cx.update(|cx| crate::tab_jump::last_summary(cx));
    assert_eq!(summary["outcome"], json!("the completion ended early"));
}

#[gpui::test]
async fn test_jump_to_another_file(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    fixture.server.0.lock().completion =
        completion("def <|cursor|>helper():", "def helper(value):");
    append(&fixture.util, "\n\ndef other():\n    return 2\n", cx);
    let cursor = append(&fixture.shapes, "print(helper())\n", cx);
    refresh(&fixture.delegate, &fixture.shapes, cursor, cx);
    assert_eq!(fixture.server.counts(), (1, 1));

    let Some(Suggestion::Jump { path, target }) = suggest(&fixture.delegate, &fixture.shapes, cx)
    else {
        panic!("expected a jump to util.py");
    };
    assert_eq!((path.as_str(), target), ("src/util.py", Point::new(0, 4)));

    // The editor for util.py picks up the edits once it opens.
    let other_delegate = new_delegate(&fixture.windsurf, &fixture.project, cx);
    let Some(Suggestion::Local(edits)) = suggest(&other_delegate, &fixture.util, cx) else {
        panic!("expected the edits in util.py");
    };
    assert_eq!(
        edits,
        vec![(Point::new(0, 11), Point::new(0, 11), "value".to_string())]
    );
    // Once handed over, the first editor stops offering the jump.
    assert!(suggest(&fixture.delegate, &fixture.shapes, cx).is_none());
}

#[gpui::test]
async fn test_disabled_tab_jump_sends_no_chat_request(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    set_user_settings(
        json!({ "fork": { "windsurf": { "api_key": API_KEY, "tab_jump": false } } }),
        cx,
    );
    fixture.server.0.lock().completion = completion(
        "def area(<|cursor|>width, height):",
        "def area(width, height, depth):",
    );
    let cursor = append(&fixture.shapes, "x = 1\n", cx);
    refresh(&fixture.delegate, &fixture.shapes, cursor, cx);
    assert_eq!(fixture.server.counts(), (1, 0));
    assert!(suggest(&fixture.delegate, &fixture.shapes, cx).is_none());
    assert_eq!(
        cx.update(|cx| crate::tab_jump::last_summary(cx)),
        serde_json::Value::Null
    );
}

#[gpui::test]
async fn test_predictive_hit_without_edits_still_asks_tab_jump(cx: &mut TestAppContext) {
    let fixture = setup(cx).await;
    fixture.server.0.lock().completion = completion(
        "def area(<|cursor|>width, height):",
        "def area(width, height, depth):",
    );
    fixture.server.0.lock().tab_rewrite = Some((
        "print(area(2, 3))".to_string(),
        "print(area(4, 5))".to_string(),
    ));
    let cursor = append(&fixture.shapes, "x = 1\n", cx);
    refresh(&fixture.delegate, &fixture.shapes, cursor, cx);
    assert_eq!(fixture.server.counts(), (1, 1));
    assert_eq!(fixture.server.predictive_count(), 1);

    // Accept: the buffer now reads as the predictive request assumed, and
    // that request predicted nothing more (the rewrite no longer applies).
    let buffer = fixture.shapes.clone();
    fixture.delegate.update(cx, |delegate, cx| {
        let start = buffer.read(cx).anchor_before(0);
        let Some(EditPrediction::Local { edits, .. }) = delegate.suggest(&buffer, start, cx) else {
            panic!("expected the GetTab prediction");
        };
        buffer.update(cx, |buffer, cx| buffer.edit(edits, None, cx));
        delegate.accept(cx);
        delegate.refresh(
            buffer.clone(),
            start,
            Duration::ZERO,
            EditPredictionRequestTrigger::PredictionAccepted,
            cx,
        );
    });
    cx.run_until_parked();

    // No new GetTab for the trigger, but Tab Jump is asked where to go next.
    assert_eq!(fixture.server.counts(), (1, 2));
    let Some(Suggestion::Local(edits)) = suggest(&fixture.delegate, &fixture.shapes, cx) else {
        panic!("expected the Tab Jump edits");
    };
    assert_eq!(
        edits,
        vec![(Point::new(0, 22), Point::new(0, 22), ", depth".to_string())]
    );
}
