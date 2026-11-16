//! Hover links carry only a short token. The token maps to the buffer, the
//! string value's offset and the buffer version at hover time, so a click can
//! re-read the value and refuse to open it once the buffer has changed.

use std::collections::VecDeque;

use editor::Editor;
use gpui::{App, Global, WeakEntity};
use language::Buffer;

pub(crate) const MAX_PENDING_OPENS: usize = 200;

/// Bounded FIFO from token to entry. Reading doesn't consume an entry, so the
/// same link can be clicked more than once.
pub(crate) struct TokenCache<T> {
    next_token: u64,
    capacity: usize,
    entries: VecDeque<(u64, T)>,
}

impl<T> TokenCache<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            next_token: 0,
            capacity,
            entries: VecDeque::new(),
        }
    }

    pub(crate) fn reserve_token(&mut self) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        token
    }

    pub(crate) fn insert(&mut self, token: u64, entry: T) {
        self.entries.push_back((token, entry));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    pub(crate) fn get(&self, token: u64) -> Option<&T> {
        self.entries
            .iter()
            .find(|(entry_token, _)| *entry_token == token)
            .map(|(_, entry)| entry)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.entries.retain(|(_, entry)| keep(entry));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Clone)]
pub(crate) struct PendingOpen {
    pub editor: WeakEntity<Editor>,
    pub buffer: WeakEntity<Buffer>,
    pub version: clock::Global,
    pub offset: usize,
}

pub(crate) struct PendingOpens(TokenCache<PendingOpen>);

impl Default for PendingOpens {
    fn default() -> Self {
        Self(TokenCache::new(MAX_PENDING_OPENS))
    }
}

impl Global for PendingOpens {}

pub(crate) fn reserve_token(cx: &mut App) -> u64 {
    cx.default_global::<PendingOpens>().0.reserve_token()
}

/// Entries of released buffers are dropped here, so closing a buffer frees its
/// entries on the next hover.
pub(crate) fn insert(token: u64, entry: PendingOpen, cx: &mut App) {
    let cache = &mut cx.default_global::<PendingOpens>().0;
    cache.retain(|entry| entry.buffer.upgrade().is_some());
    cache.insert(token, entry);
}

pub(crate) fn get(token: u64, cx: &App) -> Option<PendingOpen> {
    cx.try_global::<PendingOpens>()?.0.get(token).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_an_entry_by_token() {
        let mut cache = TokenCache::new(MAX_PENDING_OPENS);
        let token = cache.reserve_token();
        cache.insert(token, "value");
        assert_eq!(cache.get(token), Some(&"value"));
    }

    #[test]
    fn tokens_are_distinct() {
        let mut cache = TokenCache::new(MAX_PENDING_OPENS);
        let first = cache.reserve_token();
        let second = cache.reserve_token();
        assert_ne!(first, second);
        cache.insert(first, "one");
        cache.insert(second, "two");
        assert_eq!(cache.get(first), Some(&"one"));
        assert_eq!(cache.get(second), Some(&"two"));
    }

    #[test]
    fn reading_does_not_consume() {
        let mut cache = TokenCache::new(MAX_PENDING_OPENS);
        let token = cache.reserve_token();
        cache.insert(token, "keep");
        assert_eq!(cache.get(token), Some(&"keep"));
        assert_eq!(cache.get(token), Some(&"keep"));
    }

    #[test]
    fn unknown_token_is_none() {
        let cache = TokenCache::<&str>::new(MAX_PENDING_OPENS);
        assert_eq!(cache.get(12345), None);
    }

    #[test]
    fn evicts_oldest_past_capacity_and_keeps_newest() {
        let mut cache = TokenCache::new(MAX_PENDING_OPENS);
        let first = cache.reserve_token();
        cache.insert(first, 0);
        let mut last = first;
        for index in 1..=MAX_PENDING_OPENS {
            last = cache.reserve_token();
            cache.insert(last, index);
        }
        assert_eq!(cache.get(first), None);
        assert_eq!(cache.get(last), Some(&MAX_PENDING_OPENS));
        assert_eq!(cache.len(), MAX_PENDING_OPENS);
    }

    #[test]
    fn retain_drops_rejected_entries() {
        let mut cache = TokenCache::new(MAX_PENDING_OPENS);
        for index in 0..4 {
            let token = cache.reserve_token();
            cache.insert(token, index);
        }
        cache.retain(|value| value % 2 == 0);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(1), None);
        assert_eq!(cache.get(2), Some(&2));
    }
}
