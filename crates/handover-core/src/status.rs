//! Status derivation from assertion history (PLAN.md §8.2).
//!
//! A versioned application policy over evidence: Positorium keeps every position,
//! and this module decides what they mean at a cut.

use std::collections::{BTreeMap, HashMap};

use crate::item::{Assertion, Authority, ItemRecord, RegisterItem, Status};

#[derive(Clone, Debug)]
pub struct StatusPolicy {
    /// Below this extractor meta-certainty an authoritative assertion counts as uncertain.
    pub min_meta_certainty: f64,
}

impl Default for StatusPolicy {
    fn default() -> Self {
        StatusPolicy { min_meta_certainty: 0.7 }
    }
}

/// Derive every item's status. Items are returned sorted by sequence number.
pub fn derive(records: &[ItemRecord], policy: &StatusPolicy) -> Vec<RegisterItem> {
    let mut items: Vec<RegisterItem> = records.iter().map(|r| derive_one(r, policy)).collect();
    // Supersession needs the replacement's own status.
    let active: HashMap<String, bool> = items.iter().map(|i| (i.record.key.clone(), i.status == Status::Active)).collect();
    for item in &mut items {
        if item.status == Status::Revoked {
            continue;
        }
        if let Some(by) = item.record.replaced_by.iter().find(|k| active.get(*k).copied().unwrap_or(false)) {
            item.superseded_by = Some(by.clone());
            item.status = Status::Superseded;
        }
    }
    items.sort_by_key(|i| i.record.seq);
    items
}

fn latest_per_source(assertions: &[Assertion]) -> Vec<&Assertion> {
    let mut latest: BTreeMap<&str, &Assertion> = BTreeMap::new();
    for a in assertions {
        match latest.get(a.source.as_str()) {
            Some(prev) if prev.time > a.time => {}
            _ => {
                latest.insert(a.source.as_str(), a);
            }
        }
    }
    latest.into_values().collect()
}

/// Does some source hold opposite positions at the same latest instant?
fn self_contradiction(assertions: &[Assertion]) -> bool {
    let mut by_source: BTreeMap<&str, (&str, bool, bool)> = BTreeMap::new();
    for a in assertions {
        let e = by_source.entry(a.source.as_str()).or_insert((a.time.as_str(), false, false));
        if a.time.as_str() > e.0 {
            *e = (a.time.as_str(), false, false);
        }
        if a.time.as_str() == e.0 {
            if a.certainty > 0.0 {
                e.1 = true;
            } else {
                e.2 = true;
            }
        }
    }
    by_source.values().any(|(_, pos, nonpos)| *pos && *nonpos)
}

