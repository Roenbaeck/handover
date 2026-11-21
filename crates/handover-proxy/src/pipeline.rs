//! The request lifecycle (PLAN.md §5): resolve the session, classify, decide the
//! epoch, rewrite deterministically, forward and tee, record, schedule extraction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use handover_core::clock::{self, Clock};
use handover_core::conversation::{Conversation, sha256_hex};
use handover_core::enforce::{Action, Decision, Enforcer, Verdict};
use handover_core::item::{Kind, RegisterItem, Status, Strength};
use handover_core::packet::{self, PacketInput};
use handover_core::profile::Profile;
use handover_core::status::{StatusPolicy, derive};
use handover_core::verify::{DeltaOp, Source, Target, VerifiedDelta};
use handover_core::{Incoming, LaneView, Route};
use handover_protocol::chat;
use handover_protocol::sse::SseParser;
use handover_protocol::stream::{CallDecision, StreamFilter, ToolCall, Transcript, filter_completion};
use handover_register::{ActionRecord, EpochWrite, Inherited, Register, TimedDelta};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use crate::config::{Affinity, Config};
use crate::extract;
use crate::metrics::{Metrics, inc};
use crate::session::{BranchState, EpochState, Lane, LastTurn, MAX_LANES, Session};

pub type SessionRef = Arc<tokio::sync::Mutex<Session>>;

pub struct AppState {
    pub config: Config,
    pub register: Arc<Register>,
    pub http: reqwest::Client,
    pub sessions: Mutex<HashMap<String, SessionRef>>,
    pub clock: Clock,
    pub metrics: Metrics,
    pub extract_permits: Arc<Semaphore>,
    pub profiles: HashMap<String, Profile>,
    /// Inference token hash → developer label.
    pub developers: HashMap<String, String>,
    /// Admin token hash → developer label (separate credentials, §7.6).
    pub admin_tokens: HashMap<String, String>,
    /// Background extraction pauses until then after a 429/503 from the next hop (§7.2).
    pub overloaded_until: Mutex<Option<std::time::Instant>>,
    /// Last `root` seen per model id in the next hop's `/v1/models`.
    pub model_roots: Mutex<HashMap<String, String>>,
}

pub type Shared = Arc<AppState>;

/// A request after authentication and session resolution.
pub struct Ingress {
    pub developer: String,
    /// Internal key: namespaced per developer, so keys never collide across developers.
    pub session_key: String,
    /// The key as the client sent it (header, metadata or weak digest).
    pub client_key: String,
    pub weak: bool,
    pub profile: Profile,
    pub body: Value,
    /// The client's exact bytes, forwarded unchanged when nothing is rewritten (D4).
    pub raw: Option<Bytes>,
    pub client_auth: Option<HeaderValue>,
    /// Forward `client_auth` to the next hop (`forward_client_auth` without `api_key`).
    pub forward_auth: bool,
}

/// The upstream answer, after enforcement.
pub enum Upstreamed {
    Stream { headers: HeaderMap, rx: mpsc::Receiver<Result<Bytes, std::io::Error>> },
    Json { status: StatusCode, headers: HeaderMap, body: Value },
    Raw { status: StatusCode, headers: HeaderMap, body: Bytes },
}

pub fn error(status: StatusCode, msg: &str) -> Upstreamed {
    Upstreamed::Json { status, headers: HeaderMap::new(), body: json!({"error": {"message": msg, "type": "handover_error"}}) }
}

impl AppState {
    pub fn profile(&self, name: &str) -> Profile {
        self.profiles
            .get(name)
            .cloned()
            .or_else(|| self.profiles.get(&self.config.default_profile).cloned())
            .unwrap_or_else(|| Profile::builtin("generic").expect("generic profile"))
    }

    /// Developer label for an inference bearer token; anonymous when no developers are configured.
    pub fn developer_for(&self, token: Option<&str>) -> Option<String> {
        if self.developers.is_empty() {
            return Some("developer:anonymous".into());
        }
        let t = token?;
        self.developers.get(&sha256_hex(t)).cloned()
    }

    /// Developer label for an admin token. Inference tokens are not accepted:
    /// the harness (and so the agent) holds those. In anonymous mode, views
    /// (`write = false`) are open, but writes need the configured admin token:
    /// the agent's shell can reach a loopback listener as easily as anyone.
    pub fn admin_for(&self, token: Option<&str>, write: bool) -> Option<String> {
        if self.developers.is_empty() {
            if !write {
                return Some("developer:anonymous".into());
            }
            let (Some(h), Some(t)) = (&self.config.admin_token_sha256, token) else { return None };
            return h.eq_ignore_ascii_case(&sha256_hex(t)).then(|| "developer:anonymous".into());
        }
        self.admin_tokens.get(&sha256_hex(token?)).cloned()
    }

    /// May this token run read-only Traqula over every session?
    pub fn operator_ok(&self, token: Option<&str>) -> bool {
        if self.developers.is_empty() {
            return true;
        }
        match (&self.config.operator_token_sha256, token) {
            (Some(h), Some(t)) => h.eq_ignore_ascii_case(&sha256_hex(t)),
            _ => false,
        }
    }

    /// The internal session key for a developer's client key.
    pub fn namespaced(&self, developer: &str, client_key: &str) -> String {
        if self.developers.is_empty() { client_key.to_string() } else { format!("{}.{client_key}", &sha256_hex(developer)[..8]) }
    }

    pub fn status_policy(&self) -> StatusPolicy {
        StatusPolicy { min_meta_certainty: self.config.extractor.min_meta_certainty }
    }

    pub fn authorities(&self, s: &Session) -> Vec<String> {
        vec![s.developer.clone(), format!("harness:{}", s.profile)]
    }

    pub fn positor(&self, s: &Session, source: Source, model: &str) -> String {
        positor_name(&s.developer, &s.profile, source, model)
    }

    /// Items of a branch with derived status, and the compiled enforcer.
    pub async fn refresh(&self, branch_key: &str) -> anyhow::Result<(Vec<RegisterItem>, Arc<Enforcer>)> {
        let records = self.register.items(branch_key, None).await?;
        let items = derive(&records, &self.status_policy());
        let enforcer = Arc::new(Enforcer::from_register(&items, &self.config.policy.enforce()));
        Ok((items, enforcer))
    }

