//! Which trajectory steps the server already knows. The client doesn't compute
//! hashes: `PreResponse.cache_info_v2.hashes` returns one hash per step that
//! was sent, and the next request sends those hashes back in place of the
//! steps (`fork/spike/结论.md` 问题 10–12).
//!
//! Each hash is remembered next to a fingerprint of the step it stands for.
//! A request reuses the longest prefix of hashes whose steps are unchanged,
//! which is what the official language server does: after a predictive
//! request (whose last step is an assumed edit) or after the steps were
//! trimmed, only the still-matching prefix is sent as hashes.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};

use crate::proto::exa::cortex_pb::CortexTrajectoryStep;

#[derive(Default)]
pub struct TrajectoryCache {
    /// `(fingerprint of the step, server hash)` for each step sent.
    known: Vec<(u64, u64)>,
}

pub fn fingerprint(step: &CortexTrajectoryStep) -> u64 {
    let mut hasher = DefaultHasher::new();
    prost::Message::encode_to_vec(step).hash(&mut hasher);
    hasher.finish()
}

impl TrajectoryCache {
    /// The hashes to send for `steps`: those of the longest prefix of steps
    /// that are identical to the steps the hashes were returned for.
    pub fn hashes_for(&self, fingerprints: &[u64]) -> Vec<u64> {
        self.known
            .iter()
            .zip(fingerprints)
            .take_while(|((known_fingerprint, _), fingerprint)| known_fingerprint == *fingerprint)
            .map(|((_, hash), _)| *hash)
            .collect()
    }

    /// Records the hashes returned for a request that carried steps with
    /// `fingerprints`.
    pub fn record(&mut self, fingerprints: &[u64], hashes: &[u64]) {
        self.known = fingerprints
            .iter()
            .copied()
            .zip(hashes.iter().copied())
            .collect();
    }

    pub fn clear(&mut self) {
        self.known.clear();
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::proto::exa::cortex_pb::{CortexStepClipboard, cortex_trajectory_step};

    fn step(content: &str) -> CortexTrajectoryStep {
        CortexTrajectoryStep {
            step: Some(cortex_trajectory_step::Step::Clipboard(
                CortexStepClipboard {
                    content: content.to_string(),
                },
            )),
            ..Default::default()
        }
    }

    #[test]
    fn test_longest_unchanged_prefix_is_reused() {
        let fingerprints: Vec<u64> = ["a", "b", "assumed"]
            .map(|name| fingerprint(&step(name)))
            .to_vec();
        let mut cache = TrajectoryCache::default();
        assert!(cache.hashes_for(&fingerprints).is_empty());
        cache.record(&fingerprints, &[10, 20, 30]);
        assert_eq!(cache.hashes_for(&fingerprints), vec![10, 20, 30]);

        // The assumed step was replaced by what the user actually did.
        let actual: Vec<u64> = ["a", "b", "typed", "more"]
            .map(|name| fingerprint(&step(name)))
            .to_vec();
        assert_eq!(cache.hashes_for(&actual), vec![10, 20]);

        // Trimming the front invalidates everything.
        assert!(cache.hashes_for(&actual[1..]).is_empty());

        // Fewer hashes than steps: only those are used.
        cache.record(&actual, &[1]);
        assert_eq!(cache.hashes_for(&actual), vec![1]);
        cache.clear();
        assert!(cache.is_empty());
    }
}
