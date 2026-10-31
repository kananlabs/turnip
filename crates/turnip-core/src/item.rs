//! Provider-independent conversation records.
//!
//! The conversation is a flat log of [`Item`]s. Adapters translate items into
//! their wire format when a request is built (adjacent items may be grouped
//! into compound messages when the protocol requires it) and turn stream
//! events back into new items. This mirrors hax's design: one canonical
//! representation that persistence, rendering, and every provider share.

use serde::{Deserialize, Serialize};

/// Why a synthetic item exists. Zero is an ordinary, externally authored item.
/// Agent-authored provenance cannot be inferred from content because users,
/// models, and tools may reproduce the same marker text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemOrigin {
    #[default]
    None,
    /// Synthetic user message summarizing the history before it.
    CompactSeed,
    /// Synthetic user message appended after an interrupted turn.
    Continuation,
    /// Assistant message or killed tool result cut short by an abort.
    Interrupted,
    /// Tool result for a call that did not run after an abort.
    Skipped,
    /// Tool result for a call disabled by the frontend.
    Refused,
    /// Tool result standing in for separately displayed output.
    Summarized,
    /// Synthetic user message reporting finished background tasks.
    TaskNote,
    /// Usage footer of a summarization request, not an agent turn.
    Compaction,
}

/// One inline image carried by an item: base64 payload plus the metadata
/// adapters, display, and persistence need. Dimensions are parsed from the
/// image header at ingestion (0 = unknown).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemImage {
    pub mime: String,
    pub data_b64: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// One conversation record. Variants mirror hax's item kinds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ItemImage>,
        #[serde(default, skip_serializing_if = "is_default_origin")]
        origin: ItemOrigin,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "is_default_origin")]
        origin: ItemOrigin,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        /// JSON object literal (not necessarily valid while streaming).
        arguments: String,
    },
    ToolResult {
        call_id: String,
        output: String,
        /// Length of a trailing model-only annotation in `output`;
        /// user-facing rendering omits it.
        #[serde(default, skip_serializing_if = "is_zero")]
        hidden_tail: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ItemImage>,
        #[serde(default, skip_serializing_if = "is_default_origin")]
        origin: ItemOrigin,
    },
    /// May carry opaque provider state (ready to send back unchanged),
    /// displayable text, or both.
    Reasoning {
        #[serde(rename = "reasoning_json", skip_serializing_if = "Option::is_none")]
        json: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The wire member reasoning text streamed in on (Chat Completions);
        /// needed to replay it on the next request.
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Fieldless marker before each model request; adapters do not serialize it
    /// to the wire, but session files do.
    Boundary,
    /// Usage for the preceding model request.
    Usage(Box<TurnUsage>),
}