fn derive_one(r: &ItemRecord, policy: &StatusPolicy) -> RegisterItem {
    let latest = latest_per_source(&r.assertions);
    let auth: Vec<&&Assertion> = latest.iter().filter(|a| a.authority == Authority::Authoritative).collect();
    let adv: Vec<&&Assertion> = latest.iter().filter(|a| a.authority == Authority::Advisory).collect();

    let mut status = if !auth.is_empty() {
        let pos = auth.iter().any(|a| a.certainty > 0.0);
        let neg = auth.iter().any(|a| a.certainty < 0.0);
        if pos && neg {
            Status::Disputed
        } else if pos {
            let weak = auth.iter().filter(|a| a.certainty > 0.0).all(|a| a.meta_certainty.is_some_and(|m| m < policy.min_meta_certainty));
            if weak { Status::Uncertain } else { Status::Active }
        } else {
            // A withdrawal the extractor is unsure the source made is not final
            // either: loosening is expensive (D7), so the item holds.
            let weak = auth.iter().all(|a| a.meta_certainty.is_some_and(|m| m < policy.min_meta_certainty));
            if weak { Status::Uncertain } else { Status::Revoked }
        }
    } else if adv.is_empty() {
        Status::Uncertain
    } else {
        let pos = adv.iter().any(|a| a.certainty > 0.0);
        let neg = adv.iter().any(|a| a.certainty < 0.0);
        match (pos, neg) {
            (true, true) => Status::Disputed,
            (true, false) => Status::Uncertain,
            _ => Status::Revoked,
        }
    };

    // Latest wording; equal-time different wordings are a dispute about wording.
    let (statement, conflicting) = match r.statements.iter().map(|(_, t)| t).max() {
        Some(max_t) => {
            let at_max: Vec<&String> = r.statements.iter().filter(|(_, t)| t == max_t).map(|(s, _)| s).collect();
            let first = at_max[0].clone();
            let others: Vec<String> = at_max[1..].iter().filter(|s| **s != &first).map(|s| (*s).clone()).collect();
            (first, others)
        }
        None => (String::new(), Vec::new()),
    };
    if !conflicting.is_empty() && matches!(status, Status::Active | Status::Uncertain) {
        status = Status::Disputed;
    }
    // Never let row order decide between equal-time opposite positions (D8).
    if self_contradiction(&r.assertions) {
        status = Status::Disputed;
    }

    RegisterItem { record: r.clone(), status, statement, conflicting_statements: conflicting, superseded_by: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Kind, Strength};

    fn a(source: &str, auth: bool, c: f64, t: &str) -> Assertion {
        Assertion {
            source: source.into(),
            authority: if auth { Authority::Authoritative } else { Authority::Advisory },
            certainty: c,
            time: format!("2026-11-20 {t}"),
            meta_certainty: Some(0.9),
        }
    }

    fn rec(seq: u64, assertions: Vec<Assertion>) -> ItemRecord {
        ItemRecord {
            key: format!("s#{seq}"),
            seq,
            kind: Kind::Prohibition,
            strength: Strength::Hard,
            statements: vec![(format!("item {seq}"), "2026-11-20 10:00:00".into())],
            enforce: None,
            citations: vec![],
            assertions,
            replaced_by: vec![],
            requires: vec![],
            introduced: "2026-11-20 10:00:00".into(),
        }
    }

    /// The paper's coding case (Q2 in the spike).
    #[test]
    fn coding_case() {
        let p = rec(1, vec![a("developer", true, 1.0, "10:00"), a("developer", true, 0.0, "10:30")]);
        let c = rec(2, vec![a("model", false, 0.8, "10:10"), a("developer", true, -1.0, "10:20")]);
        let g = rec(3, vec![a("model", false, 0.8, "10:40"), a("tool:pytest", false, -0.9, "10:41")]);
        let h = rec(4, vec![a("developer", true, 1.0, "10:50"), a("model", false, -0.5, "10:51")]);
        let mut w = rec(5, vec![a("developer", true, 1.0, "11:00")]);
        w.statements.push(("other wording".into(), "2026-11-20 10:00:00".into()));
        let out = derive(&[p, c, g, h, w], &StatusPolicy::default());
        let st: Vec<Status> = out.iter().map(|i| i.status).collect();
        assert_eq!(st, vec![Status::Revoked, Status::Revoked, Status::Disputed, Status::Active, Status::Disputed]);
    }

    #[test]
    fn low_meta_certainty_and_supersession() {
        let mut weak = a("developer", true, 1.0, "10:00");
        weak.meta_certainty = Some(0.4);
        let old = {
            let mut r = rec(1, vec![a("developer", true, 1.0, "09:00")]);
            r.replaced_by = vec!["s#2".into()];
            r
        };
        let new = rec(2, vec![a("developer", true, 1.0, "10:00")]);
        let uncertain = rec(3, vec![weak]);
        let only_model = rec(4, vec![a("model", false, 0.8, "10:00")]);
        let out = derive(&[old, new, uncertain, only_model], &StatusPolicy::default());
        assert_eq!(out[0].status, Status::Superseded);
        assert_eq!(out[0].superseded_by.as_deref(), Some("s#2"));
        assert_eq!(out[1].status, Status::Active);
        assert_eq!(out[2].status, Status::Uncertain);
        assert_eq!(out[3].status, Status::Uncertain);
    }

    /// Review findings: a weak revoke is not final; equal-time opposite positions dispute.
    #[test]
    fn weak_revocations_hold_and_equal_time_conflicts_dispute() {
        let mut weak_revoke = a("developer", true, 0.0, "10:30");
        weak_revoke.meta_certainty = Some(0.2);
        let weak = rec(1, vec![a("developer", true, 1.0, "10:00"), weak_revoke]);
        let tie = rec(2, vec![a("developer", true, 1.0, "10:00"), a("developer", true, 0.0, "10:00")]);
        let tie_reversed = rec(3, vec![a("developer", true, 0.0, "10:00"), a("developer", true, 1.0, "10:00")]);
        let out = derive(&[weak, tie, tie_reversed], &StatusPolicy::default());
        assert_eq!(out[0].status, Status::Uncertain);
        assert_eq!(out[1].status, Status::Disputed);
        assert_eq!(out[2].status, Status::Disputed, "row order does not matter");
    }
}
