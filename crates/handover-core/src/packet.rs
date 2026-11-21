//! Handoff packet rendering (PLAN.md §9).
//!
//! Deterministic: the same register and epoch produce the same bytes. The proxy
//! stores those bytes in Positorium and re-injects them verbatim all epoch (D5).

use serde::{Deserialize, Serialize};

use crate::item::{Kind, RegisterItem, Status};

/// Packet conditions of the paper's prospective protocol (§8.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketStrategy {
    /// Status-aware core: hard invariants and their status first (default).
    #[default]
    StatusAwareCore,
    /// Every statement ever made, as plain notes, without status.
    SurfaceRich,
    /// Items in introduction order with their status in parentheses.
    Chronological,
    /// No packet at all.
    None,
}

impl PacketStrategy {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "status_aware_core" => Self::StatusAwareCore,
            "surface_rich" => Self::SurfaceRich,
            "chronological" => Self::Chronological,
            "none" => Self::None,
            _ => return None,
        })
    }
}

pub struct PacketInput<'a> {
    pub epoch_key: &'a str,
    /// Model that answered before this epoch, if any.
    pub from_model: Option<&'a str>,
    /// Last message index covered by extraction.
    pub complete_through: Option<usize>,
    /// Turns the register may be missing (Option A timed out): `(from, to)`.
    pub incomplete: Option<(usize, usize)>,
    pub items: &'a [RegisterItem],
    pub strategy: PacketStrategy,
    /// Character budget; graded lines are dropped first, hard lines never.
    pub max_chars: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Packet {
    pub text: String,
    /// `(item key, label)` for every cited item, as the packet presents it:
    /// `hard-active`, `revoked`, `superseded`, `unresolved`, `active`, or
    /// `noted` (listed without status).
    pub citations: Vec<(String, String)>,
}

struct Line {
    text: String,
    key: String,
    label: &'static str,
    droppable: bool,
}

fn positor_label(source: &str) -> &str {
    source.split(':').next().unwrap_or(source)
}

fn provenance(item: &RegisterItem) -> String {
    let source = item.record.assertions.iter().rev().find(|a| a.certainty > 0.0).map(|a| positor_label(&a.source).to_string());
    let cite = item.record.citations.iter().find(|c| c.quote().is_some_and(|q| !q.is_empty()) || c.turn().is_some());
    match (source, cite) {
        (Some(s), Some(c)) => match (c.turn(), c.quote()) {
            (Some(t), Some(q)) if !q.is_empty() => format!(" ({s}, turn {t}: \"{q}\")"),
            (Some(t), _) => format!(" ({s}, turn {t})"),
            _ => format!(" ({s})"),
        },
        (Some(s), None) => format!(" ({s})"),
        _ => String::new(),
    }
}

fn revocation(item: &RegisterItem) -> String {
    let by = item
        .record
        .assertions
        .iter()
        .rev()
        .find(|a| a.certainty <= 0.0)
        .map(|a| positor_label(&a.source).to_string())
        .unwrap_or_else(|| "an authoritative source".into());
    let turn = item.record.citations.iter().rev().find(|c| matches!(c.op(), Some("revoke" | "dispute"))).and_then(|c| c.turn());
    match turn {
        Some(t) => format!("revoked by {by}, turn {t}"),
        None => format!("revoked by {by}"),
    }
}

fn unresolved(item: &RegisterItem) -> String {
    let mut pro = Vec::new();
    let mut con = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for a in item.record.assertions.iter().rev() {
        if !seen.insert(a.source.clone()) {
            continue;
        }
        let who = positor_label(&a.source).to_string();
        if a.certainty > 0.0 {
            pro.push(who);
        } else if a.certainty < 0.0 {
            con.push(who);
        }
    }
    let mut parts = Vec::new();
    if !pro.is_empty() {
        parts.push(format!("proposed by {}", pro.join(", ")));
    }
    if !con.is_empty() {
        parts.push(format!("disputed by {}", con.join(", ")));
    }
    if !item.conflicting_statements.is_empty() {
        parts.push(format!("conflicting wording: \"{}\"", item.conflicting_statements.join("\" / \"")));
    }
    if item.status == Status::Uncertain && con.is_empty() {
        parts.push("not confirmed by the developer".into());
    }
    format!(" ({})", parts.join("; "))
}

