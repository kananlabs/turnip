//! The provider registry: shipped provider definitions overridable
//! key-by-key by `providers.<id>` configuration blocks, plus custom
//! config-only providers, availability checks, and autoselection.

use std::time::Duration;

use serde_json::Value;

use super::chat::ChatDialect;
use super::http::{HttpProvider, MetadataApi, ResolvedEndpoint};
use super::messages::MessagesDialect;
use super::{Api, Provider, ProviderDescriptor};
use crate::config::Config;
use crate::transport::HttpTransport;

/// One shipped provider definition: default field values overridable by a
/// matching `providers.<id>` config block.
#[derive(Debug)]
pub struct DefSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub api: Api,
    pub base_url: Option<&'static str>,
    /// Default for the `{port}` placeholder.
    pub port: u16,
    pub api_key_env: Option<&'static str>,
    /// No key needed; probed for availability.
    pub local: bool,
    pub efforts: &'static [&'static str],
    pub reasoning_roundtrip: Option<&'static str>,
    pub reasoning_required: bool,
    pub send_stream_options: bool,
    pub extra_headers: Option<&'static str>,
    pub extra_body: Option<&'static str>,
    pub max_tokens: i64,
    pub metadata_api: MetadataApi,
    pub unconfigured_reason: Option<&'static str>,
    pub default_model: Option<&'static str>,
}

const fn def(
    id: &'static str,
    display_name: &'static str,
    api: Api,
    base_url: Option<&'static str>,
    api_key_env: Option<&'static str>,
) -> DefSpec {
    DefSpec {
        id,
        display_name,
        api,
        base_url,
        port: 0,
        api_key_env,
        local: false,
        efforts: &[],
        reasoning_roundtrip: None,
        reasoning_required: false,
        send_stream_options: true,
        extra_headers: None,
        extra_body: None,
        max_tokens: 16_384,
        metadata_api: MetadataApi::OpenAi,
        unconfigured_reason: None,
        default_model: None,
    }
}

