//! Sends real `GetUserJwt` and `GetTab` requests to the Windsurf cloud. Ignored
//! by default; run it explicitly with an API key in the environment:
//!
//! ```sh
//! WINDSURF_API_KEY=... cargo test -p fork_windsurf --test live_get_tab -- --ignored --nocapture
//! ```
//!
//! Requests are sent one at a time, at least 2.5 seconds apart. The key and
//! token are never printed.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use fork_windsurf::{
    backend::{DocumentContext, TabBackend as _, TabRequestInput},
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
            },
            diagnostics: Vec::new(),
            steps: vec![step],
            mainline_trajectory_id: uuid::Uuid::new_v4().to_string(),
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
