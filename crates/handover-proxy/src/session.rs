//! In-memory session state (PLAN.md §5, §6). Everything here is a cache that can
//! be rebuilt from Positorium (`restore`) or from the next request (D2).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use handover_core::clock;
use handover_core::enforce::Enforcer;
use handover_core::item::{RegisterItem, Status};
use handover_core::{Conversation, LaneView};
use handover_register::{AuditResult, SessionSnapshot};
use serde::Serialize;
use serde_json::Value;
use tokio::task::JoinHandle;

/// The epoch a branch is in: model, and the exact packet bytes re-injected all epoch.
#[derive(Clone, Debug, Serialize)]
pub struct EpochState {
    pub key: String,
    pub trigger: String,
    pub model: String,
    pub packet: Option<String>,
    /// Built from a register complete through the epoch's turn.
    pub complete: bool,
    /// Tool definitions removed from every upstream request of this epoch.
    pub drop_tools: Vec<String>,
}

/// What the worker last served on a human-facing turn: the exact upstream
/// messages plus the reply, for an Option B request on the hot prefix.
#[derive(Clone, Debug)]
pub struct LastTurn {
    pub upstream_messages: Vec<Value>,
    pub assistant: Value,
    pub model: String,
    /// Client conversation including the reply (canonical indexes).
    pub conv: Conversation,
    /// Messages the proxy inserted before the client's first one.
    pub offset: usize,
    /// Tool definitions exactly as served (part of the rendered prefix).
    pub tools: Option<Value>,
    /// The client's credentials, kept only when `forward_client_auth` is on (the
    /// next hop then authenticates extraction requests the same way).
    pub auth: Option<axum::http::HeaderValue>,
}

#[derive(Debug, Serialize)]
pub struct BranchState {
    pub key: String,
    pub parent: Option<String>,
    pub fork_common: usize,
    /// Chain digests; `None` where only sparse anchors survived a restart.
    pub chain: Vec<Option<String>>,
    /// Body chain of the last accepted request (empty after a restart).
    #[serde(skip)]
    pub body: Vec<String>,
    #[serde(skip)]
    pub preamble: usize,
    /// System-prompt identity of the last accepted request (unknown after a restart).
    pub identity: Option<String>,
    #[serde(skip)]
    pub first_seen: Vec<Option<DateTime<Utc>>>,
    pub first_human: Option<usize>,
    pub epoch: Option<EpochState>,
    pub epochs: usize,
    pub all_epochs_complete: bool,
    /// Last canonical message index covered by extraction.
    pub watermark: Option<usize>,
    /// Index of the last human-facing turn's final message.
    pub head: Option<usize>,
    #[serde(skip)]
    pub last_turn: Option<LastTurn>,
    pub served_model: Option<String>,
    /// The next hop answered with another model; a new epoch opens next request.
    pub unplanned: bool,
    /// The next hop now serves different weights under the epoch's model name.
    pub weights_changed: bool,
    pub tools: Vec<String>,
    #[serde(skip)]
    pub enforcer: Arc<Enforcer>,
    pub items: Vec<RegisterItem>,
    pub audits: Vec<AuditResult>,
    pub hold_pending: bool,
    /// Incremented per accepted request; a turn that finishes after a newer
    /// request was accepted must not overwrite the newer state.
    #[serde(skip)]
    pub request_seq: u64,
    /// The served model name first seen in this epoch (aliases like
    /// `gpt-4o` → `gpt-4o-2024-08-06` are not substitutions).
    pub epoch_served: Option<String>,
    #[serde(skip)]
    pub extraction: Option<JoinHandle<()>>,
    pub extracting: bool,
}