fn is_default_origin(origin: &ItemOrigin) -> bool {
    *origin == ItemOrigin::None
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl Item {
    pub fn user(text: impl Into<String>) -> Item {
        Item::User {
            text: text.into(),
            images: Vec::new(),
            origin: ItemOrigin::None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Item {
        Item::Assistant {
            text: text.into(),
            origin: ItemOrigin::None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, output: impl Into<String>) -> Item {
        Item::ToolResult {
            call_id: call_id.into(),
            output: output.into(),
            hidden_tail: 0,
            images: Vec::new(),
            origin: ItemOrigin::None,
        }
    }

    pub fn kind(&self) -> ItemKind {
        match self {
            Item::User { .. } => ItemKind::UserMessage,
            Item::Assistant { .. } => ItemKind::AssistantMessage,
            Item::ToolCall { .. } => ItemKind::ToolCall,
            Item::ToolResult { .. } => ItemKind::ToolResult,
            Item::Reasoning { .. } => ItemKind::Reasoning,
            Item::Boundary => ItemKind::TurnBoundary,
            Item::Usage(_) => ItemKind::TurnUsage,
        }
    }

    pub fn origin(&self) -> ItemOrigin {
        match self {
            Item::User { origin, .. }
            | Item::Assistant { origin, .. }
            | Item::ToolResult { origin, .. } => *origin,
            Item::Usage(usage) => usage.origin,
            _ => ItemOrigin::None,
        }
    }

    /// True for a prompt the user typed: synthetic compaction, continuation,
    /// and task-note messages are excluded. This is the unit /undo and /fork
    /// count, in memory and in the session file.
    pub fn is_typed_prompt(&self) -> bool {
        matches!(self, Item::User { origin: ItemOrigin::None, .. })
    }

    /// Base64 bytes and count of images carried by the item.
    pub fn image_stats(&self) -> (usize, usize) {
        let images = match self {
            Item::User { images, .. } | Item::ToolResult { images, .. } => images,
            _ => return (0, 0),
        };
        (
            images.iter().map(|i| i.data_b64.len()).sum(),
            images.len(),
        )
    }
}

/// Item kinds, for match ergonomics and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    Reasoning,
    TurnBoundary,
    TurnUsage,
}

/// Usage reported for one response. `None` means unreported. `cached_tokens`
/// and `cache_write_tokens` are normally subsets of `input_tokens`, but
/// providers may report overlapping cache reads and writes; consumers must not
/// assume those categories subtract cleanly. `cost` is a provider-reported USD
/// charge, never an estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StreamUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cache_write_1h_tokens: Option<i64>,
    pub cost: Option<f64>,
}

impl StreamUsage {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add the fields of `other` where `self` reports none (a retried attempt
    /// adds what the failed attempt already reported).
    pub fn absorb(&mut self, other: &StreamUsage) {
        macro_rules! add {
            ($f:ident) => {
                if self.$f.is_none() {
                    self.$f = other.$f;
                }
            };
        }
        add!(input_tokens);
        add!(output_tokens);
        add!(cached_tokens);
        add!(cache_write_tokens);
        add!(cache_write_1h_tokens);
        add!(cost);
    }
}

/// Identity of the response behind one turn, as the provider reported it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// May resolve an alias or a router's choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Upstream endpoint a gateway selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
}

/// Display identity recorded with one usage footer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnProvenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

/// Accounting payload stored in a usage item. Costs are USD; `None` is
/// unknown. The total prefers an exact provider-reported cost, with
/// `cost_estimated` marking an estimate instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    pub usage: StreamUsage,
    /// Wall time of this attempt in milliseconds.
    #[serde(default)]
    pub elapsed_ms: i64,
    pub provenance: TurnProvenance,
    #[serde(default, skip_serializing_if = "is_default_origin")]
    pub origin: ItemOrigin,
    #[serde(default)]
    pub cost_total: Option<f64>,
    #[serde(default)]
    pub cost_estimated: bool,
}

/// Keeping at most 20 images preserves Anthropic's larger per-image dimension
/// limit; reserving 20 MiB of base64 under its 32 MB body limit leaves room
/// for prompts, tools, and text history.
pub const IMAGE_REQUEST_BASE64_BUDGET_BYTES: usize = 20 * 1024 * 1024;
pub const IMAGE_REQUEST_MAX_COUNT: usize = 20;

/// Total base64 bytes carried by the item list.
pub fn items_image_base64_bytes(items: &[Item]) -> usize {
    items.iter().map(|i| i.image_stats().0).sum()
}

/// Total image count carried by the item list.
pub fn items_image_count(items: &[Item]) -> usize {
    items.iter().map(|i| i.image_stats().1).sum()
}

/// Index of the newest compaction seed, or 0 when there is none. Compaction
/// summarizes a prefix instead of discarding it, so a request built from
/// `items` starts there, not at 0.
pub fn items_context_floor(items: &[Item]) -> usize {
    items
        .iter()
        .rposition(|i| i.origin() == ItemOrigin::CompactSeed)
        .unwrap_or(0)
}

/// Index where a cut keeping the first `keep_user_turns` typed prompts lands:
/// before the next prompt's turn boundary, so the kept tail ends with its own
/// response. Returns `items.len()` when nothing is cut.
pub fn items_user_turn_cut(items: &[Item], keep_user_turns: usize) -> usize {
    let mut seen = 0;
    for (index, item) in items.iter().enumerate() {
        if item.is_typed_prompt() {
            if seen == keep_user_turns {
                // Cut before the boundary that precedes this prompt.
                return items[..index]
                    .iter()
                    .rposition(|i| matches!(i, Item::Boundary))
                    .unwrap_or(index);
            }
            seen += 1;
        }
    }
    items.len()
}

/// Number of typed user prompts in the item list.
pub fn items_user_turn_count(items: &[Item]) -> usize {
    items.iter().filter(|i| i.is_typed_prompt()).count()
}

/// One-line text replacement for an image part, when it cannot be attached.
pub fn item_image_placeholder(image: &ItemImage) -> String {
    let dims = if image.width > 0 && image.height > 0 {
        format!(" ({}x{})", image.width, image.height)
    } else {
        String::new()
    };
    format!("[image: {}{}]", image.mime, dims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_prompts_exclude_synthetic_origins() {
        assert!(Item::user("hi").is_typed_prompt());
        assert!(!Item::User {
            text: "hi".into(),
            images: vec![],
            origin: ItemOrigin::Continuation
        }
        .is_typed_prompt());
    }

    #[test]
    fn user_turn_cut_keeps_whole_turns() {
        let items = vec![
            Item::Boundary,
            Item::user("one"),
            Item::assistant("a1"),
            Item::Boundary,
            Item::user("two"),
            Item::assistant("a2"),
            Item::Boundary,
            Item::user("three"),
        ];
        assert_eq!(items_user_turn_count(&items), 3);
        assert_eq!(items_user_turn_cut(&items, 2), 6);
        assert_eq!(items_user_turn_cut(&items, 3), items.len());
        assert_eq!(items_user_turn_cut(&items, 10), items.len());
    }

    #[test]
    fn context_floor_is_newest_seed() {
        let items = vec![
            Item::User {
                text: "summary".into(),
                images: vec![],
                origin: ItemOrigin::CompactSeed
            },
            Item::user("hi"),
            Item::User {
                text: "newer summary".into(),
                images: vec![],
                origin: ItemOrigin::CompactSeed
            },
            Item::user("again"),
        ];
        assert_eq!(items_context_floor(&items), 2);
    }

    #[test]
    fn session_json_roundtrip() {
        let item = Item::ToolResult {
            call_id: "call_1".into(),
            output: "done".into(),
            hidden_tail: 4,
            images: vec![],
            origin: ItemOrigin::Skipped,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains(r#""type":"tool_result""#));
        let back: Item = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Item::ToolResult { .. }));
    }
}