/// Shipped defs, user-facing ones first in autoselect priority order.
pub static DEFS: &[DefSpec] = &[
    DefSpec {
        efforts: &["low", "medium", "high"],
        ..def("openai", "OpenAI", Api::ChatCompletions, Some("https://api.openai.com/v1"), Some("OPENAI_API_KEY"))
    },
    DefSpec {
        api: Api::Messages,
        efforts: &["minimal", "low", "medium", "high", "xhigh"],
        metadata_api: MetadataApi::Anthropic,
        ..def("anthropic", "Anthropic", Api::Messages, Some("https://api.anthropic.com/v1"), Some("ANTHROPIC_API_KEY"))
    },
    DefSpec {
        extra_headers: Some(
            r#"{"X-Title": "turnip", "HTTP-Referer": "https://github.com/kananlabs/turnip"}"#,
        ),
        ..def("openrouter", "OpenRouter", Api::ChatCompletions, Some("https://openrouter.ai/api/v1"), Some("OPENROUTER_API_KEY"))
    },
    DefSpec {
        extra_headers: Some(r#"{"x-opencode-client": "turnip"}"#),
        ..def("opencode-zen", "OpenCode Zen", Api::ChatCompletions, Some("https://opencode.ai/zen/v1"), Some("OPENCODE_API_KEY"))
    },
    DefSpec {
        extra_headers: Some(r#"{"x-opencode-client": "turnip"}"#),
        ..def("opencode-go", "OpenCode Go", Api::ChatCompletions, Some("https://opencode.ai/zen/go/v1"), Some("OPENCODE_API_KEY"))
    },
    DefSpec {
        reasoning_roundtrip: Some("reasoning_content"),
        reasoning_required: true,
        ..def("deepseek", "DeepSeek", Api::ChatCompletions, Some("https://api.deepseek.com"), Some("DEEPSEEK_API_KEY"))
    },
    DefSpec {
        display_name: "llama.cpp",
        base_url: Some("http://127.0.0.1:{port}/v1"),
        port: 8080,
        local: true,
        reasoning_roundtrip: Some("reasoning_content"),
        send_stream_options: false,
        unconfigured_reason: Some("llama-server is not reachable"),
        ..def("llamacpp", "llama.cpp", Api::ChatCompletions, None, None)
    },
    DefSpec {
        base_url: Some("http://127.0.0.1:{port}/v1"),
        port: 11434,
        local: true,
        reasoning_roundtrip: Some("reasoning"),
        send_stream_options: false,
        unconfigured_reason: Some("ollama is not running"),
        ..def("ollama", "Ollama", Api::ChatCompletions, None, None)
    },
    DefSpec {
        api: Api::ChatCompletions,
        unconfigured_reason: Some("TURNIP_OPENAI_BASE_URL not set"),
        ..def("openai-compatible", "OpenAI-compatible", Api::ChatCompletions, None, Some("OPENAI_API_KEY"))
    },
    DefSpec {
        api: Api::Messages,
        unconfigured_reason: Some("TURNIP_ANTHROPIC_BASE_URL not set"),
        ..def("anthropic-compatible", "Anthropic-compatible", Api::Messages, None, Some("TURNIP_ANTHROPIC_API_KEY"))
    },
];

fn find_shipped(id: &str) -> Option<&'static DefSpec> {
    DEFS.iter().find(|d| d.id == id)
}

fn provider_leaf(config: &Config, id: &str, leaf: &str) -> Option<String> {
    config.str(&format!("providers.{id}.{leaf}"))
}

/// Resolve a descriptor for every user-facing provider: shipped defs plus
/// config-defined ones, in autoselect order.
pub fn descriptors(config: &Config) -> Vec<ProviderDescriptor> {
    let mut list: Vec<ProviderDescriptor> = DEFS
        .iter()
        .map(|spec| descriptor_of(config, spec))
        .collect();
    // Config-only providers, in config order, after the shipped ones.
    for name in config.object_keys("providers") {
        if find_shipped(&name).is_some() {
            continue;
        }
        let api = match provider_leaf(config, &name, "api").as_deref() {
            Some("messages") | Some("anthropic-messages") => Api::Messages,
            _ => Api::ChatCompletions,
        };
        list.push(ProviderDescriptor {
            display_name: provider_leaf(config, &name, "display_name")
                .unwrap_or_else(|| name.clone()),
            api,
            base_url: provider_leaf(config, &name, "base_url"),
            api_key_env: None,
            local: provider_leaf(config, &name, "api_key").is_none()
                && provider_leaf(config, &name, "base_url")
                    .is_some_and(|url| url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost")),
            unconfigured_reason: Some("no base_url configured".into()),
            ..ProviderDescriptor {
                id: name,
                ..Default::default()
            }
        });
    }
    list
}

fn descriptor_of(config: &Config, spec: &DefSpec) -> ProviderDescriptor {
    let base_url = provider_leaf(config, spec.id, "base_url").or_else(|| {
        spec.base_url.map(|url| {
            if url.contains("{port}") {
                let port = provider_leaf(config, spec.id, "port")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(if spec.port != 0 { spec.port } else { 8080 });
                url.replace("{port}", &port.to_string())
            } else {
                url.to_string()
            }
        })
    });
    ProviderDescriptor {
        id: spec.id.to_string(),
        display_name: provider_leaf(config, spec.id, "display_name")
            .unwrap_or_else(|| spec.display_name.to_string()),
        api: match provider_leaf(config, spec.id, "api").as_deref() {
            Some("messages") | Some("anthropic-messages") => Api::Messages,
            Some("chat") | Some("completions") | Some("openai-completions") => Api::ChatCompletions,
            _ => spec.api,
        },
        base_url,
        api_key_env: spec.api_key_env.map(String::from),
        local: spec.local,
        unconfigured_reason: spec.unconfigured_reason.map(String::from),
        internal: false,
    }
}

/// Look up a descriptor by id (shipped names take precedence).
pub fn find(config: &Config, id: &str) -> Option<ProviderDescriptor> {
    if let Some(spec) = find_shipped(id) {
        return Some(descriptor_of(config, spec));
    }
    descriptors(config).into_iter().find(|d| d.id == id)
}

/// Build a usable provider for `descriptor`, or None when configuration is
/// insufficient (no base URL).
pub fn build(config: &Config, descriptor: &ProviderDescriptor, transport: HttpTransport) -> Option<Box<dyn Provider>> {
    let id = &descriptor.id;
    let spec = find_shipped(id);
    let base_url = descriptor.base_url.clone()?;

    // API key: config block, then the def's environment variable.
    let api_key = provider_leaf(config, id, "api_key").or_else(|| {
        let bare = spec
            .and_then(|s| s.api_key_env)
            .map(String::from)
            .unwrap_or_else(|| id.to_ascii_uppercase().replace('-', "_"));
        api_key_from_env(&bare)
    });

    let api = descriptor.api;
    let efforts: Vec<String> = spec
        .map(|s| s.efforts.iter().map(|e| e.to_string()).collect())
        .unwrap_or_default();
    let max_tokens = provider_leaf(config, id, "max_tokens")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| spec.map(|s| s.max_tokens).unwrap_or(16_384));

    let endpoint = ResolvedEndpoint {
        id: id.clone(),
        display_name: descriptor.display_name.clone(),
        api,
        base_url,
        api_key,
        local: descriptor.local,
        default_model: provider_leaf(config, id, "model").or_else(|| {
            spec.and_then(|s| s.default_model.map(String::from))
        }),
        efforts,
        chat: ChatDialect {
            reasoning_roundtrip: spec
                .and_then(|s| s.reasoning_roundtrip)
                .map(String::from)
                .or_else(|| provider_leaf(config, id, "reasoning_roundtrip")),
            send_stream_options: spec
                .map(|s| s.send_stream_options)
                .unwrap_or(true),
            reasoning_required: spec
                .map(|s| s.reasoning_required)
                .unwrap_or(false)
                || provider_leaf(config, id, "reasoning_required")
                    .is_some_and(|v| v == "on"),
        },
        messages: MessagesDialect {
            max_tokens,
            strict_signatures: id == "anthropic",
        },
        extra_headers: provider_leaf(config, id, "extra_headers").or_else(|| {
            spec.and_then(|s| s.extra_headers).map(String::from)
        }),
        extra_body: provider_leaf(config, id, "extra_body")
            .or_else(|| spec.and_then(|s| s.extra_body).map(String::from)),
        metadata_api: match provider_leaf(config, id, "metadata_api").as_deref() {
            Some("anthropic") => MetadataApi::Anthropic,
            Some("openai") => MetadataApi::OpenAi,
            _ => spec.map(|s| s.metadata_api).unwrap_or(match api {
                Api::Messages => MetadataApi::Anthropic,
                Api::ChatCompletions => MetadataApi::OpenAi,
            }),
        },
    };
    Some(Box::new(HttpProvider::new(endpoint, transport)))
}

/// Read an API key from the environment. The `TURNIP_`-prefixed name wins;
/// the bare provider-declared name (e.g. `OPENROUTER_API_KEY`) is accepted
/// as the portable fallback.
fn api_key_from_env(bare: &str) -> Option<String> {
    for name in [format!("TURNIP_{bare}"), bare.to_string()] {
        if let Ok(value) = std::env::var(&name) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// A warning when an API key is being read from the bare provider-declared
/// env var rather than the `TURNIP_`-prefixed one or a config block, so the
/// selection stays explicit. None when nothing is worth reporting.
pub fn bare_api_key_warning(config: &Config, descriptor: &ProviderDescriptor) -> Option<String> {
    let env = descriptor.api_key_env.as_deref()?;
    if provider_leaf(config, &descriptor.id, "api_key").is_some() {
        return None;
    }
    let prefixed = format!("TURNIP_{env}");
    if std::env::var(&prefixed).is_ok_and(|v| !v.is_empty()) {
        return None;
    }
    if std::env::var(env).is_ok_and(|v| !v.is_empty()) {
        return Some(format!("using {env}; set {prefixed} to be explicit"));
    }
    None
}

/// Whether a provider is usable right now. `Err(reason)` explains why not.
/// Local servers are probed with a short request; keyed providers are
/// available whenever their key is set.
pub fn availability(
    config: &Config,
    descriptor: &ProviderDescriptor,
    transport: &HttpTransport,
) -> Result<(), String> {
    let Some(base_url) = &descriptor.base_url else {
        return Err(descriptor
            .unconfigured_reason
            .clone()
            .unwrap_or_else(|| "no base_url configured".into()));
    };
    if descriptor.local {
        let url = format!("{base_url}/models");
        let mut headers = std::collections::BTreeMap::new();
        if let Some(env) = &descriptor.api_key_env {
            if let Some(key) = api_key_from_env(env) {
                headers.insert("Authorization".into(), format!("Bearer {key}"));
            }
        }
        let probe = transport.request("GET", &url, &headers, None, Some(Duration::from_secs(2)));
        if probe.is_err() {
            return Err(format!("{} is not reachable", descriptor.display_name));
        }
        return Ok(());
    }
    match &descriptor.api_key_env {
        Some(env) => {
            let var = format!("TURNIP_{env}");
            if api_key_from_env(env).is_some()
                || provider_leaf(config, &descriptor.id, "api_key").is_some()
            {
                Ok(())
            } else {
                Err(format!("{var} (or {env}) is not set"))
            }
        }
        None => {
            if provider_leaf(config, &descriptor.id, "api_key").is_some() {
                Ok(())
            } else {
                Err(format!("providers.{}.api_key is not set", descriptor.id))
            }
        }
    }
}

/// The highest-priority available provider, or None when nothing is
/// configured. Returns the descriptor so callers can report what won.
pub fn autoselect(
    config: &Config,
    transport: &HttpTransport,
) -> Option<(ProviderDescriptor, Box<dyn Provider>)> {
    for descriptor in descriptors(config) {
        if availability(config, &descriptor, transport).is_ok() {
            if let Some(provider) = build(config, &descriptor, clone_transport(transport)) {
                return Some((descriptor, provider));
            }
        }
    }
    None
}

/// Transports are cheap handles over a shared connection pool; each provider
/// gets its own so per-provider policies stay independent.
pub fn clone_transport(transport: &HttpTransport) -> HttpTransport {
    HttpTransport::new(crate::transport::RetryPolicy {
        max_retries: transport.policy.max_retries,
        retry_base_ms: transport.policy.retry_base_ms,
        idle_timeout_ms: transport.policy.idle_timeout_ms,
    })
}

/// Transport from configuration (`http.*` settings).
pub fn transport_from_config(config: &Config) -> HttpTransport {
    HttpTransport::new(crate::transport::RetryPolicy {
        max_retries: config.int_or("http.max_retries", 4).max(0) as u32,
        retry_base_ms: config.duration_ms_or("http.retry_base", 1000).max(0) as u64,
        idle_timeout_ms: config.duration_ms_or("http.idle_timeout", 600_000).max(0) as u64,
    })
}

/// Map a former provider id to its current one so saved sessions keep
/// resolving; unrecognized names pass through unchanged.
pub fn canonical_id(name: &str) -> &str {
    match name {
        "openai-chat" => "openai",
        _ => name,
    }
}

/// Whether any config `providers.<id>.*` block exists (for pickers).
pub fn has_custom_providers(config: &Config) -> bool {
    config.object_keys("providers").iter().any(|name| !name.contains('.'))
}

#[allow(unused)]
fn json_object(string: Option<&str>) -> Option<Value> {
    string.and_then(|s| serde_json::from_str(s).ok())
}