impl BranchState {
    pub fn new(key: String, parent: Option<String>, fork_common: usize) -> Self {
        BranchState {
            key,
            parent,
            fork_common,
            chain: Vec::new(),
            body: Vec::new(),
            preamble: 0,
            identity: None,
            first_seen: Vec::new(),
            first_human: None,
            epoch: None,
            epochs: 0,
            all_epochs_complete: true,
            watermark: None,
            head: None,
            last_turn: None,
            served_model: None,
            unplanned: false,
            weights_changed: false,
            tools: Vec::new(),
            enforcer: Arc::new(Enforcer::default()),
            items: Vec::new(),
            audits: Vec::new(),
            hold_pending: false,
            request_seq: 0,
            epoch_served: None,
            extraction: None,
            extracting: false,
        }
    }

    /// Remember the chain of the request just accepted.
    pub fn accept_chain(&mut self, conv: &Conversation, identity: &str, now: DateTime<Utc>) {
        let old_seen = std::mem::take(&mut self.first_seen);
        self.chain = conv.chain().into_iter().map(Some).collect();
        self.first_seen = (0..conv.len()).map(|i| old_seen.get(i).copied().flatten().or(Some(now))).collect();
        self.first_human = conv.first_human();
        self.body.clone_from(&conv.body_chain);
        self.preamble = conv.preamble;
        self.identity = Some(identity.to_string());
    }

    pub fn view(&self) -> LaneView<'_> {
        LaneView {
            chain: &self.chain,
            body: &self.body,
            preamble: self.preamble,
            first_human: self.first_human,
            identity: self.identity.as_deref(),
            delegated: false,
        }
    }

    /// When message `i` was first seen (for the appearance time of what it says).
    pub fn seen_at(&self, i: usize) -> Option<DateTime<Utc>> {
        self.first_seen.get(i).copied().flatten()
    }

    /// Distinct, ordered appearance times for messages `0..len`: the arrival time
    /// where known, otherwise one microsecond after the previous message, so two
    /// changes from different messages never share a time (and "latest" holds).
    pub fn message_times(&self, len: usize, now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        // Unknown messages before the first known arrival get the microseconds just before it.
        let mut prev = match (0..len).find_map(|i| self.seen_at(i).map(|t| (i, t))) {
            Some((i, t)) => t - chrono::Duration::microseconds(i as i64 + 1),
            None => now - chrono::Duration::microseconds(len as i64 + 1),
        };
        let mut out: Vec<DateTime<Utc>> = Vec::with_capacity(len);
        for i in 0..len {
            let t = match self.seen_at(i) {
                Some(t) if t > prev => t,
                _ => prev + chrono::Duration::microseconds(1),
            };
            out.push(t);
            prev = t;
        }
        out
    }

    /// Unextracted messages before index `upto` (exclusive)?
    pub fn lags(&self, upto: usize) -> bool {
        upto > 0 && self.watermark.is_none_or(|w| w + 1 < upto)
    }

    pub fn next_new_from(&self) -> usize {
        self.watermark.map(|w| w + 1).unwrap_or(0)
    }
}

/// A delegated lane (PLAN.md §6): a subagent's or side call's conversation in
/// the same session. It has no register branch of its own: its parent branch's
/// items govern it live, and its denials and tool decisions are written there.
/// It never extracts, opens no epochs and is not restored after a restart.
#[derive(Debug, Serialize)]
pub struct Lane {
    pub key: String,
    /// The main branch whose register governs the lane.
    pub parent: String,
    #[serde(skip)]
    pub chain: Vec<Option<String>>,
    #[serde(skip)]
    pub body: Vec<String>,
    #[serde(skip)]
    pub preamble: usize,
    #[serde(skip)]
    pub first_human: Option<usize>,
    #[serde(skip)]
    pub first_seen: Vec<Option<DateTime<Utc>>>,
    pub identity: String,
    pub tools: Vec<String>,
    /// Rendered from the parent's register when the lane opened (only for a
    /// lane that can act, i.e. offers tools) and re-injected byte-identically.
    pub packet: Option<String>,
    pub model: String,
    pub served_model: Option<String>,
    pub requests: u64,
    #[serde(skip)]
    pub request_seq: u64,
    pub hold_pending: bool,
    #[serde(skip)]
    pub last_used: std::time::Instant,
}