    /// Get or create the session; after a restart, rebuild it from the store.
    pub async fn session(&self, ing: &Ingress) -> anyhow::Result<SessionRef> {
        if let Some(s) = self.sessions.lock().unwrap().get(&ing.session_key).cloned() {
            return Ok(s);
        }
        let mut s = Session::new(ing.session_key.clone(), ing.developer.clone(), ing.profile.name.clone(), ing.weak);
        s.client_key = ing.client_key.clone();
        let now = self.clock.now();
        match self.register.session_snapshot(&ing.session_key).await? {
            Some(snap) => {
                if snap.owner.as_deref().is_some_and(|o| o != ing.developer) {
                    anyhow::bail!("session belongs to another developer");
                }
                s.restore(&snap);
                let key = s.branch().key.clone();
                let (items, enforcer) = self.refresh(&key).await?;
                let b = s.branch_mut();
                b.items = items;
                b.enforcer = enforcer;
                tracing::info!(session = %ing.session_key, branches = snap.branches.len(), "restored session from store");
            }
            None => {
                // The session, its root branch and its first epoch (no packet, D4) in one commit.
                let auth = self.authorities(&s);
                let model = chat::model(&ing.body).unwrap_or("").to_string();
                let first = EpochWrite {
                    key: s.next_epoch_key(),
                    trigger: "session_start".into(),
                    to_model: model.clone(),
                    packet: None,
                    citations: vec![],
                    turn: chat::messages(&ing.body).len().saturating_sub(1),
                    through: None,
                    drop_tools: vec![],
                };
                self.register.ensure_session(&ing.session_key, &ing.developer, &ing.profile.name, &auth, Some(&first), now).await?;
                inc(&self.metrics.epochs_total);
                let b = s.branch_mut();
                b.epochs = 1;
                b.epoch =
                    Some(EpochState { key: first.key, trigger: first.trigger, model, packet: None, complete: true, drop_tools: vec![] });
            }
        }
        let arc = Arc::new(tokio::sync::Mutex::new(s));
        Ok(self.sessions.lock().unwrap().entry(ing.session_key.clone()).or_insert(arc).clone())
    }
}

/// The positor a verified change is attributed to.
pub fn positor_name(developer: &str, profile: &str, source: Source, model: &str) -> String {
    match source {
        Source::Developer => developer.to_string(),
        Source::Harness => format!("harness:{profile}"),
        Source::Injected => format!("harness-injected:{profile}"),
        Source::Model => format!("model:{model}"),
        Source::Tool => format!("tool:{profile}"),
        Source::Extractor => format!("extractor:{model}"),
    }
}

/// Session key: explicit header, harness-native metadata, or a weak digest (§13).
pub fn session_key(headers: &HeaderMap, profile: &Profile, native: Option<String>, developer: &str, conv: &Conversation) -> (String, bool) {
    let header = profile.session_header.as_deref().unwrap_or("X-Session-ID");
    for h in [header, "X-Session-ID"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()).filter(|v| !v.trim().is_empty()) {
            return (v.trim().to_string(), false);
        }
    }
    if let Some(n) = native.filter(|n| !n.is_empty()) {
        return (n, false);
    }
    let weak = sha256_hex(&format!("{developer}\u{0}{}\u{0}{}", conv.system_text(), conv.first_human_text()));
    (format!("weak-{}", &weak[..16]), true)
}

struct Prepared {
    upstream: Value,
    rewritten: bool,
    branch_key: String,
    enforcer: Arc<Enforcer>,
    offset: usize,
    model: String,
    epoch_key: Option<String>,
    light: &'static str,
    seq: u64,
    /// The delegated lane the request belongs to (`branch_key` is its parent).
    lane: Option<String>,
    /// A side request: answered and gated, never accepted into its lane.
    side: bool,
}

/// Handle one Chat Completions request end to end.
pub async fn run(state: Shared, ing: Ingress) -> Upstreamed {
    inc(&state.metrics.requests_total);
    let mut conv = Conversation::from_chat(chat::messages(&ing.body), &ing.profile);
    if conv.is_empty() {
        return error(StatusCode::BAD_REQUEST, "messages must be a non-empty array");
    }
    let session = match state.session(&ing).await {
        Ok(s) => s,
        Err(e) if e.to_string().contains("another developer") => {
            return error(StatusCode::FORBIDDEN, "session belongs to another developer");
        }
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("register: {e}")),
    };
    let prep = {
        let mut s = session.lock().await;
        if s.developer != ing.developer {
            return error(StatusCode::FORBIDDEN, "session belongs to another developer");
        }
        s.last_used = std::time::Instant::now();
        match prepare(&state, &mut s, &ing, &mut conv).await {
            Ok(p) => p,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("handover: {e}")),
        }
    };
    forward(state, session, ing, conv, prep).await
}

/// Where a routed request goes.
enum Dest {
    /// A main branch, with the route that led there.
    Main { bi: usize, route: Route },
    /// An existing delegated lane.
    Lane(usize),
    /// A new delegated lane governed by this main branch.
    NewLane { parent: usize },
}

