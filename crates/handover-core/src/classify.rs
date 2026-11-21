//! Request classification against a session's known digest chain (PLAN.md §5, §6).

use serde::Serialize;

/// How an incoming request relates to the branch the proxy knows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Classification {
    /// No known chain for this session.
    New,
    /// The request extends (or repeats) the known chain.
    Continuation,
    /// The chain breaks at or before the first human turn: the harness replaced
    /// its history (compaction). Items from dropped turns survive only via the register.
    Compaction,
    /// The chain breaks after a human turn: rewind, edit or fork. `common` is the
    /// number of leading messages both chains share.
    Branch { common: usize },
}

/// Classify `incoming` against `known`, where `known[i]` is the stored chain
/// digest at message `i`, or `None` where only sparse anchors survive (after a
/// restart the proxy knows digests only at human-facing turn anchors).
/// `first_human` is the index of the first human message of the known chain.
pub fn classify(known: &[Option<String>], incoming: &[String], first_human: Option<usize>) -> Classification {
    if known.iter().all(Option::is_none) {
        return Classification::New;
    }
    // The chain property means agreement at i implies agreement before i, so the
    // shared prefix ends right after the last known position that still matches.
    let mut common = 0;
    let mut mismatch = false;
    for (i, k) in known.iter().enumerate() {
        let Some(k) = k else { continue };
        match incoming.get(i) {
            Some(d) if d == k => common = i + 1,
            _ => {
                mismatch = true;
                break;
            }
        }
    }
    if !mismatch && incoming.len() >= known.len() {
        return Classification::Continuation;
    }
    if !mismatch {
        // A strict prefix of the known chain: the developer rewound.
        return Classification::Branch { common: incoming.len() };
    }
    match first_human {
        Some(h) if common > h => Classification::Branch { common },
        _ => Classification::Compaction,
    }
}

/// What the router knows about one lane of a session: a register branch the
/// main conversation runs on, or a delegated lane (subagent, side call).
#[derive(Clone, Copy, Debug)]
pub struct LaneView<'a> {
    pub chain: &'a [Option<String>],
    /// Body chain (messages after the system prompt); empty when unknown.
    pub body: &'a [String],
    /// Number of system-prompt messages before the body.
    pub preamble: usize,
    pub first_human: Option<usize>,
    /// System-prompt identity; `None` when unknown (after a restart).
    pub identity: Option<&'a str>,
    pub delegated: bool,
}

/// The request being routed.
#[derive(Clone, Copy, Debug)]
pub struct Incoming<'a> {
    pub chain: &'a [String],
    pub body: &'a [String],
    pub preamble: usize,
    pub identity: &'a str,
    /// The harness marks this conversation as delegated (a subagent).
    pub delegated: bool,
    /// The history starts with the harness's compaction summary.
    pub compaction_summary: bool,
}

/// Where a request belongs (indexes are into the `lanes` slice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Route {
    /// Nothing known yet: the request starts the current main lane.
    New,
    /// The request extends (or repeats) this lane's chain.
    Continue { lane: usize },
    /// Same conversation, new system prompt (the harness re-prompted it, a
    /// model switch in an IDE, a harness update between turns).
    Reprompt { lane: usize },
    /// Rewind, edit or fork of this lane after its first human turn.
    Branch { lane: usize, common: usize },
    /// This main lane's history was replaced.
    Compaction { lane: usize },
    /// A new delegated lane: a subagent or side call with its own system prompt.
    Delegate,
}