impl Lane {
    pub fn new(key: String, parent: String, packet: Option<String>) -> Self {
        Lane {
            key,
            parent,
            chain: Vec::new(),
            body: Vec::new(),
            preamble: 0,
            first_human: None,
            first_seen: Vec::new(),
            identity: String::new(),
            tools: Vec::new(),
            packet,
            model: String::new(),
            served_model: None,
            requests: 0,
            request_seq: 0,
            hold_pending: false,
            last_used: std::time::Instant::now(),
        }
    }

    pub fn accept_chain(&mut self, conv: &Conversation, identity: &str, now: DateTime<Utc>) {
        let old_seen = std::mem::take(&mut self.first_seen);
        self.chain = conv.chain().into_iter().map(Some).collect();
        self.first_seen = (0..conv.len()).map(|i| old_seen.get(i).copied().flatten().or(Some(now))).collect();
        self.first_human = conv.first_human();
        self.body.clone_from(&conv.body_chain);
        self.preamble = conv.preamble;
        identity.clone_into(&mut self.identity);
    }

    pub fn seen_at(&self, i: usize) -> Option<DateTime<Utc>> {
        self.first_seen.get(i).copied().flatten()
    }

    pub fn view(&self) -> LaneView<'_> {
        LaneView {
            chain: &self.chain,
            body: &self.body,
            preamble: self.preamble,
            first_human: self.first_human,
            identity: Some(&self.identity),
            delegated: true,
        }
    }
}

/// Delegated lanes kept per session; the least recently used one goes first.
pub const MAX_LANES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Light {
    Green,
    Amber,
    Red,
}

