//! Configuration (PLAN.md §15).

use std::collections::HashMap;
use std::path::PathBuf;

use handover_core::enforce::{Decision, EnforcePolicy};
use handover_core::item::Kind;
use handover_core::packet::PacketStrategy;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub listen: Listen,
    #[serde(default)]
    pub store: Store,
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<Upstream>,
    #[serde(default)]
    pub extractor: Extractor,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default, rename = "developer")]
    pub developers: Vec<Developer>,
    /// Harness profile used when the request names none (`X-Handover-Harness`).
    #[serde(default = "default_profile")]
    pub default_profile: String,
    /// Extra profile files: name → path.
    #[serde(default)]
    pub profiles: HashMap<String, PathBuf>,
    /// Drop a session from memory after this long without requests; it is
    /// rebuilt from the store on its next request.
    #[serde(default = "default_idle")]
    pub session_idle_secs: u64,
    /// Poll the next hop's `/v1/models` this often; a changed `root` for a model
    /// id means new weights under the same name (§6). 0 disables polling.
    #[serde(default = "default_poll")]
    pub models_poll_secs: u64,
    /// Largest accepted request body. Long coding sessions resend their whole
    /// history, tool output included, on every turn.
    #[serde(default = "default_body")]
    pub max_body_bytes: usize,
    /// Hex SHA-256 of the operator token for `/admin/traqula` (disabled when unset,
    /// unless no developers are configured).
    #[serde(default)]
    pub operator_token_sha256: Option<String>,
    /// With no `[[developer]]` entries every request is anonymous; listening on
    /// anything but loopback then needs this explicit opt-in (dev stacks only).
    #[serde(default)]
    pub allow_anonymous: bool,
    /// Hex SHA-256 of the admin token used when no developers are configured:
    /// explicit decisions are refused in anonymous mode without it (the agent's
    /// shell can reach a loopback listener too).
    #[serde(default)]
    pub admin_token_sha256: Option<String>,
}

fn default_body() -> usize {
    64 << 20
}

fn default_poll() -> u64 {
    60
}

fn default_idle() -> u64 {
    6 * 3600
}

fn default_profile() -> String {
    "generic".into()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    pub addr: String,
}

