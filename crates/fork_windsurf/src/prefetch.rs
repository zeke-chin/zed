//! Predictive requests. Right after a prediction is shown, one more `GetTab`
//! is sent as if the prediction had been accepted: the document is the buffer
//! with the prediction applied, the cursor is where accepting leaves it
//! (`input_document.cursor_position` is set, as in the official client's
//! predictive requests) and the trajectory has the accepting edit as one more
//! step. When the user accepts the prediction, or types it out, the buffer
//! reads exactly like that document and the response is shown without a new
//! request (`fork/spike/结论.md` 问题 20).
//!
//! Request volume: each shown prediction starts at most one predictive
//! request, and showing a prefetched prediction is itself the answer to a
//! trigger, so a trigger costs at most two `GetTab` requests (the official
//! client sends one to three). A predictive request is dropped (cancelling
//! the HTTP request) as soon as the user does something else, and none is
//! sent while backing off or after an error.

use std::{sync::Arc, time::Duration};

use gpui::{AppContext as _, Context, Entity, EntityId, Task, WeakEntity};
use language::{Anchor, Buffer, BufferSnapshot, ToOffset as _};

use settings::Settings as _;

use crate::{
    PreparedRequest, RequestOutcome, WindsurfSettings,
    delegate::{ResponseForBuffer, WindsurfDelegate, prepare_prediction},
    proto::exa::codeium_common_pb::SupercompleteTriggerCondition,
    tab_jump,
};

pub(crate) struct Prefetch {
    buffer_id: EntityId,
    buffer: WeakEntity<Buffer>,
    /// The buffer text the response is for.
    assumed_text: Arc<str>,
    generation: u64,
    state: PrefetchState,
}

enum PrefetchState {
    Pending {
        _task: Task<()>,
        /// Set when a trigger arrived for the assumed text before the
        /// response: the response is shown when it arrives.
        awaited_cursor: Option<Anchor>,
    },
    /// `None` when the request failed.
    Ready(Option<ResponseForBuffer>),
}

pub(crate) enum PrefetchUse {
    /// The prefetched response answers this trigger.
    Used,
    /// The predictive request for this buffer state is still in flight.
    Waiting,
    Unavailable,
}

/// The text of `snapshot` with `edits` applied, and the offset just after the
/// last edit, which is where accepting leaves the cursor.
pub(crate) fn apply_edits(
    snapshot: &BufferSnapshot,
    edits: &[(std::ops::Range<Anchor>, Arc<str>)],
) -> (String, usize) {
    let mut text = snapshot.text();
    let mut ranges: Vec<_> = edits
        .iter()
        .map(|(range, new_text)| {
            (
                range.start.to_offset(snapshot)..range.end.to_offset(snapshot),
                new_text.clone(),
            )
        })
        .collect();
    ranges.sort_by_key(|(range, _)| range.start);
    let mut cursor = 0;
    let mut delta: isize = 0;
    for (range, new_text) in &ranges {
        let start = (range.start as isize + delta) as usize;
        let end = (range.end as isize + delta) as usize;
        text.replace_range(start..end, new_text);
        delta += new_text.len() as isize - range.len() as isize;
        cursor = start + new_text.len();
    }
    (text, cursor)
}

impl WindsurfDelegate {
    /// Starts the predictive request for the prediction just installed.
    /// Replaces any earlier one.
    pub(crate) fn start_prefetch(
        &mut self,
        buffer: &Entity<Buffer>,
        assumed_text: String,
        assumed_cursor: usize,
        trigger_id: &str,
        cx: &mut Context<Self>,
    ) {
        self.prefetch = None;
        let prepared = self.windsurf.update(cx, |windsurf, cx| {
            windsurf.prepare_predictive_request(
                buffer,
                &assumed_text,
                assumed_cursor,
                trigger_id,
                cx,
            )
        });
        let Some(prepared) = prepared else {
            return;
        };
        self.next_prefetch_generation += 1;
        let generation = self.next_prefetch_generation;
        let windsurf = self.windsurf.clone();
        let task = cx.spawn(async move |this, cx| {
            let PreparedRequest {
                backend,
                api_key,
                input,
                window,
                abs_path,
                details,
            } = prepared;
            let exchange = cx.background_spawn(backend.get_tab(api_key, input)).await;
            let outcome = if exchange.result.is_ok() {
                RequestOutcome::Prefetched
            } else {
                RequestOutcome::Failed
            };
            windsurf.update(cx, |windsurf, cx| {
                windsurf.finish_request(0, &details, &exchange, outcome, cx)
            });
            let prompt_id = exchange
                .request
                .as_ref()
                .map(|request| request.prompt_id.clone())
                .unwrap_or_default();
            let response = exchange.result.ok().map(|stream| ResponseForBuffer {
                stream,
                window,
                abs_path,
                prompt_id,
                document: details.document,
                from_cache: true,
            });
            this.update(cx, |this, cx| {
                this.prefetch_finished(generation, response, cx)
            })
            .ok();
        });
        self.prefetch = Some(Prefetch {
            buffer_id: buffer.entity_id(),
            buffer: buffer.downgrade(),
            assumed_text: assumed_text.into(),
            generation,
            state: PrefetchState::Pending {
                _task: task,
                awaited_cursor: None,
            },
        });
    }