impl Light {
    pub fn as_str(self) -> &'static str {
        match self {
            Light::Green => "green",
            Light::Amber => "amber",
            Light::Red => "red",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Session {
    pub key: String,
    /// The key as the client knows it (the internal key is namespaced per developer).
    pub client_key: String,
    pub developer: String,
    pub profile: String,
    /// Fallback key: does not survive compaction, so always amber.
    pub weak_identity: bool,
    pub branches: Vec<BranchState>,
    /// The main branch used most recently.
    pub current: usize,
    /// Delegated lanes (subagents, side calls); in memory only.
    pub lanes: Vec<Lane>,
    pub epoch_counter: usize,
    pub branch_counter: usize,
    pub lane_counter: usize,
    /// For idle eviction; the session is rebuilt from the store when it returns.
    #[serde(skip)]
    pub last_used: std::time::Instant,
}

impl Session {
    pub fn new(key: String, developer: String, profile: String, weak: bool) -> Self {
        let root = handover_register::root_branch(&key);
        Session {
            client_key: key.clone(),
            key,
            developer,
            profile,
            weak_identity: weak,
            branches: vec![BranchState::new(root, None, 0)],
            current: 0,
            lanes: Vec::new(),
            epoch_counter: 0,
            branch_counter: 0,
            lane_counter: 0,
            last_used: std::time::Instant::now(),
        }
    }

    pub fn branch(&self) -> &BranchState {
        &self.branches[self.current]
    }

    pub fn branch_mut(&mut self) -> &mut BranchState {
        &mut self.branches[self.current]
    }

    pub fn branch_by_key(&mut self, key: &str) -> Option<&mut BranchState> {
        self.branches.iter_mut().find(|b| b.key == key)
    }

    pub fn next_epoch_key(&mut self) -> String {
        self.epoch_counter += 1;
        format!("{}#e{}", self.key, self.epoch_counter)
    }

    pub fn next_branch_key(&mut self) -> String {
        self.branch_counter += 1;
        format!("{}#b{}", self.key, self.branch_counter)
    }

    pub fn next_lane_key(&mut self) -> String {
        self.lane_counter += 1;
        format!("{}#l{}", self.key, self.lane_counter)
    }

    /// Rebuild from the store after a restart: sparse chains from turn anchors,
    /// the latest epoch with its exact packet bytes, watermark and served model.
    pub fn restore(&mut self, snap: &SessionSnapshot) {
        self.branches.clear();
        let mut latest_activity: Vec<String> = Vec::new();
        for b in &snap.branches {
            let mut st = BranchState::new(b.key.clone(), None, b.fork_common.max(0) as usize);
            let len = b.anchors.iter().map(|(i, _, _)| i + 1).max().unwrap_or(0);
            st.chain = vec![None; len];
            st.first_seen = vec![None; len];
            for (i, d, t) in &b.anchors {
                st.chain[*i] = Some(d.clone());
                st.first_seen[*i] = clock::parse(t);
            }
            st.head = b.anchors.last().map(|(i, _, _)| *i);
            // Anchors are written only at human-facing turns, so the first one is at
            // or after the first human message: an edit past it is a fork, not a compaction.
            st.first_human = b.anchors.first().map(|(i, _, _)| *i);
            st.epochs = b.epochs.len();
            st.all_epochs_complete = b.epochs.iter().all(|e| e.trigger == "session_start" || e.through + 1 >= e.turn);
            st.epoch = b.epochs.last().map(|e| EpochState {
                key: e.key.clone(),
                trigger: e.trigger.clone(),
                model: e.model.clone(),
                packet: e.packet.clone(),
                complete: e.trigger == "session_start" || e.through + 1 >= e.turn,
                drop_tools: e.drop_tools.clone(),
            });
            st.watermark = b.watermark.filter(|w| *w >= 0).map(|w| w as usize);
            st.served_model = b.served_model.clone();
            let activity = b.anchors.last().map(|a| a.2.clone()).unwrap_or_else(|| b.created.clone());
            latest_activity.push(activity);
            self.epoch_counter += b.epochs.len();
            self.branches.push(st);
        }
        self.branch_counter = self.branches.len().saturating_sub(1);
        if self.branches.is_empty() {
            self.branches.push(BranchState::new(handover_register::root_branch(&self.key), None, 0));
        }
        self.current = latest_activity.iter().enumerate().max_by(|a, b| a.1.cmp(b.1)).map(|(i, _)| i).unwrap_or(0);
    }

    /// Green / amber / red (PLAN.md §11, paper §6.4).
    pub fn light(&self) -> Light {
        let b = self.branch();
        let active_hard = b.items.iter().any(|i| i.is_hard() && i.status == Status::Active);
        if b.audits.iter().any(|a| a.verdict == "violated") || (b.unplanned && active_hard) {
            return Light::Red;
        }
        let unresolved_hard = b.items.iter().any(|i| i.is_hard() && matches!(i.status, Status::Uncertain | Status::Disputed));
        let lag = b.head.is_some_and(|h| b.lags(h + 1));
        let indeterminate = b.audits.iter().any(|a| a.verdict != "satisfied");
        let lane_hold = self.lanes.iter().any(|l| l.hold_pending && l.parent == b.key);
        if self.weak_identity || unresolved_hard || lag || b.hold_pending || lane_hold || indeterminate || b.unplanned {
            return Light::Amber;
        }
        Light::Green
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn message_times_are_distinct_and_ordered() {
        let mut b = BranchState::new("b".into(), None, 0);
        let t = |s: i64| Utc.timestamp_opt(1_800_000_000 + s, 0).unwrap();
        b.first_seen = vec![None, Some(t(10)), None, None, Some(t(20)), None];
        let times = b.message_times(7, t(30));
        assert!(times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
        assert_eq!(times[1], t(10));
        assert_eq!(times[4], t(20));
        // With nothing known, times still increase and end before now.
        let fresh = BranchState::new("c".into(), None, 0).message_times(3, t(30));
        assert!(fresh.windows(2).all(|w| w[0] < w[1]) && fresh[2] < t(30));
    }
}
