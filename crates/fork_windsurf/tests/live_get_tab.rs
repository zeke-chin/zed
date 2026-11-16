//! Sends real `GetUserJwt` and `GetTab` requests to the Windsurf cloud. Ignored
//! by default; run it explicitly with an API key in the environment:
//!
//! ```sh
//! WINDSURF_API_KEY=... cargo test -p fork_windsurf --test live_get_tab -- --ignored --nocapture
//! ```
//!
//! Requests are sent one at a time, at least 2.5 seconds apart. The key and
//! token are never printed. Run one test at a time (`--test-threads 1`).

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use fork_windsurf::{
    backend::{DocumentContext, FeedbackReport, TabBackend as _, TabRequestInput},
    context::selection_window,
    diff::prediction_edits,
    native::{ClientConfig, NativeBackend, connect::Timer},
    proto::exa::codeium_common_pb::Language,
    trajectory::edit_step,
};
use futures::{FutureExt as _, channel::oneshot, executor::block_on};

const RENAME_BEFORE: &str = "def order_total(items, tax_rate):\n    total = 0\n    for item in items:\n        total += item.price * item.quantity\n    tax = total * tax_rate\n    return total + tax\n\n\ndef describe(order):\n    return f\"{order.id}: {len(order.items)} items\"\n";

const COMPLETE_BEFORE: &str = "class Stack:\n    def __init__(self):\n        self.items = []\n\n    def push(self, item):\n        self.items.append(item)\n\n    def pop(self):\n        return self.items.pop()\n\n    def peek(self):\n";

const PAUSE: Duration = Duration::from_millis(2500);

fn thread_timer() -> Timer {
    Arc::new(|duration| {
        let (sender, receiver) = oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            sender.send(()).ok();
        });
        receiver.map(|_| ()).boxed()
    })
}

struct Scenario {
    name: &'static str,
    before: String,
    after: String,
    cursor: usize,
    language: Language,
}

fn scenarios() -> Vec<Scenario> {
    let position = RENAME_BEFORE.find("total = 0").expect("rename anchor");
    let mut renamed = RENAME_BEFORE.to_string();
    renamed.insert_str(position, "sub");
    let mut completed = COMPLETE_BEFORE.to_string();
    completed.push_str("        return self.");
    let rename = |language| Scenario {
        name: "rename",
        before: RENAME_BEFORE.to_string(),
        after: renamed.clone(),
        cursor: position + 3,
        language,
    };
    vec![
        rename(Language::Python),
        rename(Language::Python),
        rename(Language::Python),
        Scenario {
            name: "complete",
            before: COMPLETE_BEFORE.to_string(),
            cursor: completed.len(),
            after: completed.clone(),
            language: Language::Python,
        },
        Scenario {
            name: "complete",
            before: COMPLETE_BEFORE.to_string(),
            cursor: completed.len(),
            after: completed,
            language: Language::Python,
        },
        Scenario {
            name: "rename (LANGUAGE_UNSPECIFIED)",
            ..rename(Language::Unspecified)
        },
    ]
}