/// Render the packet, or `None` when the strategy is `None` or the register is empty.
pub fn render(input: &PacketInput) -> Option<Packet> {
    if input.strategy == PacketStrategy::None || input.items.is_empty() {
        return None;
    }
    let mut header = format!("<handover-register epoch=\"{}\"", input.epoch_key);
    if let Some(m) = input.from_model {
        header.push_str(&format!(" from=\"model:{m}\""));
    }
    if let Some(t) = input.complete_through {
        header.push_str(&format!(" register-complete-through=\"turn {t}\""));
    }
    header.push('>');

    let mut out = vec![header];
    let mut lines: Vec<(Option<&'static str>, Line)> = Vec::new();
    match input.strategy {
        PacketStrategy::StatusAwareCore => status_aware(input.items, &mut lines),
        PacketStrategy::SurfaceRich => surface_rich(input.items, &mut lines),
        PacketStrategy::Chronological => chronological(input.items, &mut lines),
        PacketStrategy::None => unreachable!(),
    }

    let intro = [
        "You are continuing an existing session. These items govern it. Earlier".to_string(),
        "turns may be missing from your context; this register is authoritative.".to_string(),
    ];
    let mut footer = vec![
        "ATTRIBUTION".to_string(),
        "  Work listed above was performed by the previous model. Do not claim to".to_string(),
        "  remember running tools that you cannot see in this context.".to_string(),
    ];
    if let Some((from, to)) = input.incomplete {
        footer.push(format!("  The register may be incomplete for turns {from}..{to}; ask before relying on them."));
    }
    footer.push("</handover-register>".to_string());

    // Budget: drop droppable lines from the end until the text fits.
    let fixed: usize = out.iter().chain(intro.iter()).chain(footer.iter()).map(|l| l.len() + 1).sum();
    let mut keep: Vec<bool> = vec![true; lines.len()];
    let size = |keep: &[bool], lines: &[(Option<&'static str>, Line)]| -> usize {
        let mut sections = std::collections::BTreeSet::new();
        let mut n = 0;
        for (k, (sec, l)) in keep.iter().zip(lines) {
            if *k {
                n += l.text.len() + 1;
                if let Some(s) = sec
                    && sections.insert(*s)
                {
                    n += s.len() + 1;
                }
            }
        }
        n
    };
    // Complementary pairs (`requires`) are kept or dropped together: a pair
    // with any undroppable member is undroppable as a whole.
    let pairs: Vec<(String, String)> =
        input.items.iter().flat_map(|i| i.record.requires.iter().map(move |r| (i.key().to_string(), r.clone()))).collect();
    let partners = |key: &str| -> Vec<String> {
        let mut group = vec![key.to_string()];
        let mut changed = true;
        while changed {
            changed = false;
            for (a, b) in &pairs {
                for (x, y) in [(a, b), (b, a)] {
                    if group.contains(x) && !group.contains(y) {
                        group.push(y.clone());
                        changed = true;
                    }
                }
            }
        }
        group
    };
    for idx in 0..lines.len() {
        if lines[idx].1.droppable {
            let group = partners(&lines[idx].1.key);
            if lines.iter().any(|(_, l)| !l.droppable && group.contains(&l.key)) {
                lines[idx].1.droppable = false;
            }
        }
    }
    let mut i = lines.len();
    while fixed + size(&keep, &lines) > input.max_chars && i > 0 {
        i -= 1;
        if lines[i].1.droppable && keep[i] {
            let group = partners(&lines[i].1.key);
            for (k, (_, l)) in keep.iter_mut().zip(&lines) {
                if group.contains(&l.key) {
                    *k = false;
                }
            }
        }
    }

    out.extend(intro);
    let mut citations = Vec::new();
    let mut current: Option<&'static str> = None;
    for (k, (sec, line)) in keep.iter().zip(&lines) {
        if !k {
            continue;
        }
        if *sec != current {
            if let Some(s) = sec {
                out.push((*s).to_string());
            }
            current = *sec;
        }
        out.push(line.text.clone());
        citations.push((line.key.clone(), line.label.to_string()));
    }
    out.extend(footer);
    Some(Packet { text: out.join("\n") + "\n", citations })
}

const HARD: &str = "HARD — in force (do not violate; ask before any action near them)";
const ENDED: &str = "REVOKED / SUPERSEDED — do not resume";
const UNRESOLVED: &str = "UNRESOLVED — confirm with the developer before relying on these";
const GUIDANCE: &str = "GUIDANCE — in force, lower priority";
const GOALS: &str = "COMMITMENTS AND GOALS";

fn status_aware(items: &[RegisterItem], lines: &mut Vec<(Option<&'static str>, Line)>) {
    let line =
        |i: &RegisterItem, text: String, label: &'static str, droppable: bool| Line { text, key: i.key().to_string(), label, droppable };
    for i in items.iter().filter(|i| i.is_hard() && i.status == Status::Active) {
        lines.push((Some(HARD), line(i, format!("  [{}] {}{}", i.handle(), i.statement, provenance(i)), "hard-active", false)));
    }
    for i in items.iter().filter(|i| matches!(i.status, Status::Revoked | Status::Superseded)) {
        let (what, label) = match (i.status, &i.superseded_by) {
            (Status::Superseded, Some(by)) => {
                let by_handle = items.iter().find(|x| x.key() == by).map(|x| x.handle()).unwrap_or_else(|| by.clone());
                (format!("superseded by {by_handle}"), "superseded")
            }
            _ => (revocation(i), "revoked"),
        };
        // Revoked hard items are what the stale-summary failure is about: never drop them.
        lines.push((Some(ENDED), line(i, format!("  [{}] ~~{}~~ {what}", i.handle(), i.statement), label, !i.is_hard())));
    }
    for i in items.iter().filter(|i| matches!(i.status, Status::Disputed | Status::Uncertain)) {
        lines.push((Some(UNRESOLVED), line(i, format!("  [{}] {}{}", i.handle(), i.statement, unresolved(i)), "unresolved", !i.is_hard())));
    }
    for i in items.iter().filter(|i| {
        !i.is_hard() && i.status == Status::Active && matches!(i.kind(), Kind::Permission | Kind::Prohibition | Kind::RoleBoundary)
    }) {
        lines.push((Some(GUIDANCE), line(i, format!("  [{}] {}", i.handle(), i.statement), "active", true)));
    }
    for i in items.iter().filter(|i| !i.is_hard() && i.status == Status::Active && matches!(i.kind(), Kind::Commitment | Kind::Goal)) {
        lines.push((Some(GOALS), line(i, format!("  [{}] {}", i.handle(), i.statement), "active", true)));
    }
}

fn label_for(i: &RegisterItem) -> &'static str {
    match i.status {
        Status::Active if i.is_hard() => "hard-active",
        Status::Active => "active",
        Status::Revoked => "revoked",
        Status::Superseded => "superseded",
        Status::Disputed | Status::Uncertain => "unresolved",
    }
}

fn surface_rich(items: &[RegisterItem], lines: &mut Vec<(Option<&'static str>, Line)>) {
    let mut all: Vec<(&str, &RegisterItem, &str)> = Vec::new();
    for i in items {
        for (text, time) in &i.record.statements {
            all.push((time.as_str(), i, text.as_str()));
        }
    }
    all.sort_by(|a, b| a.0.cmp(b.0).then(a.1.record.seq.cmp(&b.1.record.seq)));
    for (_, i, text) in all {
        lines.push((
            Some("SESSION NOTES"),
            // Citations record how the packet presents an item: here as a plain
            // note, whatever its status (so audit C2 sees the stale summary).
            Line { text: format!("  - {text}"), key: i.key().to_string(), label: "noted", droppable: !i.is_hard() },
        ));
    }
}

fn chronological(items: &[RegisterItem], lines: &mut Vec<(Option<&'static str>, Line)>) {
    let mut sorted: Vec<&RegisterItem> = items.iter().collect();
    sorted.sort_by(|a, b| a.record.introduced.cmp(&b.record.introduced).then(a.record.seq.cmp(&b.record.seq)));
    for i in sorted {
        lines.push((
            Some("ITEMS IN ORDER"),
            Line {
                text: format!(
                    "  [{}] {} ({}, {}, {})",
                    i.handle(),
                    i.statement,
                    i.kind().as_str(),
                    i.strength().as_str(),
                    i.status.as_str()
                ),
                key: i.key().to_string(),
                label: label_for(i),
                droppable: !i.is_hard(),
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::item::{Assertion, Authority, Citation, ItemRecord, Strength};
    use crate::status::{StatusPolicy, derive};

    fn rec(
        seq: u64,
        kind: Kind,
        strength: Strength,
        text: &str,
        positions: &[(&str, f64, &str)],
        cite: Option<(u64, &str, &str)>,
    ) -> ItemRecord {
        ItemRecord {
            key: format!("s#{seq}"),
            seq,
            kind,
            strength,
            statements: vec![(text.into(), "2026-11-20 10:00:00".into())],
            enforce: None,
            citations: cite
                .map(|(t, q, op)| vec![Citation { time: "2026-11-20 10:00:00".into(), value: json!({"turn": t, "quote": q, "op": op}) }])
                .unwrap_or_default(),
            assertions: positions
                .iter()
                .map(|(s, c, t)| Assertion {
                    source: s.to_string(),
                    authority: if s.starts_with("developer") { Authority::Authoritative } else { Authority::Advisory },
                    certainty: *c,
                    time: format!("2026-11-20 {t}"),
                    meta_certainty: Some(0.95),
                })
                .collect(),
            replaced_by: vec![],
            requires: vec![],
            introduced: "2026-11-20 10:00:00".into(),
        }
    }

    fn items() -> Vec<RegisterItem> {
        let mut c3 = rec(3, Kind::Commitment, Strength::Graded, "Rewrite retry logic as a queue", &[("developer:42", 1.0, "10:05")], None);
        c3.replaced_by = vec!["s#5".into()];
        let records = vec![
            rec(
                1,
                Kind::Prohibition,
                Strength::Hard,
                "Do not trigger a production deployment.",
                &[("developer:42", 1.0, "10:00")],
                Some((12, "no prod deploys from here", "add")),
            ),
            rec(
                2,
                Kind::Permission,
                Strength::Hard,
                "May run database migrations",
                &[("developer:42", 1.0, "10:01"), ("developer:42", 0.0, "10:30")],
                Some((30, "no more migrations", "revoke")),
            ),
            c3,
            rec(4, Kind::Goal, Strength::Graded, "Use asyncpg", &[("model:qwen", 0.8, "10:40"), ("tool:pytest", -0.9, "10:41")], None),
            rec(
                5,
                Kind::Commitment,
                Strength::Graded,
                "Fix duplicate retries within the existing retry module.",
                &[("developer:42", 1.0, "10:45")],
                None,
            ),
        ];
        derive(&records, &StatusPolicy::default())
    }

    #[test]
    fn status_aware_core_matches_the_plan_layout() {
        let items = items();
        let p = render(&PacketInput {
            epoch_key: "e-1832",
            from_model: Some("qwen3-coder@2026-10"),
            complete_through: Some(41),
            incomplete: None,
            items: &items,
            strategy: PacketStrategy::StatusAwareCore,
            max_chars: 10_000,
        })
        .unwrap();
        let expected = "\
<handover-register epoch=\"e-1832\" from=\"model:qwen3-coder@2026-10\" register-complete-through=\"turn 41\">
You are continuing an existing session. These items govern it. Earlier
turns may be missing from your context; this register is authoritative.
HARD — in force (do not violate; ask before any action near them)
  [P1] Do not trigger a production deployment. (developer, turn 12: \"no prod deploys from here\")
REVOKED / SUPERSEDED — do not resume
  [P2] ~~May run database migrations~~ revoked by developer, turn 30
  [C3] ~~Rewrite retry logic as a queue~~ superseded by C5
UNRESOLVED — confirm with the developer before relying on these
  [G4] Use asyncpg (proposed by model; disputed by tool)
COMMITMENTS AND GOALS
  [C5] Fix duplicate retries within the existing retry module.
ATTRIBUTION
  Work listed above was performed by the previous model. Do not claim to
  remember running tools that you cannot see in this context.
</handover-register>
";
        assert_eq!(p.text, expected);
        assert_eq!(
            p.citations,
            vec![
                ("s#1".to_string(), "hard-active".to_string()),
                ("s#2".to_string(), "revoked".to_string()),
                ("s#3".to_string(), "superseded".to_string()),
                ("s#4".to_string(), "unresolved".to_string()),
                ("s#5".to_string(), "active".to_string()),
            ]
        );
    }

    #[test]
    fn budget_drops_graded_lines_never_hard_ones() {
        let items = items();
        let p = render(&PacketInput {
            epoch_key: "e",
            from_model: None,
            complete_through: None,
            incomplete: Some((3, 9)),
            items: &items,
            strategy: PacketStrategy::StatusAwareCore,
            max_chars: 100,
        })
        .unwrap();
        assert!(p.text.contains("[P1]"));
        assert!(p.text.contains("[P2]"), "revoked hard items are kept");
        assert!(!p.text.contains("[C5]"));
        assert!(p.text.contains("incomplete for turns 3..9"));
        assert!(p.citations.iter().all(|(k, _)| k != "s#5"));
    }

    #[test]
    fn other_strategies() {
        let items = items();
        let base = |strategy| PacketInput {
            epoch_key: "e",
            from_model: None,
            complete_through: None,
            incomplete: None,
            items: &items,
            strategy,
            max_chars: 10_000,
        };
        assert!(render(&base(PacketStrategy::None)).is_none());
        let surface = render(&base(PacketStrategy::SurfaceRich)).unwrap();
        assert!(surface.text.contains("  - May run database migrations"));
        assert!(!surface.text.contains("revoked"));
        let chrono = render(&base(PacketStrategy::Chronological)).unwrap();
        assert!(chrono.text.contains("[P2] May run database migrations (permission, hard, revoked)"));
        // Deterministic.
        assert_eq!(render(&base(PacketStrategy::StatusAwareCore)), render(&base(PacketStrategy::StatusAwareCore)));
    }

    #[test]
    fn complementary_pairs_are_kept_or_dropped_together() {
        let mut items = items();
        // C5 (graded) requires P1 (hard, never dropped): C5 must survive any budget.
        items[4].record.requires = vec!["s#1".into()];
        let p = render(&PacketInput {
            epoch_key: "e",
            from_model: None,
            complete_through: None,
            incomplete: None,
            items: &items,
            strategy: PacketStrategy::StatusAwareCore,
            max_chars: 100,
        })
        .unwrap();
        assert!(p.text.contains("[C5]"), "{}", p.text);
        // G4 requires C5, both graded: dropping one drops the other.
        let mut items = self::items();
        items[3].record.requires = vec!["s#5".into()];
        let full = render(&PacketInput {
            epoch_key: "e",
            from_model: None,
            complete_through: None,
            incomplete: None,
            items: &items,
            strategy: PacketStrategy::StatusAwareCore,
            max_chars: 10_000,
        })
        .unwrap();
        let budget = full.text.len() - 10; // forces dropping the last graded line (C5)
        let p = render(&PacketInput {
            epoch_key: "e",
            from_model: None,
            complete_through: None,
            incomplete: None,
            items: &items,
            strategy: PacketStrategy::StatusAwareCore,
            max_chars: budget,
        })
        .unwrap();
        assert!(!p.text.contains("[C5]") && !p.text.contains("[G4]"), "{}", p.text);
    }
}
