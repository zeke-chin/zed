use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Context, Entity, Global};
use language::{Buffer, BufferId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConvertHintKind {
    PythonRepr,
    LineBreakRepair,
}

impl ConvertHintKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PythonRepr => "Convert Python repr to JSON",
            Self::LineBreakRepair => "Repair line breaks and convert to JSON",
        }
    }
}

struct Hint {
    version: clock::Global,
    kind: ConvertHintKind,
}

/// Convertible-content hints found at paste time, plus the buffers that have a
/// paste detection or conversion running. A hint is only valid while the
/// buffer is still at the version it was computed for.
#[derive(Default)]
pub(crate) struct ConvertState {
    hints: HashMap<BufferId, Hint>,
    in_flight: HashSet<BufferId>,
}

struct GlobalConvertState(Entity<ConvertState>);

impl Global for GlobalConvertState {}

impl ConvertState {
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalConvertState>() {
            return global.0.clone();
        }
        let state = cx.new(|_| Self::default());
        cx.set_global(GlobalConvertState(state.clone()));
        state
    }

    pub(crate) fn try_begin(&mut self, buffer_id: BufferId) -> bool {
        self.in_flight.insert(buffer_id)
    }

    pub(crate) fn finish(&mut self, buffer_id: BufferId) {
        self.in_flight.remove(&buffer_id);
    }

    pub(crate) fn set_hint(
        &mut self,
        buffer_id: BufferId,
        version: clock::Global,
        kind: ConvertHintKind,
        cx: &mut Context<Self>,
    ) {
        self.hints.insert(buffer_id, Hint { version, kind });
        cx.notify();
    }

    pub(crate) fn clear_hint(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        if self.hints.remove(&buffer_id).is_some() {
            cx.notify();
        }
    }

    pub(crate) fn hint_for(&self, buffer: &Buffer) -> Option<ConvertHintKind> {
        let hint = self.hints.get(&buffer.remote_id())?;
        (hint.version == buffer.version()).then_some(hint.kind)
    }
}