#[test]
#[ignore = "talks to the real Windsurf cloud; needs WINDSURF_API_KEY"]
fn live_get_tab() {
    let Some(api_key) = std::env::var("WINDSURF_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        eprintln!("WINDSURF_API_KEY is not set; skipping");
        return;
    };
    let http = Arc::new(reqwest_client::ReqwestClient::new());
    let backend = NativeBackend::new(http, thread_timer(), ClientConfig::default());
    let uri = "file:///tmp/windsurf-live/order.py";

    let mut latencies = Vec::new();
    for (index, scenario) in scenarios().into_iter().enumerate() {
        if index > 0 {
            std::thread::sleep(PAUSE);
        }
        let step = edit_step(
            uri,
            &scenario.before,
            &scenario.after,
            Some(scenario.before.clone()),
            SystemTime::now(),
        )
        .expect("the scenario has an edit");
        let cursor_row = scenario.after[..scenario.cursor].matches('\n').count() as u32;
        let window = selection_window(&scenario.after, cursor_row);
        let input = TabRequestInput {
            document: DocumentContext {
                text: scenario.after.clone(),
                absolute_uri: uri.to_string(),
                workspace_uri: "file:///tmp/windsurf-live".to_string(),
                language: scenario.language,
                editor_language: "python".to_string(),
                cursor_offset: scenario.cursor,
                selection: window.text.clone(),
                selection_start_line: window.start_line,
                selection_end_line: window.end_line,
                is_dirty: true,
                cursor_position: None,
            },
            diagnostics: Vec::new(),
            steps: vec![step],
            mainline_trajectory_id: uuid::Uuid::new_v4().to_string(),
            cached_hashes: Vec::new(),
            feedback: Vec::new(),
            trigger_id: uuid::Uuid::new_v4().to_string(),
        };
        let exchange = block_on(backend.get_tab(api_key.clone(), input));
        let stream = match exchange.result {
            Ok(stream) => stream,
            Err(error) => panic!("{}: GetTab failed: {error}", scenario.name),
        };
        latencies.push(exchange.elapsed);
        println!(
            "[{index}] {}: {} ms total, first delta after {:?} ms, {} deltas, stop reason {:?}",
            scenario.name,
            exchange.elapsed.as_millis(),
            exchange
                .first_delta_after
                .map(|duration| duration.as_millis()),
            stream.deltas,
            stream.stop_reason
        );
        assert_eq!(
            stream.old_text, window.text,
            "old text should be the window"
        );
        match prediction_edits(
            &scenario.after,
            window.byte_range.clone(),
            &stream.old_text,
            &stream.new_text,
        ) {
            Ok(edits) if edits.is_empty() => println!("    no prediction"),
            Ok(edits) => {
                for (range, text) in edits {
                    println!(
                        "    replace {:?} ({:?}) with {:?}",
                        range,
                        &scenario.after[range.clone()],
                        text
                    );
                }
            }
            Err(error) => panic!(
                "{}: couldn't apply the prediction: {error:?}",
                scenario.name
            ),
        }
    }
    latencies.sort();
    println!(
        "GetTab latency (includes GetUserJwt on the first call): min {} ms, median {} ms, max {} ms",
        latencies[0].as_millis(),
        latencies[latencies.len() / 2].as_millis(),
        latencies[latencies.len() - 1].as_millis()
    );
}