/// Route a request to the lane whose history it continues, across every lane
/// of the session (not just the current one), so a subagent's requests and the
/// main conversation's requests interleave without looking like rewinds or
/// compactions of each other. `current` is the main lane used most recently.
pub fn route(lanes: &[LaneView], current: usize, inc: &Incoming) -> Route {
    let known = |l: &LaneView| l.chain.iter().any(Option::is_some);
    // 1. An exact continuation of any lane; the longest known chain wins.
    let mut best: Option<(usize, usize)> = None;
    for (i, l) in lanes.iter().enumerate().filter(|(_, l)| known(l)) {
        if classify(l.chain, inc.chain, l.first_human) == Classification::Continuation && best.is_none_or(|(_, n)| l.chain.len() > n) {
            best = Some((i, l.chain.len()));
        }
    }
    if let Some((lane, _)) = best {
        return Route::Continue { lane };
    }
    // 2. The same body under another system prompt. The lane must have had a
    // turn already, or the request must add one: a one-message side call that
    // happens to repeat the developer's first message is not a re-prompt.
    let mut best: Option<(usize, usize)> = None;
    for (i, l) in lanes.iter().enumerate() {
        let n = l.body.len();
        // Message indexes must stay aligned, so the system prompt keeps its length.
        if n == 0 || l.preamble != inc.preamble || inc.body.len() < n || inc.body[..n] != *l.body || (inc.body.len() == n && n < 2) {
            continue;
        }
        if inc.delegated && !l.delegated {
            continue;
        }
        let better = match best {
            None => true,
            Some((j, m)) => n > m || (n == m && lanes[j].delegated && !l.delegated),
        };
        if better {
            best = Some((i, n));
        }
    }
    if let Some((lane, _)) = best {
        return Route::Reprompt { lane };
    }
    // 3. A rewind or edit after the first human turn; the longest shared prefix wins.
    let mut best: Option<(usize, usize)> = None;
    for (i, l) in lanes.iter().enumerate().filter(|(_, l)| known(l)) {
        if let Classification::Branch { common } = classify(l.chain, inc.chain, l.first_human)
            && best.is_none_or(|(_, c)| common > c)
        {
            best = Some((i, common));
        }
    }
    if let Some((lane, common)) = best {
        return Route::Branch { lane, common };
    }
    // 4. The harness says this conversation is delegated.
    if inc.delegated {
        return Route::Delegate;
    }
    // 5. The current main lane has seen nothing yet.
    let cur = &lanes[current];
    if !known(cur) {
        return Route::New;
    }
    let main_with_identity =
        || lanes.iter().enumerate().rev().filter(|(_, l)| !l.delegated && l.identity == Some(inc.identity)).map(|(i, _)| i);
    let prefer_current = |found: Option<usize>| {
        if cur.identity == Some(inc.identity) { current } else { found.unwrap_or(current) }
    };
    // 6. The harness's own compaction summary: a compaction of the main lane
    // with this system prompt, or of the current one.
    if inc.compaction_summary {
        return Route::Compaction { lane: prefer_current(main_with_identity().next()) };
    }
    // 7. A main lane's system prompt: that lane's history was replaced (a
    // harness without a recognizable summary, or an edit of the first message).
    if let Some(found) = main_with_identity().next() {
        return Route::Compaction { lane: prefer_current(Some(found)) };
    }
    // 8. Another system prompt than every main lane we can identify: a subagent
    // or a side call (titles, topic checks) of the same session.
    if lanes.iter().any(|l| !l.delegated && l.identity.is_some()) {
        return Route::Delegate;
    }
    // 9. Nothing to compare with (after a restart): the old rule.
    Route::Compaction { lane: current }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn k(xs: &[&str]) -> Vec<Option<String>> {
        xs.iter().map(|s| if s.is_empty() { None } else { Some(s.to_string()) }).collect()
    }

    #[test]
    fn cases() {
        let known = k(&["s", "u1", "a1", "u2"]);
        assert_eq!(classify(&[], &d(&["s"]), None), Classification::New);
        assert_eq!(classify(&known, &d(&["s", "u1", "a1", "u2", "a2", "u3"]), Some(1)), Classification::Continuation);
        assert_eq!(classify(&known, &d(&["s", "u1", "a1", "u2"]), Some(1)), Classification::Continuation);
        assert_eq!(classify(&known, &d(&["s", "u1", "x1", "u9"]), Some(1)), Classification::Branch { common: 2 });
        assert_eq!(classify(&known, &d(&["s", "u1"]), Some(1)), Classification::Branch { common: 2 });
        assert_eq!(classify(&known, &d(&["s", "summary", "u3"]), Some(1)), Classification::Compaction);
        assert_eq!(classify(&known, &d(&["other"]), Some(1)), Classification::Compaction);
    }

    fn lane<'a>(chain: &'a [Option<String>], body: &'a [String], identity: Option<&'a str>, delegated: bool) -> LaneView<'a> {
        LaneView { chain, body, preamble: 1, first_human: Some(1), identity, delegated }
    }

    fn inc<'a>(chain: &'a [String], body: &'a [String], identity: &'a str) -> Incoming<'a> {
        Incoming { chain, body, preamble: 1, identity, delegated: false, compaction_summary: false }
    }

    #[test]
    fn routes_interleaved_lanes() {
        // Main lane: system S, then u1 a1 u2. A subagent: system T, then t1 b1.
        let main = k(&["s", "u1", "a1", "u2"]);
        let main_body = d(&["U1", "A1", "U2"]);
        let sub = k(&["t", "t1", "b1"]);
        let sub_body = d(&["T1", "B1"]);
        let lanes = [lane(&main, &main_body, Some("S"), false), lane(&sub, &sub_body, Some("T"), true)];

        // The main conversation resumes after the subagent: not a compaction.
        let (c, b) = (d(&["s", "u1", "a1", "u2", "a2", "u3"]), d(&["U1", "A1", "U2", "A2", "U3"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "S")), Route::Continue { lane: 0 });
        // The subagent's next request continues its own lane.
        let (c, b) = (d(&["t", "t1", "b1", "r1"]), d(&["T1", "B1", "R1"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "T")), Route::Continue { lane: 1 });
        // A second subagent of the same type: a new lane, not a compaction of the first.
        let (c, b) = (d(&["t'", "t2"]), d(&["T2"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "T")), Route::Delegate);
        // A side call with another system prompt: delegated too.
        let (c, b) = (d(&["x", "q"]), d(&["Q"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "X")), Route::Delegate);
        // Marked as a subagent by the harness, whatever its system prompt.
        let (c, b) = (d(&["s2", "v"]), d(&["V"]));
        assert_eq!(route(&lanes, 0, &Incoming { delegated: true, ..inc(&c, &b, "S") }), Route::Delegate);
        // A rewind of the main lane.
        let (c, b) = (d(&["s", "u1", "a1", "zz"]), d(&["U1", "A1", "ZZ"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "S")), Route::Branch { lane: 0, common: 3 });
        // The harness compacted: summary first, system prompt slightly changed.
        let (c, b) = (d(&["s3", "sum", "u9"]), d(&["SUM", "U9"]));
        assert_eq!(route(&lanes, 0, &Incoming { compaction_summary: true, ..inc(&c, &b, "S3") }), Route::Compaction { lane: 0 });
        // Same system prompt, history replaced without a recognizable summary.
        let (c, b) = (d(&["s", "other"]), d(&["OTHER"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "S")), Route::Compaction { lane: 0 });
    }

    #[test]
    fn reprompt_keeps_the_lane() {
        let main = k(&["s", "u1", "a1", "u2"]);
        let main_body = d(&["U1", "A1", "U2"]);
        let lanes = [lane(&main, &main_body, Some("S"), false)];
        // A model switch in an IDE that sends another system prompt per model.
        let (c, b) = (d(&["s'", "u1'", "a1'", "u2'", "a2", "u3"]), d(&["U1", "A1", "U2", "A2", "U3"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "S'")), Route::Reprompt { lane: 0 });
        // A one-message side call that repeats the developer's first message is not.
        let first = k(&["s", "u1"]);
        let first_body = d(&["U1"]);
        let lanes = [lane(&first, &first_body, Some("S"), false)];
        let (c, b) = (d(&["title", "u1'"]), d(&["U1"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "TITLE")), Route::Delegate);
    }

    #[test]
    fn fresh_and_restored_sessions() {
        let empty: [Option<String>; 0] = [];
        let lanes = [lane(&empty, &[], None, false)];
        let (c, b) = (d(&["s", "u1"]), d(&["U1"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "S")), Route::New);
        assert_eq!(route(&lanes, 0, &Incoming { delegated: true, ..inc(&c, &b, "S") }), Route::Delegate);
        // After a restart only sparse anchors and no identity are known.
        let restored = k(&["", "u1", "", "u2"]);
        let lanes = [lane(&restored, &[], None, false)];
        let (c, b) = (d(&["s", "u1", "a1", "u2", "a2"]), d(&["U1", "A1", "U2", "A2"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "S")), Route::Continue { lane: 0 });
        let (c, b) = (d(&["t", "t1"]), d(&["T1"]));
        assert_eq!(route(&lanes, 0, &inc(&c, &b, "T")), Route::Compaction { lane: 0 });
    }

    #[test]
    fn sparse_anchors_after_restart() {
        let known = k(&["", "u1", "", "u2"]);
        assert_eq!(classify(&known, &d(&["s", "u1", "a1", "u2", "a2", "u3"]), Some(1)), Classification::Continuation);
        assert_eq!(classify(&known, &d(&["s", "u1", "a1", "zz", "a2"]), Some(1)), Classification::Branch { common: 2 });
    }
}