async fn prepare(state: &Shared, s: &mut Session, ing: &Ingress, conv: &mut Conversation) -> anyhow::Result<Prepared> {
    let now = state.clock.now();
    let preamble = conv.preamble_text();
    let identity = ing.profile.system_identity(&preamble);
    let route = {
        let chain = conv.chain();
        let summary = conv.first_body().is_some_and(|m| m.spans.iter().any(|sp| ing.profile.is_compaction_summary(&sp.text)));
        let views: Vec<LaneView> = s.branches.iter().map(BranchState::view).chain(s.lanes.iter().map(Lane::view)).collect();
        let incoming = Incoming {
            chain: &chain,
            body: &conv.body_chain,
            preamble: conv.preamble,
            identity: &identity,
            delegated: ing.profile.is_delegated(&preamble),
            compaction_summary: summary,
        };
        handover_core::route(&views, s.current, &incoming)
    };
    let nb = s.branches.len();
    let target = match route {
        Route::Continue { lane } | Route::Reprompt { lane } if lane >= nb => Dest::Lane(lane - nb),
        // A delegated conversation edited or retried differently: a sibling lane.
        Route::Branch { lane, .. } if lane >= nb => {
            let parent = &s.lanes[lane - nb].parent;
            Dest::NewLane { parent: s.branches.iter().position(|b| &b.key == parent).unwrap_or(s.current) }
        }
        Route::Delegate => Dest::NewLane { parent: s.current },
        Route::New => Dest::Main { bi: s.current, route },
        Route::Continue { lane } | Route::Reprompt { lane } | Route::Branch { lane, .. } | Route::Compaction { lane } => {
            Dest::Main { bi: lane, route }
        }
    };
    // The harness asking for a summary of the history it is about to compact:
    // answered on the lane's prefix, never accepted into the lane.
    let side = matches!(route, Route::Continue { .. }) && conv.last_user_text().is_some_and(|t| ing.profile.is_side_request(&t));
    match target {
        _ if side => Ok(prepare_side(state, s, target, ing)),
        Dest::Main { bi, route } => prepare_main(state, s, bi, route, &identity, ing, conv, now).await,
        Dest::Lane(li) => prepare_lane(state, s, li, &identity, ing, conv, now).await,
        Dest::NewLane { parent } => {
            let li = open_lane(state, s, parent, ing);
            prepare_lane(state, s, li, &identity, ing, conv, now).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_main(
    state: &Shared,
    s: &mut Session,
    bi: usize,
    route: Route,
    identity: &str,
    ing: &Ingress,
    conv: &Conversation,
    now: DateTime<Utc>,
) -> anyhow::Result<Prepared> {
    let model = chat::model(&ing.body).unwrap_or("").to_string();
    s.current = bi;
    if matches!(route, Route::New | Route::Continue { .. } | Route::Reprompt { .. })
        && let Some(h) = s.branch_mut().extraction.take()
    {
        h.abort(); // this request carries a newer turn; extraction is rescheduled after it
        s.branch_mut().extracting = false;
    }
    // A fork leaves the old branch's extraction running: it covers the old
    // branch, and after a compaction it is the only record of what was dropped.
    let mut trigger: Option<&str> = match route {
        Route::Compaction { .. } => {
            // The history was replaced: indexes restart and the hot prefix is gone.
            // A fresh branch inherits every item as of now, so the old chain's
            // anchors never mix with the new one's (also after a restart).
            fork(state, s, 0, Some(now), now).await?;
            Some("compaction")
        }
        Route::Branch { common, .. } => {
            fork(state, s, common, None, now).await?;
            Some("branch")
        }
        Route::Reprompt { .. } => {
            tracing::info!(session = %s.key, branch = %s.branch().key, "system prompt changed, conversation continues");
            None
        }
        _ => None,
    };
    {
        let b = s.branch();
        if b.epoch.is_none() && trigger.is_none() {
            trigger = Some(if b.parent.is_some() { "branch" } else { "session_start" });
        } else if trigger.is_none() && b.unplanned {
            trigger = Some("unplanned_substitution");
        } else if trigger.is_none() && b.weights_changed {
            trigger = Some("weights_swapped");
        } else if trigger.is_none() && b.epoch.as_ref().is_some_and(|e| e.model != model) {
            trigger = Some("model_change");
        }
    }
    if trigger == Some("session_start") && s.branch().epoch.is_none() && !s.branch().items.is_empty() {
        // Restored or inherited items exist: treat the first epoch as a handover.
        trigger = Some("resume");
    }

    deterministic_events(state, s, ing, conv, now).await?;

    if let Some(t) = trigger {
        open_epoch(state, s, t, &model, ing, conv, now).await?;
    }

    let b = s.branch_mut();
    b.accept_chain(conv, identity, now);
    b.hold_pending = false;
    b.request_seq += 1;
    let seq = b.request_seq;
    let epoch = b.epoch.clone();
    let mut upstream = ing.body.clone();
    let mut offset = 0;
    let mut rewritten = false;
    if let Some(p) = epoch.as_ref().and_then(|e| e.packet.as_deref()) {
        offset = chat::append_system(&mut upstream, p);
        rewritten = true;
        inc(&state.metrics.packets_injected_total);
    }
    if let Some(drop) = epoch.as_ref().map(|e| &e.drop_tools).filter(|d| !d.is_empty()) {
        // Re-applied identically on every request of the epoch (D5).
        if chat::remove_tools(&mut upstream, drop) > 0 {
            rewritten = true;
        }
    }
    let pinned = epoch.as_ref().map(|e| e.model.clone()).unwrap_or_else(|| model.clone());
    if pinned != model {
        chat::set_model(&mut upstream, &pinned);
        rewritten = true;
    }
    if !rewritten {
        inc(&state.metrics.passthrough_total);
    }
    let branch_key = b.key.clone();
    let enforcer = b.enforcer.clone();
    Ok(Prepared {
        upstream,
        rewritten,
        branch_key,
        enforcer,
        offset,
        model: pinned,
        epoch_key: epoch.map(|e| e.key),
        light: s.light().as_str(),
        seq,
        lane: None,
        side: false,
    })
}

/// A side request (the harness's compaction summary): the lane's packet and
/// dropped tools are applied exactly as on its other requests, so the worker
/// reuses the hot prefix; the tool calls are gated by the lane's register. The
/// request is not accepted into the lane, opens no epoch, aborts no
/// extraction and pins no model.
fn prepare_side(state: &Shared, s: &Session, target: Dest, ing: &Ingress) -> Prepared {
    inc(&state.metrics.side_requests_total);
    let (branch, packet, drop, lane) = match target {
        Dest::Lane(li) => {
            let l = &s.lanes[li];
            let b = s.branches.iter().find(|b| b.key == l.parent).unwrap_or(s.branch());
            (b, l.packet.clone(), Vec::new(), Some(l.key.clone()))
        }
        Dest::Main { bi, .. } | Dest::NewLane { parent: bi } => {
            let b = &s.branches[bi];
            let e = b.epoch.as_ref();
            (b, e.and_then(|e| e.packet.clone()), e.map(|e| e.drop_tools.clone()).unwrap_or_default(), None)
        }
    };
    let mut upstream = ing.body.clone();
    let mut offset = 0;
    let mut rewritten = false;
    if let Some(p) = packet.as_deref() {
        offset = chat::append_system(&mut upstream, p);
        rewritten = true;
        inc(&state.metrics.packets_injected_total);
    }
    if !drop.is_empty() && chat::remove_tools(&mut upstream, &drop) > 0 {
        rewritten = true;
    }
    if !rewritten {
        inc(&state.metrics.passthrough_total);
    }
    tracing::info!(session = %s.key, branch = %branch.key, lane = ?lane, "side request");
    Prepared {
        upstream,
        rewritten,
        branch_key: branch.key.clone(),
        enforcer: branch.enforcer.clone(),
        offset,
        model: chat::model(&ing.body).unwrap_or("").to_string(),
        epoch_key: branch.epoch.as_ref().map(|e| e.key.clone()),
        light: s.light().as_str(),
        seq: 0,
        lane,
        side: true,
    }
}

/// Open a delegated lane under a main branch. Its packet is rendered once from
/// the parent's register, and only if the lane can act (it offers tools).
fn open_lane(state: &Shared, s: &mut Session, parent: usize, ing: &Ingress) -> usize {
    if s.lanes.len() >= MAX_LANES
        && let Some(oldest) = s.lanes.iter().enumerate().min_by_key(|(_, l)| l.last_used).map(|(i, _)| i)
    {
        s.lanes.remove(oldest);
    }
    let key = s.next_lane_key();
    let p = &s.branches[parent];
    let packet = if chat::tool_names(&ing.body).is_empty() {
        None
    } else {
        packet::render(&PacketInput {
            epoch_key: &key,
            from_model: None,
            complete_through: p.watermark,
            incomplete: None,
            items: &p.items,
            strategy: state.config.policy.strategy(),
            max_chars: state.config.policy.packet_max_chars,
        })
        .map(|p| p.text)
    };
    inc(&state.metrics.lanes_total);
    tracing::info!(session = %s.key, lane = %key, parent = %p.key, packet = packet.is_some(), "delegated lane");
    let lane = Lane::new(key, p.key.clone(), packet);
    s.lanes.push(lane);
    s.lanes.len() - 1
}

/// A request on a delegated lane: the parent's register governs it live; only
/// developer denials are recorded (on the parent), nothing is extracted.
async fn prepare_lane(
    state: &Shared,
    s: &mut Session,
    li: usize,
    identity: &str,
    ing: &Ingress,
    conv: &mut Conversation,
    now: DateTime<Utc>,
) -> anyhow::Result<Prepared> {
    // The user role of a delegated conversation carries the parent model's words.
    conv.delegate();
    lane_events(state, s, li, ing, now).await?;
    let model = chat::model(&ing.body).unwrap_or("").to_string();
    let l = &mut s.lanes[li];
    l.accept_chain(conv, identity, now);
    l.tools = chat::tool_names(&ing.body);
    l.model.clone_from(&model);
    l.requests += 1;
    l.request_seq += 1;
    l.hold_pending = false;
    l.last_used = std::time::Instant::now();
    let seq = l.request_seq;
    let lane_key = l.key.clone();
    let parent_key = l.parent.clone();
    let mut upstream = ing.body.clone();
    let mut offset = 0;
    let mut rewritten = false;
    if let Some(p) = l.packet.as_deref() {
        offset = chat::append_system(&mut upstream, p);
        rewritten = true;
        inc(&state.metrics.packets_injected_total);
    }
    if !rewritten {
        inc(&state.metrics.passthrough_total);
    }
    let parent = s.branches.iter().find(|b| b.key == parent_key).ok_or_else(|| anyhow::anyhow!("lane parent {parent_key} is gone"))?;
    Ok(Prepared {
        upstream,
        rewritten,
        branch_key: parent_key.clone(),
        enforcer: parent.enforcer.clone(),
        offset,
        model,
        epoch_key: parent.epoch.as_ref().map(|e| e.key.clone()),
        light: s.light().as_str(),
        seq,
        lane: Some(lane_key),
        side: false,
    })
}

/// Rewind / edit / fork: a child branch inherits the parent's items as of just
/// before the first divergent message (§6, paper §7.2).
/// `cut` overrides the inheritance cut (compaction inherits everything known now).
async fn fork(state: &Shared, s: &mut Session, common: usize, cut: Option<DateTime<Utc>>, now: DateTime<Utc>) -> anyhow::Result<()> {
    let parent = s.branch();
    let parent_key = parent.key.clone();
    // Just before the first message at or after the divergence that we know the
    // arrival of (after a restart only turn anchors carry times).
    let cut =
        cut.unwrap_or_else(|| (common..parent.first_seen.len()).find_map(|i| parent.seen_at(i)).map(clock::just_before).unwrap_or(now));
    let parent_watermark = parent.watermark;
    let parent_seen: Vec<Option<DateTime<Utc>>> = parent.first_seen.iter().take(common).copied().collect();
    let records = state.register.items(&parent_key, Some(cut)).await?;
    let inherited: Vec<Inherited> = records
        .iter()
        .map(|r| {
            let mut latest: HashMap<&str, (&str, f64, Option<f64>)> = HashMap::new();
            for a in &r.assertions {
                match latest.get(a.source.as_str()) {
                    Some((t, _, _)) if *t > a.time.as_str() => {}
                    _ => {
                        latest.insert(a.source.as_str(), (a.time.as_str(), a.certainty, a.meta_certainty));
                    }
                }
            }
            let mut positions: Vec<(String, f64, Option<f64>)> =
                latest.into_iter().map(|(src, (_, c, m))| (src.to_string(), c, m)).collect();
            positions.sort_by(|a, b| a.0.cmp(&b.0));
            Inherited { item_key: r.key.clone(), positions }
        })
        .collect();
    let child = s.next_branch_key();
    let auth = state.authorities(s);
    let watermark = match (parent_watermark, common) {
        (_, 0) => None,
        (Some(w), c) => Some(w.min(c - 1)),
        (None, _) => None,
    };
    state.register.create_branch(&s.key, &parent_key, &child, common, &auth, &inherited, watermark, now).await?;
    let mut b = BranchState::new(child.clone(), Some(parent_key), common);
    b.watermark = watermark;
    b.first_seen = parent_seen;
    let (items, enforcer) = state.refresh(&child).await?;
    b.items = items;
    b.enforcer = enforcer;
    s.branches.push(b);
    s.current = s.branches.len() - 1;
    tracing::info!(session = %s.key, branch = %child, common, "fork");
    Ok(())
}

/// Deterministic register events from structured traffic (§7.1), no LLM:
/// - a tool the harness removed becomes a hard role boundary (lifted if it returns);
/// - a tool result reporting a developer denial forbids retrying that exact call;
/// - plan/todo tool calls become goal candidates from an advisory source.
async fn deterministic_events(
    state: &Shared,
    s: &mut Session,
    ing: &Ingress,
    conv: &Conversation,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let new_from = s.branch().chain.len().min(conv.len());
    let turn = conv.len().saturating_sub(1);
    let model = chat::model(&ing.body).unwrap_or("").to_string();
    let harness = state.positor(s, Source::Harness, "");
    let items = s.branch().items.clone();
    let live =
        |statement: &str| items.iter().find(|i| i.statement == statement && !matches!(i.status, Status::Revoked | Status::Superseded));
    let mut deltas: Vec<TimedDelta> = Vec::new();
    let mut planned: Vec<String> = Vec::new();
    let event = |op: DeltaOp,
                 target: Target,
                 kind: Kind,
                 strength: Strength,
                 statement: Option<String>,
                 enforce: Option<Value>,
                 source: Source,
                 certainty: f64,
                 cite: Value| {
        VerifiedDelta {
            op,
            target,
            replaces: None,
            kind,
            strength,
            statement,
            enforce,
            source,
            certainty,
            meta_certainty: 1.0,
            citation: cite,
            requires: None,
            notes: vec![],
        }
    };

    // Tools added to or removed from `tools[]`.
    let tools = chat::tool_names(&ing.body);
    let previous = std::mem::replace(&mut s.branch_mut().tools, tools.clone());
    if !previous.is_empty() && previous != tools {
        for removed in previous.iter().filter(|t| !tools.contains(t)) {
            let statement = format!("The harness removed the tool `{removed}`; do not call it.");
            if live(&statement).is_none() {
                let cite = json!({"turn": turn, "quote": "", "op": "add", "verified": true, "event": "tool_removed", "tool": removed});
                let d = event(
                    DeltaOp::Add,
                    Target::New,
                    Kind::RoleBoundary,
                    Strength::Hard,
                    Some(statement),
                    Some(json!({"tools": [removed]})),
                    Source::Harness,
                    1.0,
                    cite,
                );
                deltas.push(TimedDelta { delta: d, positor: harness.clone(), at: now });
            }
        }
        for added in tools.iter().filter(|t| !previous.contains(t)) {
            if let Some(item) = live(&format!("The harness removed the tool `{added}`; do not call it.")) {
                let cite = json!({"turn": turn, "quote": "", "op": "revoke", "verified": true, "event": "tool_added", "tool": added});
                let d = event(
                    DeltaOp::Revoke,
                    Target::Existing(item.key().to_string()),
                    Kind::RoleBoundary,
                    Strength::Hard,
                    None,
                    None,
                    Source::Harness,
                    0.0,
                    cite,
                );
                deltas.push(TimedDelta { delta: d, positor: harness.clone(), at: now });
            }
        }
    }

    // New messages: denials in tool results, plans in the model's tool calls.
    for d in denials(ing, new_from) {
        if live(&d.statement).is_none() && deltas.iter().all(|x| x.delta.statement.as_deref() != Some(&d.statement)) {
            let cite = json!({"turn": d.index, "quote": d.quote, "op": "add", "verified": true, "event": "denial"});
            let at = s.branch().seen_at(d.index).unwrap_or(now);
            let ev = event(
                DeltaOp::Add,
                Target::New,
                Kind::Prohibition,
                Strength::Hard,
                Some(d.statement),
                Some(d.enforce),
                Source::Developer,
                1.0,
                cite,
            );
            deltas.push(TimedDelta { delta: ev, positor: s.developer.clone(), at });
        }
    }
    let msgs = chat::messages(&ing.body);
    for (i, m) in msgs.iter().enumerate().skip(new_from) {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        for c in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
            if !ing.profile.is_plan_tool(name) {
                continue;
            }
            let args: Value =
                serde_json::from_str(c.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}")).unwrap_or(Value::Null);
            let entries = args.get("todos").or_else(|| args.get("plan")).and_then(Value::as_array).cloned().unwrap_or_default();
            for e in entries {
                let text = e.get("content").or_else(|| e.get("step")).and_then(Value::as_str).unwrap_or("").trim().to_string();
                let done = e.get("status").and_then(Value::as_str) == Some("completed");
                if text.is_empty() {
                    continue;
                }
                let positor = state.positor(s, Source::Model, &model);
                let cite = json!({"turn": i, "quote": text, "op": if done { "revoke" } else { "add" }, "verified": true, "event": "plan"});
                match (live(&text), done) {
                    (None, false) if !planned.contains(&text) => {
                        planned.push(text.clone());
                        let d = event(DeltaOp::Add, Target::New, Kind::Goal, Strength::Graded, Some(text), None, Source::Model, 0.6, cite);
                        deltas.push(TimedDelta { delta: d, positor, at: s.branch().seen_at(i).unwrap_or(now) });
                    }
                    (Some(item), true) if item.kind() == Kind::Goal => {
                        let d = event(
                            DeltaOp::Revoke,
                            Target::Existing(item.key().to_string()),
                            Kind::Goal,
                            Strength::Graded,
                            None,
                            None,
                            Source::Model,
                            0.0,
                            cite,
                        );
                        deltas.push(TimedDelta { delta: d, positor, at: s.branch().seen_at(i).unwrap_or(now) });
                    }
                    _ => {}
                }
            }
        }
    }
    if deltas.is_empty() {
        return Ok(());
    }
    let key = s.branch().key.clone();
    state.register.apply(&s.key, &key, &deltas, None, None, None, now).await?;
    let (items, enforcer) = state.refresh(&key).await?;
    let b = s.branch_mut();
    b.items = items;
    b.enforcer = enforcer;
    Ok(())
}

/// A developer denial reported in the tool result at message `index`.
struct Denial {
    index: usize,
    statement: String,
    enforce: Value,
    quote: String,
}

/// Denials in tool results from message `from` on: each forbids retrying that
/// exact call without asking.
fn denials(ing: &Ingress, from: usize) -> Vec<Denial> {
    let msgs = chat::messages(&ing.body);
    let mut out = Vec::new();
    for (i, m) in msgs.iter().enumerate().skip(from) {
        if m.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let text = handover_core::conversation::content_text(m.get("content"));
        if !ing.profile.is_denial(&text) {
            continue;
        }
        let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
        let Some(call) = msgs[..i]
            .iter()
            .rev()
            .filter_map(|a| a.get("tool_calls").and_then(Value::as_array))
            .flatten()
            .find(|c| c.get("id").and_then(Value::as_str) == Some(id))
        else {
            continue;
        };
        let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
        let args = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
        let action = ing.profile.action(name, args);
        let enforce = if let Some(cmd) = &action.command {
            json!({"ops": ["exec"], "commands": [format!("^{}$", regex::escape(cmd.trim()))]})
        } else if !action.paths.is_empty() {
            json!({"ops": [action.op], "paths": action.paths})
        } else {
            json!({"tools": [name]})
        };
        let statement = format!("The developer denied `{}`; do not retry it without asking.", action.summary());
        if out.iter().any(|d: &Denial| d.statement == statement) {
            continue;
        }
        out.push(Denial { index: i, statement, enforce, quote: text.chars().take(200).collect() });
    }
    out
}

/// Deterministic events of a delegated lane: only developer denials, written
/// to the parent branch (they speak for the whole session). Tool-set changes
/// and plans of a subagent say nothing about the session.
async fn lane_events(state: &Shared, s: &mut Session, li: usize, ing: &Ingress, now: DateTime<Utc>) -> anyhow::Result<()> {
    let (lane_key, parent_key) = (s.lanes[li].key.clone(), s.lanes[li].parent.clone());
    let found = denials(ing, s.lanes[li].chain.len());
    if found.is_empty() {
        return Ok(());
    }
    let Some(parent) = s.branches.iter().find(|b| b.key == parent_key) else { return Ok(()) };
    let mut deltas = Vec::new();
    for d in found {
        let live = parent.items.iter().any(|i| i.statement == d.statement && !matches!(i.status, Status::Revoked | Status::Superseded));
        if live {
            continue;
        }
        let cite = json!({"turn": d.index, "lane": lane_key, "quote": d.quote, "op": "add", "verified": true, "event": "denial"});
        let delta = VerifiedDelta {
            op: DeltaOp::Add,
            target: Target::New,
            replaces: None,
            kind: Kind::Prohibition,
            strength: Strength::Hard,
            statement: Some(d.statement),
            enforce: Some(d.enforce),
            source: Source::Developer,
            certainty: 1.0,
            meta_certainty: 1.0,
            citation: cite,
            requires: None,
            notes: vec![],
        };
        let at = s.lanes[li].seen_at(d.index).unwrap_or(now);
        deltas.push(TimedDelta { delta, positor: s.developer.clone(), at });
    }
    if deltas.is_empty() {
        return Ok(());
    }
    state.register.apply(&s.key, &parent_key, &deltas, None, None, None, now).await?;
    let (items, enforcer) = state.refresh(&parent_key).await?;
    if let Some(b) = s.branch_by_key(&parent_key) {
        b.items = items;
        b.enforcer = enforcer;
    }
    Ok(())
}

/// Open a new epoch (§5 step 3): Option A if the watermark lags, render the
/// packet from the register at this moment, persist its exact bytes.
async fn open_epoch(
    state: &Shared,
    s: &mut Session,
    trigger: &str,
    model: &str,
    ing: &Ingress,
    conv: &Conversation,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let turn = conv.len() - 1;
    let mut incomplete = None;
    let is_handover = trigger != "session_start";
    let overloaded = state.overloaded_until.lock().unwrap().is_some_and(|t| t > std::time::Instant::now());
    if is_handover && s.branch().lags(turn) && state.config.extractor.enabled && !overloaded {
        inc(&state.metrics.option_a_total);
        let deadline = Duration::from_millis(state.config.extractor.deadline_ms_on_handover);
        let input = match s.branch().last_turn.clone() {
            // Planned switch with a hot cache: the outgoing model on its own prefix.
            Some(lt) if lt.conv.len() >= turn && trigger != "compaction" => lt,
            // Otherwise the incoming model on the request's own history.
            _ => cold_input(ing, turn, model),
        };
        match extract::run_extraction(state, s, input, deadline).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                inc(&state.metrics.option_a_timeouts_total);
                tracing::warn!(session = %s.key, "option A timed out");
            }
            Err(e) => tracing::warn!(session = %s.key, "option A failed: {e}"),
        }
        if s.branch().lags(turn) {
            incomplete = Some((s.branch().next_new_from(), turn.saturating_sub(1)));
        }
    }
    let key = s.branch().key.clone();
    let (items, enforcer) = state.refresh(&key).await?;
    let epoch_key = s.next_epoch_key();
    let from_model = s.branch().epoch.as_ref().map(|e| e.model.clone()).or_else(|| s.branch().served_model.clone());
    // A session's first epoch owes no packet, so it is complete by definition.
    let complete = !is_handover || !s.branch().lags(turn);
    let packet = if is_handover {
        packet::render(&PacketInput {
            epoch_key: &epoch_key,
            from_model: from_model.as_deref().filter(|m| *m != model),
            complete_through: s.branch().watermark,
            incomplete,
            items: &items,
            strategy: state.config.policy.strategy(),
            max_chars: state.config.policy.packet_max_chars,
        })
    } else {
        None
    };
    let drop_tools: Vec<String> = if is_handover && state.config.policy.drop_revoked_tools {
        let offered = chat::tool_names(&ing.body);
        let kinds = state.config.policy.enforce().enforce_kinds;
        let mut out: Vec<String> = items
            .iter()
            .filter(|i| i.is_hard() && i.status == Status::Active && kinds.contains(&i.kind()))
            .filter_map(|i| i.record.enforce.as_ref())
            .filter(|e| e.as_object().is_some_and(|o| o.keys().all(|k| k == "tools")))
            .flat_map(|e| e["tools"].as_array().cloned().unwrap_or_default())
            .filter_map(|t| t.as_str().map(str::to_string))
            .filter(|t| offered.contains(t))
            .collect();
        out.sort();
        out.dedup();
        out
    } else {
        Vec::new()
    };
    state
        .register
        .open_epoch(
            &key,
            &EpochWrite {
                key: epoch_key.clone(),
                trigger: trigger.to_string(),
                to_model: model.to_string(),
                packet: packet.as_ref().map(|p| p.text.clone()),
                citations: packet.as_ref().map(|p| p.citations.clone()).unwrap_or_default(),
                turn,
                through: s.branch().watermark,
                drop_tools: drop_tools.clone(),
            },
            now,
        )
        .await?;
    inc(&state.metrics.epochs_total);
    let b = s.branch_mut();
    b.items = items;
    b.enforcer = enforcer;
    b.epochs += 1;
    b.all_epochs_complete &= complete;
    b.unplanned = false;
    b.epoch_served = None;
    b.weights_changed = false;
    b.epoch = Some(EpochState {
        key: epoch_key.clone(),
        trigger: trigger.to_string(),
        model: model.to_string(),
        packet: packet.map(|p| p.text),
        complete,
        drop_tools,
    });
    let ready = b.all_epochs_complete;
    tracing::info!(session = %s.key, branch = %key, epoch = %epoch_key, trigger, complete, "epoch");
    if is_handover {
        if ready {
            state.register.declare_ready(&key, now).await?;
        }
        spawn_audits(state.clone(), s.key.clone(), key, ready);
    }
    Ok(())
}