/// Stage 5: the second request sends the first step as the hash the server
/// returned, a predictive request adds an assumed step, and one
/// `ProvideFeedback` is sent. Three `GetTab` and one `ProvideFeedback`, at
/// least 2.5 seconds apart.
#[test]
#[ignore = "talks to the real Windsurf cloud; needs WINDSURF_API_KEY"]
fn live_trajectory_cache_and_prediction() {
    let Some(api_key) = std::env::var("WINDSURF_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        eprintln!("WINDSURF_API_KEY is not set; skipping");
        return;
    };
    let http = Arc::new(reqwest_client::ReqwestClient::new());
    let backend = NativeBackend::new(http, thread_timer(), ClientConfig::default());
    let uri = "file:///tmp/windsurf-live/order.py";
    let mainline = uuid::Uuid::new_v4().to_string();

    let position = RENAME_BEFORE.find("total = 0").expect("rename anchor");
    let mut first_text = RENAME_BEFORE.to_string();
    first_text.insert_str(position, "sub");
    let mut second_text = first_text.clone();
    let second_position = second_text.find("total += item").expect("second anchor");
    second_text.insert_str(second_position, "sub");
    let first_step = edit_step(
        uri,
        RENAME_BEFORE,
        &first_text,
        Some(RENAME_BEFORE.to_string()),
        SystemTime::now(),
    )
    .expect("first edit");
    let second_step =
        edit_step(uri, &first_text, &second_text, None, SystemTime::now()).expect("second edit");

    let input = |text: &str,
                 cursor: usize,
                 steps: Vec<_>,
                 cached_hashes: Vec<u64>,
                 cursor_position: Option<(u32, u32)>| {
        let cursor_row = text[..cursor].matches('\n').count() as u32;
        let window = selection_window(text, cursor_row);
        TabRequestInput {
            document: DocumentContext {
                text: text.to_string(),
                absolute_uri: uri.to_string(),
                workspace_uri: "file:///tmp/windsurf-live".to_string(),
                language: Language::Python,
                editor_language: "python".to_string(),
                cursor_offset: cursor,
                selection: window.text.clone(),
                selection_start_line: window.start_line,
                selection_end_line: window.end_line,
                is_dirty: true,
                cursor_position,
            },
            diagnostics: Vec::new(),
            steps,
            mainline_trajectory_id: mainline.clone(),
            cached_hashes,
            feedback: Vec::new(),
            trigger_id: uuid::Uuid::new_v4().to_string(),
        }
    };
    let report = |label: &str, exchange: &fork_windsurf::backend::TabExchange| {
        let stream = match &exchange.result {
            Ok(stream) => stream,
            Err(error) => panic!("{label}: GetTab failed: {error}"),
        };
        println!(
            "{label}: {} ms, request {} bytes, resent full trajectory: {}, returned {} hashes, new text {} chars",
            exchange.elapsed.as_millis(),
            exchange.request_bytes,
            exchange.resent_full_trajectory,
            stream
                .cache_hashes
                .as_ref()
                .map(Vec::len)
                .unwrap_or_default(),
            stream.new_text.chars().count(),
        );
        stream.clone()
    };

    let first = block_on(backend.get_tab(
        api_key.clone(),
        input(
            &first_text,
            position + 3,
            vec![first_step.clone()],
            Vec::new(),
            None,
        ),
    ));
    let first_stream = report("full", &first);
    let first_hashes = first_stream.cache_hashes.expect("cache hashes");
    assert_eq!(first_hashes.len(), 1);

    std::thread::sleep(PAUSE);
    let second = block_on(backend.get_tab(
        api_key.clone(),
        input(
            &second_text,
            second_position + 3,
            vec![first_step.clone(), second_step.clone()],
            first_hashes,
            None,
        ),
    ));
    let second_stream = report("cached", &second);
    assert!(
        !second.resent_full_trajectory,
        "the server should know the hash of the first step"
    );
    assert!(second.request_bytes < first.request_bytes);
    let second_hashes = second_stream.cache_hashes.clone().expect("cache hashes");
    assert_eq!(second_hashes.len(), 2);

    // Predictive: as if the prediction had been accepted.
    let window = selection_window(
        &second_text,
        second_text[..second_position].matches('\n').count() as u32,
    );
    let edits = prediction_edits(
        &second_text,
        window.byte_range,
        &second_stream.old_text,
        &second_stream.new_text,
    )
    .expect("prediction applies");
    let mut assumed_text = second_text.clone();
    let mut assumed_cursor = second_position + 3;
    for (range, text) in edits.iter().rev() {
        assumed_text.replace_range(range.clone(), text);
    }
    if let Some((range, text)) = edits.last() {
        let delta: isize = edits
            .iter()
            .take(edits.len() - 1)
            .map(|(range, text)| text.len() as isize - range.len() as isize)
            .sum();
        assumed_cursor = (range.start as isize + delta) as usize + text.len();
    }
    let assumed_step = edit_step(uri, &second_text, &assumed_text, None, SystemTime::now())
        .unwrap_or_else(|| second_step.clone());
    let before_cursor = &assumed_text[..assumed_cursor];
    let row = before_cursor.matches('\n').count() as u32;
    let column = before_cursor
        .rsplit('\n')
        .next()
        .map(|line| line.chars().count() as u32)
        .unwrap_or_default();
    std::thread::sleep(PAUSE);
    let predictive = block_on(backend.get_tab(
        api_key.clone(),
        input(
            &assumed_text,
            assumed_cursor,
            vec![first_step, second_step, assumed_step],
            second_hashes,
            Some((row, column)),
        ),
    ));
    report("predictive", &predictive);
    assert!(!predictive.resent_full_trajectory);

    std::thread::sleep(PAUSE);
    let prompt_id = second
        .request
        .as_ref()
        .map(|request| request.prompt_id.clone())
        .expect("request");
    let feedback = block_on(
        backend.provide_feedback(
            api_key,
            FeedbackReport {
                prompt_id,
                completion_id: uuid::Uuid::new_v4().to_string(),
                accepted: true,
                from_cache: false,
                feedback_delay_ms: 800,
                timestamp_unix_ms: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or_default(),
            },
        ),
    );
    println!("ProvideFeedback: {feedback:?}");
    assert!(feedback.is_ok());
}