    fn prefetch_finished(
        &mut self,
        generation: u64,
        response: Option<ResponseForBuffer>,
        cx: &mut Context<Self>,
    ) {
        let Some(prefetch) = self
            .prefetch
            .as_mut()
            .filter(|prefetch| prefetch.generation == generation)
        else {
            return;
        };
        let state = std::mem::replace(&mut prefetch.state, PrefetchState::Ready(response));
        let PrefetchState::Pending {
            awaited_cursor: Some(cursor),
            ..
        } = state
        else {
            return;
        };
        let Some(buffer) = prefetch.buffer.upgrade() else {
            self.prefetch = None;
            return;
        };
        match self.use_prefetch(&buffer, cursor, cx) {
            PrefetchUse::Used | PrefetchUse::Waiting => {}
            PrefetchUse::Unavailable => {
                // The predictive request failed or the user moved on while
                // waiting: answer the trigger with a request of its own.
                self.prefetch = None;
                self.start_request(
                    buffer,
                    cursor,
                    Duration::ZERO,
                    SupercompleteTriggerCondition::SupercompleteAccept,
                    cx,
                );
            }
        }
    }

    /// Answers a trigger with the predictive request's response when the
    /// buffer reads as the request assumed.
    pub(crate) fn use_prefetch(
        &mut self,
        buffer: &Entity<Buffer>,
        cursor: Anchor,
        cx: &mut Context<Self>,
    ) -> PrefetchUse {
        let Some(prefetch) = self.prefetch.as_mut() else {
            return PrefetchUse::Unavailable;
        };
        if prefetch.buffer_id != buffer.entity_id() {
            return PrefetchUse::Unavailable;
        }
        let matches = {
            let buffer = buffer.read(cx);
            buffer.len() == prefetch.assumed_text.len()
                && buffer.text().as_str() == &*prefetch.assumed_text
        };
        if !matches {
            return PrefetchUse::Unavailable;
        }
        if let PrefetchState::Pending { awaited_cursor, .. } = &mut prefetch.state {
            *awaited_cursor = Some(cursor);
            self.cancel_pending_request(cx);
            return PrefetchUse::Waiting;
        }
        let Some(Prefetch {
            state: PrefetchState::Ready(Some(response)),
            ..
        }) = self.prefetch.take()
        else {
            return PrefetchUse::Unavailable;
        };
        log::debug!("windsurf: answering the trigger with the predictive request's response");
        self.windsurf
            .update(cx, |windsurf, _| windsurf.count_predictive_hit());
        self.cancel_pending_request(cx);
        let request_id = self
            .windsurf
            .update(cx, |windsurf, _| windsurf.next_request_id());
        let buffer = buffer.clone();
        let windsurf = self.windsurf.clone();
        let project = self.project.clone();
        let task = cx.spawn(async move |this, cx| {
            let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
            let prediction = prepare_prediction(
                &buffer,
                &snapshot,
                &response.window,
                &response.stream,
                &response.abs_path,
                cx,
            )
            .await;
            if let Err(reason) = &prediction {
                log::debug!("windsurf: dropping the predictive response: {reason}");
            }
            // Nothing to edit here: like any other trigger without an edit
            // prediction, ask Tab Jump where to go next.
            let jump = match &prediction {
                Ok(Some(_)) => None,
                _ => {
                    let started = windsurf.read_with(cx, |windsurf, cx| {
                        let api_key = WindsurfSettings::get_global(cx).api_key.clone();
                        api_key.map(|api_key| (windsurf.backend(), api_key))
                    });
                    match started.and_then(|(backend, api_key)| {
                        tab_jump::start(
                            &windsurf, project, &buffer, cursor, request_id, backend, api_key, cx,
                        )
                    }) {
                        Some(tab_jump) => tab_jump.await,
                        None => None,
                    }
                }
            };
            this.update(cx, |this, cx| {
                this.finish_pending(request_id);
                if let Ok(Some((edits, edit_preview))) = prediction {
                    let trigger_id = uuid::Uuid::new_v4().to_string();
                    this.install(
                        &buffer,
                        snapshot,
                        edits,
                        edit_preview,
                        response,
                        request_id,
                        &trigger_id,
                        cx,
                    );
                } else if let Some(jump) = jump {
                    this.current = Some(jump);
                }
                cx.notify();
            })
            .ok();
        });
        self.pending_request = Some((request_id, task));
        PrefetchUse::Used
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    use super::*;

    #[gpui::test]
    fn test_apply_edits_tracks_offsets(cx: &mut TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("one two three", cx));
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let edits: Vec<(std::ops::Range<Anchor>, Arc<str>)> = vec![
                (
                    snapshot.anchor_after(8)..snapshot.anchor_before(13),
                    "3".into(),
                ),
                (
                    snapshot.anchor_after(0)..snapshot.anchor_before(3),
                    "1".into(),
                ),
            ];
            let (text, cursor) = apply_edits(&snapshot, &edits);
            assert_eq!(text, "1 two 3");
            assert_eq!(cursor, text.len());
        });
    }
}