fn cold_input(ing: &Ingress, turn: usize, model: &str) -> LastTurn {
    let msgs = chat::messages(&ing.body);
    let history: Vec<Value> = msgs[..turn.min(msgs.len())].to_vec();
    LastTurn {
        conv: Conversation::from_chat(&history, &ing.profile),
        upstream_messages: history,
        assistant: Value::Null,
        model: model.to_string(),
        offset: 0,
        tools: ing.body.get("tools").cloned(),
        auth: forwarded_auth(ing),
    }
}

fn forwarded_auth(ing: &Ingress) -> Option<HeaderValue> {
    if ing.forward_auth { ing.client_auth.clone() } else { None }
}

/// Run the audits for a branch in the background and store the verdicts.
pub fn spawn_audits(state: Shared, session_key: String, branch_key: String, ready: bool) {
    tokio::spawn(async move {
        if let Err(e) = run_audits(&state, &session_key, &branch_key, ready).await {
            tracing::warn!(session = %session_key, "audit failed: {e}");
        }
    });
}

pub async fn run_audits(
    state: &Shared,
    session_key: &str,
    branch_key: &str,
    ready: bool,
) -> anyhow::Result<Vec<handover_register::AuditResult>> {
    let results = state.register.audit(branch_key, ready, state.config.policy.record_audits).await?;
    for r in &results {
        inc(&state.metrics.audits_total);
        if r.verdict == "violated" {
            inc(&state.metrics.audit_violations_total);
            tracing::warn!(session = %session_key, branch = %branch_key, audit = %r.name, findings = r.findings, "audit violated");
        }
    }
    let session = state.sessions.lock().unwrap().get(session_key).cloned();
    if let Some(session) = session {
        let mut s = session.lock().await;
        if let Some(b) = s.branch_by_key(branch_key) {
            b.audits = results.clone();
        }
    }
    Ok(results)
}

