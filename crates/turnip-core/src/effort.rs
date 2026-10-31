//! Categorical reasoning-effort values accepted on the wire.
//!
//! Every provider offers (a subset of) one ladder; per-model metadata narrows
//! it. A backend with its own vocabulary lists that instead.

use serde::{Deserialize, Serialize};

/// Every categorical effort value turnip can send, lowest to highest.
pub const EFFORT_LADDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

fn effort_rank(level: &str) -> Option<usize> {
    EFFORT_LADDER.iter().position(|l| *l == level)
}

/// The set of effort levels a model accepts. `known` distinguishes an
/// explicitly empty set from absent metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortSet {
    pub values: Vec<String>,
    pub known: bool,
}

impl EffortSet {
    pub fn from_iter<'a>(values: impl Iterator<Item = &'a str>) -> Self {
        let mut set = EffortSet {
            known: true,
            ..Default::default()
        };
        for value in values {
            set.add(value);
        }
        set
    }

    /// Add `level` unless it is empty or a duplicate. The set becomes known
    /// even when the value is rejected. Returns true when stored.
    pub fn add(&mut self, level: &str) -> bool {
        self.known = true;
        if level.is_empty() || self.values.iter().any(|v| v == level) {
            return false;
        }
        self.values.push(level.to_string());
        true
    }

    pub fn has(&self, level: &str) -> bool {
        self.values.iter().any(|v| v == level)
    }

    /// Return `requested` when offered; otherwise the nearest lower known
    /// level, or the nearest higher level when none is lower. Returns `None`
    /// for an empty set or an unrecognized request.
    pub fn clamp<'a>(&'a self, requested: &'a str) -> Option<&'a str> {
        if self.values.is_empty() {
            return None;
        }
        if self.has(requested) {
            return Some(requested);
        }
        let rank = effort_rank(requested);
        let lower = rank.and_then(|r| {
            (0..r)
                .rev()
                .find(|i| self.has(EFFORT_LADDER[*i]))
        });
        if let Some(index) = lower {
            return Some(EFFORT_LADDER[index]);
        }
        rank.and_then(|r| (r + 1..EFFORT_LADDER.len()).find(|i| self.has(EFFORT_LADDER[*i])))
            .map(|i| EFFORT_LADDER[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[&str]) -> EffortSet {
        EffortSet::from_iter(values.iter().copied())
    }

    #[test]
    fn clamp_exact_and_lower() {
        let s = set(&["low", "medium", "high"]);
        assert_eq!(s.clamp("medium"), Some("medium"));
        assert_eq!(s.clamp("xhigh"), Some("high"));
        assert_eq!(s.clamp("bogus"), None);
    }

    #[test]
    fn clamp_falls_up_when_no_lower() {
        let s = set(&["medium", "high"]);
        assert_eq!(s.clamp("minimal"), Some("medium"));
        assert_eq!(s.clamp("none"), Some("medium"));
    }

    #[test]
    fn empty_set_clamps_nothing() {
        let s = set(&[]);
        assert!(s.known);
        assert_eq!(s.clamp("high"), None);
    }

    #[test]
    fn unranked_vocabulary_only_matches_exactly() {
        let s = set(&["turbo", "eco"]);
        assert_eq!(s.clamp("turbo"), Some("turbo"));
        assert_eq!(s.clamp("high"), None);
    }
}
