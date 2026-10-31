//! Model metadata reported by providers, plus version-aware model sorting for
//! pickers.

use serde::{Deserialize, Serialize};

use crate::effort::EffortSet;

/// Tri-state capability answer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    #[default]
    Unknown,
    Yes,
    No,
}

impl Capability {
    /// Unknown is treated as yes for image input because rejecting an image
    /// is recoverable, while suppressing one silently loses content.
    pub fn permits(self) -> bool {
        !matches!(self, Capability::No)
    }

    pub fn from_bool(value: bool) -> Self {
        if value {
            Capability::Yes
        } else {
            Capability::No
        }
    }
}

/// Raw metadata reported by a provider for one model. Every field after `id`
/// is optional; unknown sentinels are `None`/`Unknown`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Exact wire id; empty in metadata-only merged views.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Served context window in tokens; 0 = unknown.
    #[serde(default)]
    pub context_window: i64,
    /// Provider-declared ceiling for context overrides; 0 = unknown.
    #[serde(default)]
    pub max_context: i64,
    /// Maximum output tokens per response; 0 = unknown.
    #[serde(default)]
    pub max_output: i64,
    #[serde(default)]
    pub image_input: Capability,
    #[serde(default)]
    pub tools: Capability,
    /// Provider-reported USD rates per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cache_write_1h: Option<f64>,
    #[serde(default)]
    pub efforts: EffortSet,
}

impl ModelInfo {
    pub fn new(id: impl Into<String>) -> Self {
        ModelInfo {
            id: id.into(),
            ..Default::default()
        }
    }
}

/// Version-aware model sort for pickers: chunked natural comparison where digit
/// runs compare numerically, familiar model families rank usefully, and ties
/// fall back to plain byte order. Returns an ordering sorting "best first".
pub fn compare_model_ids(a: &str, b: &str) -> std::cmp::Ordering {
    family_rank(a)
        .cmp(&family_rank(b))
        .then_with(|| natural_cmp(a, b))
}

fn family_rank(id: &str) -> u8 {
    let id = id.to_ascii_lowercase();
    // Preferred families first, then known-but-less-preferred, then unknown.
    const PREFERRED: &[&str] = &[
        "sonnet", "opencoder", "gpt-5", "gpt-4.1", "gpt-4o", "o3", "o4-mini", "gemini-3",
        "gemini-2.5", "grok-4", "kimi-k2", "qwen3-coder", "glm-4.6", "deepseek-chat",
        "deepseek-reasoner", "grok-code", "claude-opus", "claude-haiku",
    ];
    for (index, family) in PREFERRED.iter().enumerate() {
        if id.contains(family) {
            return index as u8;
        }
    }
    200
}

/// Natural comparison: digit runs compare numerically, other runs bytewise.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let a_num = take_digits(&mut a);
                let b_num = take_digits(&mut b);
                let ord = a_num
                    .len()
                    .cmp(&b_num.len())
                    .then_with(|| a_num.cmp(b_num));
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.cmp(y);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn take_digits<'a>(bytes: &mut &'a [u8]) -> &'a [u8] {
    let end = bytes
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    let digits = &bytes[..end];
    *bytes = &bytes[end..];
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_prefers_newer_versions() {
        assert_eq!(natural_cmp("gpt-4o-2024-11-20", "gpt-4o-2024-05-13"), std::cmp::Ordering::Greater);
        assert_eq!(natural_cmp("qwen3:8b", "qwen3:14b"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("m2", "m10"), std::cmp::Ordering::Less);
        assert_eq!(natural_cmp("abc", "abd"), std::cmp::Ordering::Less);
    }

    #[test]
    fn sort_is_best_first() {
        let mut models = vec!["llama3:8b".to_string(), "gpt-5.2".to_string()];
        models.sort_by(|a, b| compare_model_ids(a, b));
        assert_eq!(models[0], "gpt-5.2");
    }
}