fn response_headers(prep: &Prepared, session_key: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("x-handover-status", HeaderValue::from_static(prep.light));
    if let Ok(v) = HeaderValue::from_str(session_key) {
        h.insert("x-handover-session", v);
    }
    if let Some(e) = prep.epoch_key.as_deref().and_then(|e| HeaderValue::from_str(e).ok()) {
        h.insert("x-handover-epoch", e);
    }
    if let Some(l) = prep.lane.as_deref().and_then(|l| HeaderValue::from_str(l).ok()) {
        h.insert("x-handover-lane", l);
    }
    h
}

struct Turn {
    state: Shared,
    session: SessionRef,
    session_key: String,
    branch_key: String,
    model: String,
    upstream_messages: Vec<Value>,
    client_messages: Vec<Value>,
    offset: usize,
    profile: Profile,
    head_digest: Option<String>,
    upstream_tools: Option<Value>,
    seq: u64,
    client_auth: Option<HeaderValue>,
    lane: Option<String>,
    side: bool,
}

async fn forward(state: Shared, session: SessionRef, ing: Ingress, conv: Conversation, prep: Prepared) -> Upstreamed {
    let up = state.config.upstream();
    let url = format!("{}/v1/chat/completions", up.base_url.trim_end_matches('/'));
    let body_bytes = match (&ing.raw, prep.rewritten) {
        (Some(raw), false) => raw.clone(),
        _ => Bytes::from(serde_json::to_vec(&prep.upstream).unwrap_or_default()),
    };
    let mut req = state.http.post(&url).header("content-type", "application/json").body(body_bytes);
    if let Affinity::Header { name } = &up.affinity {
        req = req.header(name.as_str(), ing.session_key.as_str());
    }
    match (&up.api_key, forwarded_auth(&ing)) {
        (Some(k), _) => req = req.bearer_auth(k),
        (None, Some(a)) => req = req.header("authorization", a),
        _ => {}
    }
    let mut headers = response_headers(&prep, &ing.client_key);
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            inc(&state.metrics.upstream_errors_total);
            return error(StatusCode::BAD_GATEWAY, &format!("upstream: {e}"));
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let header_model =
        up.served_model_header.as_ref().and_then(|h| resp.headers().get(h.as_str())).and_then(|v| v.to_str().ok()).map(str::to_string);
    if !status.is_success() {
        inc(&state.metrics.upstream_errors_total);
        if matches!(status.as_u16(), 429 | 503) {
            *state.overloaded_until.lock().unwrap() = Some(std::time::Instant::now() + Duration::from_secs(30));
        }
        let body = resp.bytes().await.unwrap_or_default();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        return Upstreamed::Raw { status, headers, body };
    }
    let turn = Turn {
        state: state.clone(),
        session,
        session_key: ing.session_key.clone(),
        branch_key: prep.branch_key.clone(),
        model: prep.model.clone(),
        upstream_messages: chat::messages(&prep.upstream).to_vec(),
        client_messages: chat::messages(&ing.body).to_vec(),
        offset: prep.offset,
        profile: ing.profile.clone(),
        head_digest: conv.head().map(str::to_string),
        upstream_tools: prep.upstream.get("tools").cloned(),
        seq: prep.seq,
        client_auth: forwarded_auth(&ing),
        lane: prep.lane.clone(),
        side: prep.side,
    };
    let enforcer = prep.enforcer.clone();
    let profile = ing.profile.clone();
    let is_stream = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("text/event-stream"));
    if !is_stream {
        let mut body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_GATEWAY, &format!("upstream body: {e}")),
        };
        let mut decisions = Vec::new();
        let transcript = filter_completion(&mut body, |call| gate(&enforcer, &profile, call, &mut decisions));
        tokio::spawn(after_turn(turn, transcript, decisions, header_model));
        return Upstreamed::Json { status, headers, body };
    }
    headers.insert("content-type", HeaderValue::from_static("text/event-stream"));
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);
    tokio::spawn(async move {
        let mut upstream = resp.bytes_stream();
        let mut parser = SseParser::new();
        let mut decisions = Vec::new();
        let mut client_gone = false;
        let transcript = {
            let mut filter = StreamFilter::new(|call: &ToolCall| gate(&enforcer, &profile, call, &mut decisions));
            while let Some(chunk) = upstream.next().await {
                match chunk {
                    Ok(bytes) => {
                        for ev in parser.push(&bytes) {
                            for out in filter.event(&ev) {
                                if !client_gone && tx.send(Ok(out)).await.is_err() {
                                    client_gone = true;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
                        break;
                    }
                }
            }
            filter.finish();
            filter.into_transcript()
        };
        drop(tx);
        after_turn(turn, transcript, decisions, header_model).await;
    });
    Upstreamed::Stream { headers, rx }
}

/// Decide one complete tool call (§10).
fn gate(enforcer: &Enforcer, profile: &Profile, call: &ToolCall, decisions: &mut Vec<(ToolCall, Action, Verdict)>) -> CallDecision {
    let action = profile.action(&call.name, &call.arguments);
    let verdict = enforcer.decide(&action);
    let out = match verdict.decision {
        Decision::Block | Decision::Hold => {
            CallDecision::Replace(verdict.message(&action).unwrap_or_else(|| "Blocked by a session constraint.".into()))
        }
        _ => CallDecision::Forward,
    };
    decisions.push((call.clone(), action, verdict));
    out
}

/// Record the turn (§5 steps 6–7) and schedule Option B.
async fn after_turn(t: Turn, transcript: Transcript, decisions: Vec<(ToolCall, Action, Verdict)>, header_model: Option<String>) {
    let state = &t.state;
    let now = state.clock.now();
    let turn_index = t.client_messages.len();
    let mut held = false;
    let mut actions = Vec::new();
    for (call, action, verdict) in &decisions {
        inc(&state.metrics.tool_calls_total);
        match verdict.decision {
            Decision::Block => inc(&state.metrics.blocked_total),
            Decision::Hold => {
                inc(&state.metrics.held_total);
                held = true;
            }
            Decision::Warn => inc(&state.metrics.warned_total),
            Decision::Forward => {}
        }
        let record = json!({
            "turn": turn_index, "call_id": call.id, "tool": action.tool, "op": action.op,
            "paths": action.paths, "command": action.command, "urls": action.urls,
        });
        let matched: Vec<String> = verdict.matched.iter().map(|m| m.item_key.clone()).collect();
        actions.push(ActionRecord { action: record, decision: verdict.decision.as_str().to_string(), matched });
    }

    let served = header_model.or(transcript.model.clone());
    if t.side || t.lane.is_some() {
        // Side requests and delegated lanes leave only their tool decisions,
        // on the governing branch: no anchor, no served model, no extraction.
        if let Some(lane) = &t.lane
            && !t.side
        {
            let mut s = t.session.lock().await;
            if let Some(l) = s.lanes.iter_mut().find(|l| &l.key == lane) {
                if l.request_seq == t.seq {
                    l.hold_pending = held;
                }
                if let Some(sv) = &served {
                    if !(sv == &t.model || sv.starts_with(t.model.as_str()) || t.model.starts_with(sv.as_str())) {
                        inc(&state.metrics.unplanned_handovers_total);
                        tracing::warn!(session = %t.session_key, lane = %lane, requested = %t.model, served = %sv, "lane served by another model");
                    }
                    l.served_model = Some(sv.clone());
                }
            }
        }
        if let Err(e) = state.register.record_exchange(&t.branch_key, None, None, &actions, now).await {
            tracing::warn!(session = %t.session_key, "record exchange: {e}");
        }
        return;
    }
    let yields = transcript.yields_to_human();
    let schedule = {
        let mut s = t.session.lock().await;
        let Some(b) = s.branch_by_key(&t.branch_key) else { return };
        // A turn that finishes after a newer request was accepted (the client
        // interrupted and resent) must not overwrite the newer turn's state.
        let current = b.request_seq == t.seq;
        if let Some(sv) = &served {
            // Unplanned substitution: another model than the epoch has been
            // served so far. The first answer of an epoch sets the baseline, so
            // dated aliases (`gpt-4o` → `gpt-4o-2024-08-06`) are not handovers.
            let unplanned = match &b.epoch_served {
                Some(baseline) => sv != baseline,
                None => !(sv == &t.model || sv.starts_with(t.model.as_str()) || t.model.starts_with(sv.as_str())),
            };
            if b.epoch_served.is_none() && !unplanned {
                b.epoch_served = Some(sv.clone());
            }
            if unplanned {
                b.unplanned = true;
                inc(&state.metrics.unplanned_handovers_total);
                tracing::warn!(session = %t.session_key, pinned = %t.model, served = %sv, "unplanned handover");
            }
        }
        if current {
            b.hold_pending = held;
        }
        let served_changed = served.is_some() && served != b.served_model;
        if served_changed {
            b.served_model = served.clone();
        }
        let yields = yields && current;
        if yields {
            let assistant = chat::assistant_message(&transcript);
            let mut all = t.client_messages.clone();
            all.push(assistant.clone());
            b.head = Some(turn_index.saturating_sub(1));
            b.last_turn = Some(LastTurn {
                upstream_messages: t.upstream_messages.clone(),
                assistant,
                model: t.model.clone(),
                conv: Conversation::from_chat(&all, &t.profile),
                offset: t.offset,
                tools: t.upstream_tools.clone(),
                auth: t.client_auth.clone(),
            });
            if b.first_seen.len() < all.len() {
                b.first_seen.resize(all.len(), Some(now));
            }
        }
        (served_changed, yields && state.config.extractor.enabled, yields)
    };
    let yields = schedule.2;
    // One commit for the whole exchange: anchor, served model, decisions.
    let anchor = if yields { t.head_digest.as_deref().map(|d| (turn_index.saturating_sub(1), d)) } else { None };
    let served_model = if schedule.0 { served.as_deref() } else { None };
    if let Err(e) = state.register.record_exchange(&t.branch_key, anchor, served_model, &actions, now).await {
        tracing::warn!(session = %t.session_key, "record exchange: {e}");
    }
    if schedule.1 {
        extract::schedule(state.clone(), t.session.clone(), t.branch_key.clone()).await;
    }
}

/// Evict sessions idle longer than `session_idle_secs` that nothing else holds
/// (no request in flight, no extraction or audit task). Returns how many.
pub fn evict_idle(state: &Shared, idle: Duration) -> usize {
    let mut map = state.sessions.lock().unwrap();
    let before = map.len();
    map.retain(|_, s| {
        if Arc::strong_count(s) > 1 {
            return true;
        }
        match s.try_lock() {
            Ok(guard) => guard.last_used.elapsed() < idle,
            Err(_) => true,
        }
    });
    before - map.len()
}

/// Poll the next hop's `/v1/models` once. A model id whose `root` changed since
/// the last poll now has new weights under the same name: every session whose
/// epoch uses it opens a new epoch on its next request (§6). Returns the ids.
pub async fn poll_models(state: &Shared) -> anyhow::Result<Vec<String>> {
    let up = state.config.upstream();
    let mut req = state.http.get(format!("{}/v1/models", up.base_url.trim_end_matches('/')));
    if let Some(k) = &up.api_key {
        req = req.bearer_auth(k);
    }
    let v: Value = req.send().await?.error_for_status()?.json().await?;
    let mut changed = Vec::new();
    {
        let mut roots = state.model_roots.lock().unwrap();
        for m in v["data"].as_array().into_iter().flatten() {
            let (Some(id), Some(root)) = (m["id"].as_str(), m["root"].as_str()) else { continue };
            if let Some(prev) = roots.insert(id.to_string(), root.to_string())
                && prev != root
            {
                changed.push(id.to_string());
            }
        }
    }
    if changed.is_empty() {
        return Ok(changed);
    }
    let sessions: Vec<SessionRef> = state.sessions.lock().unwrap().values().cloned().collect();
    for s in sessions {
        let mut s = s.lock().await;
        for b in &mut s.branches {
            if b.epoch.as_ref().is_some_and(|e| changed.contains(&e.model)) {
                b.weights_changed = true;
            }
        }
    }
    for id in &changed {
        inc(&state.metrics.weight_swaps_total);
        tracing::warn!(model = %id, "next hop serves new weights under the same name");
    }
    Ok(changed)
}