impl Default for Listen {
    fn default() -> Self {
        Listen { addr: "127.0.0.1:8443".into() }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Store {
    /// Path of the Positorium store; in-memory when absent.
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub affinity: Affinity,
    #[serde(default)]
    pub models: Vec<String>,
    /// Bearer token for the next hop; the client's Authorization is not forwarded.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Response header that names the served model, if the next hop sets one.
    #[serde(default)]
    pub served_model_header: Option<String>,
    /// Forward the client's `Authorization` to the next hop when `api_key` is
    /// unset (cloud stand-ins in Tier 3). Off by default: the proxy's tokens are
    /// its own credentials, not the router's.
    #[serde(default)]
    pub forward_client_auth: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Affinity {
    /// Send the session key in a header (default `X-Session-ID`).
    Header {
        #[serde(default = "default_affinity_header")]
        name: String,
    },
    /// No header; rely on identical prefix bytes (cache-aware routers).
    Prefix,
    None,
}

fn default_affinity_header() -> String {
    "X-Session-ID".into()
}

impl Default for Affinity {
    fn default() -> Self {
        Affinity::Header { name: default_affinity_header() }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extractor {
    /// `same_route` (Option B) or `dedicated` (Option C).
    #[serde(default = "default_mode")]
    pub mode: String,
    pub url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
    #[serde(default = "default_concurrency")]
    pub max_concurrent: usize,
    #[serde(default = "default_deadline")]
    pub deadline_ms_on_handover: u64,
    #[serde(default = "default_min_meta")]
    pub min_meta_certainty: f64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// vLLM `priority` for background extraction requests (lower runs first
    /// under `--scheduling-policy priority`); omitted when unset.
    #[serde(default)]
    pub priority: Option<i64>,
}

fn default_mode() -> String {
    "same_route".into()
}
fn default_debounce() -> u64 {
    1500
}
fn default_concurrency() -> usize {
    4
}
fn default_deadline() -> u64 {
    4000
}
fn default_min_meta() -> f64 {
    0.7
}
fn default_max_tokens() -> u64 {
    1024
}
fn default_true() -> bool {
    true
}

impl Default for Extractor {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default = "default_enforce_kinds")]
    pub enforce_kinds: Vec<String>,
    /// `hold` (default), `block` or `warn`.
    #[serde(default = "default_on_uncertain")]
    pub on_uncertain_hard: String,
    #[serde(default = "default_strategy")]
    pub packet_strategy: String,
    #[serde(default = "default_packet_chars")]
    pub packet_max_chars: usize,
    /// Record every audit report as an audit posit.
    #[serde(default = "default_true")]
    pub record_audits: bool,
    /// At an epoch change, remove tool definitions that an active hard item
    /// forbids outright (`enforce = {"tools": [...]}` only), for the whole epoch.
    #[serde(default)]
    pub drop_revoked_tools: bool,
}

fn default_enforce_kinds() -> Vec<String> {
    vec!["prohibition".into(), "role_boundary".into()]
}
fn default_on_uncertain() -> String {
    "hold".into()
}
fn default_strategy() -> String {
    "status_aware_core".into()
}
fn default_packet_chars() -> usize {
    8000
}

impl Default for Policy {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}

impl Policy {
    pub fn enforce(&self) -> EnforcePolicy {
        EnforcePolicy {
            enforce_kinds: self.enforce_kinds.iter().filter_map(|k| Kind::parse(k)).collect(),
            on_uncertain_hard: match self.on_uncertain_hard.as_str() {
                "block" => Decision::Block,
                "warn" => Decision::Warn,
                _ => Decision::Hold,
            },
            on_graded: Decision::Warn,
        }
    }

    pub fn strategy(&self) -> PacketStrategy {
        PacketStrategy::parse(&self.packet_strategy).unwrap_or_default()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Developer {
    /// Hex SHA-256 of the bearer token the harness uses for inference.
    pub token_sha256: String,
    pub label: String,
    /// Hex SHA-256 of a separate token for the admin API (explicit decisions,
    /// audits, session views). Never give it to the harness: an agent holding
    /// it could lift its own constraints. Admin calls are refused without it.
    #[serde(default)]
    pub admin_token_sha256: Option<String>,
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Config> {
        let c: Config = toml::from_str(text)?;
        if c.upstreams.is_empty() {
            anyhow::bail!("at least one [[upstream]] is required");
        }
        if PacketStrategy::parse(&c.policy.packet_strategy).is_none() {
            anyhow::bail!("unknown packet_strategy {:?}", c.policy.packet_strategy);
        }
        Ok(c)
    }

    /// A config for tests and local runs: one upstream, in-memory store.
    pub fn for_upstream(base_url: &str) -> Config {
        let mut c: Config = toml::from_str(&format!("[[upstream]]\nname = \"default\"\nbase_url = \"{base_url}\"\n")).expect("valid");
        c.extractor.debounce_ms = 50;
        c
    }

    pub fn upstream(&self) -> &Upstream {
        &self.upstreams[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_configs_parse() {
        let example = Config::from_toml(include_str!("../../../deploy/handover.example.toml")).unwrap();
        assert_eq!(example.extractor.debounce_ms, 1500);
        assert!(example.developers.is_empty());
        let dev = Config::from_toml(include_str!("../../../deploy/dev.toml")).unwrap();
        assert_eq!(dev.upstream().base_url, "http://router:8000");
    }

    #[test]
    fn plan_example_parses() {
        let c = Config::from_toml(
            r#"
[listen]
addr = "0.0.0.0:8443"
[store]
path = "/var/lib/handover/register.store"
[[upstream]]
name = "fleet"
base_url = "http://vllm-router:8000"
affinity = { kind = "header", name = "X-Session-ID" }
models = ["qwen3-coder@2026-11"]
[extractor]
mode = "same_route"
debounce_ms = 1500
max_concurrent = 4
deadline_ms_on_handover = 4000
min_meta_certainty = 0.7
[policy]
enforce_kinds = ["prohibition", "role_boundary"]
on_uncertain_hard = "hold"
packet_strategy = "status_aware_core"
[[developer]]
token_sha256 = "abc"
label = "developer:42"
"#,
        )
        .unwrap();
        assert_eq!(c.upstream().name, "fleet");
        assert_eq!(c.developers[0].label, "developer:42");
        assert!(matches!(c.upstream().affinity, Affinity::Header { .. }));
        assert_eq!(c.extractor.debounce_ms, 1500);
    }
}
