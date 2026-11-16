//! Sends real Tab Jump `GetChatMessage` requests, assembled by
//! `tab_jump::prompt`, to the Windsurf cloud. Ignored by default; run it
//! explicitly with an API key in the environment:
//!
//! ```sh
//! WINDSURF_API_KEY=... cargo test -p fork_windsurf --test live_tab_jump -- --ignored --nocapture
//! ```
//!
//! Three requests, one at a time, at least 2.5 seconds apart. The key and
//! token are never printed.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use fork_windsurf::{
    backend::TabBackend as _,
    native::{ClientConfig, NativeBackend, connect::Timer},
    proto::exa::codeium_common_pb::Language,
    tab_jump::{
        prompt::{ActiveDocument, PromptInput, build_prompt, build_request, history_from_steps},
        response::{locate, parse_completion, split_cursor_marker},
    },
    trajectory::edit_step,
};
use futures::{FutureExt as _, channel::oneshot, executor::block_on};

const PAUSE: Duration = Duration::from_millis(2500);
const PATH: &str = "/tmp/windsurf-live/inventory.py";

const INVENTORY: &str = "class Inventory:\n    def __init__(self):\n        self.items = {}\n\n    def add_item(self, name, count):\n        self.items[name] = self.items.get(name, 0) + count\n\n    def remove_item(self, name, count):\n        self.items[name] = self.items.get(name, 0) - count\n\n    def total(self):\n        return sum(self.items.values())\n\n\ndef restock(inventory):\n    inventory.add_item(\"apple\", 10)\n    inventory.add_item(\"pear\", 5)\n\n\ndef report(inventory):\n    print(\"items:\", inventory.total())\n";

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
    after: String,
    /// Where the cursor is, as a needle the cursor sits right after.
    cursor_after: &'static str,
    /// Text the jump should land in, if the model follows the edit.
    expected_target: &'static str,
}

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "rename method, call sites follow",
            after: INVENTORY.replace("def add_item(", "def add_stock("),
            cursor_after: "def add_stock",
            expected_target: "add_item(",
        },
        Scenario {
            name: "new parameter, call sites follow",
            after: INVENTORY.replace(
                "def restock(inventory):",
                "def restock(inventory, multiplier):",
            ),
            cursor_after: "multiplier",
            expected_target: "inventory.add_item(",
        },
        Scenario {
            name: "rename attribute",
            after: INVENTORY.replacen("self.items = {}", "self.stock = {}", 1),
            cursor_after: "self.stock",
            expected_target: "self.items",
        },
    ]
}

#[test]
#[ignore = "talks to the real Windsurf cloud; needs WINDSURF_API_KEY"]
fn live_tab_jump() {
    let Some(api_key) = std::env::var("WINDSURF_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        eprintln!("WINDSURF_API_KEY is not set; skipping");
        return;
    };
    let http = Arc::new(reqwest_client::ReqwestClient::new());
    let backend = NativeBackend::new(http, thread_timer(), ClientConfig::default());
    let uri = format!("file://{PATH}");

    let mut followed = 0;
    for (index, scenario) in scenarios().into_iter().enumerate() {
        if index > 0 {
            std::thread::sleep(PAUSE);
        }
        let step = edit_step(
            &uri,
            INVENTORY,
            &scenario.after,
            Some(INVENTORY.to_string()),
            SystemTime::now(),
        )
        .expect("the scenario has an edit");
        let cursor_offset = scenario
            .after
            .find(scenario.cursor_after)
            .expect("cursor needle")
            + scenario.cursor_after.len();
        let prompt = build_prompt(&PromptInput {
            active: ActiveDocument {
                path: PATH.to_string(),
                language: Language::Python,
                editor_language: "python".to_string(),
                text: scenario.after.clone(),
                cursor_offset,
                code_item: None,
            },
            other_documents: Vec::new(),
            history: history_from_steps(&[step]),
            presented_at: "Nov 16, 2026 at 9:00am, UTC-05:00".to_string(),
        });
        let exchange = block_on(backend.get_chat_message(api_key.clone(), build_request(prompt)));
        let stream = match exchange.result {
            Ok(stream) => stream,
            Err(error) => panic!("{}: GetChatMessage failed: {error}", scenario.name),
        };
        println!(
            "[{index}] {}: {} ms, first delta after {:?} ms, {} deltas, stop reason {:?}",
            scenario.name,
            exchange.elapsed.as_millis(),
            exchange
                .first_delta_after
                .map(|duration| duration.as_millis()),
            stream.deltas,
            stream.stop_reason,
        );
        println!("    completion: {:?}", stream.text);
        let completion = match parse_completion(&stream.text) {
            Ok(Some(completion)) => completion,
            Ok(None) => {
                println!("    no jump");
                continue;
            }
            Err(error) => panic!("{}: unreadable completion: {error:?}", scenario.name),
        };
        match locate(&scenario.after, &completion.target_content) {
            Ok(found) => {
                let (target_text, _) = split_cursor_marker(&completion.target_content);
                let cursor = found.cursor.unwrap_or(found.range.start);
                let row = scenario.after[..cursor].matches('\n').count();
                println!(
                    "    jump to row {row}: {target_text:?} -> {:?}",
                    completion.replacement_content
                );
                if target_text.contains(scenario.expected_target) {
                    followed += 1;
                }
            }
            Err(error) => println!("    target not usable: {error:?}"),
        }
    }
    println!("{followed} of 3 jumps went where the edit implies");
    assert!(followed > 0, "no jump followed the edit");
}
