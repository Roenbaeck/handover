//! Q2: reading the register and deriving status.
//!
//! Scenario modelled on the paper's coding case (all times 2026-11-20):
//! - P  permission "May trigger the production deploy workflow": developer +100% @10:00,
//!      developer 0% @10:30 (revoked).
//! - C  commitment "Rewrite retry logic as a queue": model +80% @10:10, developer -100% @10:20.
//! - G  commitment "Use asyncpg": model +80% @10:40, tool:pytest -90% @10:41 (disputed).
//! - H  prohibition "Do not modify migrations/": developer +100% @10:50, statement reworded @10:55.
//! - W  goal with two equal-time conflicting statements @11:00 (disputed wording).

use std::collections::BTreeMap;

use positorium::{CollectedResultSet, Database, ExecutionOptions};
use positorium_spike::{ROLES, run, thing, time};

fn scenario() -> (Database, String) {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let setup = r#"
        add posit [{(+dev, name)}, "developer:42", '2026-11-20'], [{(dev, authority)}, "authoritative", '2026-11-20'],
                  [{(+model, name)}, "model:qwen3-coder@2026-10", '2026-11-20'], [{(model, authority)}, "advisory", '2026-11-20'],
                  [{(+pytest, name)}, "tool:pytest", '2026-11-20'], [{(pytest, authority)}, "advisory", '2026-11-20'],
                  [{(+s, session_key)}, "sess-1", '2026-11-20'];

        add posit +mp [{(+p, item), (s, session)}, "registered", '2026-11-20 10:00:00']
                    and assert +ap by dev with 100% at '2026-11-20 10:00:00',
                  [{(p, item_key)}, "P", '2026-11-20 10:00:00'],
                  [{(p, kind)}, "permission", '2026-11-20 10:00:00'],
                  [{(p, strength)}, "hard", '2026-11-20 10:00:00'],
                  [{(p, statement)}, "May trigger the production deploy workflow", '2026-11-20 10:00:00'];
        add posit [{(mp, posit), (dev, ascertains)}, 0%, '2026-11-20 10:30:00'];

        add posit +mc [{(+c, item), (s, session)}, "registered", '2026-11-20 10:10:00']
                    and assert by model with 80% at '2026-11-20 10:10:00',
                  [{(c, item_key)}, "C", '2026-11-20 10:10:00'],
                  [{(c, kind)}, "commitment", '2026-11-20 10:10:00'],
                  [{(c, strength)}, "graded", '2026-11-20 10:10:00'],
                  [{(c, statement)}, "Rewrite retry logic as a queue", '2026-11-20 10:10:00'];
        add posit [{(mc, posit), (dev, ascertains)}, -100%, '2026-11-20 10:20:00'];

        add posit +mg [{(+g, item), (s, session)}, "registered", '2026-11-20 10:40:00']
                    and assert by model with 80% at '2026-11-20 10:40:00',
                  [{(g, item_key)}, "G", '2026-11-20 10:40:00'],
                  [{(g, kind)}, "commitment", '2026-11-20 10:40:00'],
                  [{(g, strength)}, "graded", '2026-11-20 10:40:00'],
                  [{(g, statement)}, "Use asyncpg", '2026-11-20 10:40:00'];
        add posit [{(mg, posit), (pytest, ascertains)}, -90%, '2026-11-20 10:41:00'];

        add posit +mh [{(+h, item), (s, session)}, "registered", '2026-11-20 10:50:00']
                    and assert by dev with 100% at '2026-11-20 10:50:00',
                  [{(h, item_key)}, "H", '2026-11-20 10:50:00'],
                  [{(h, kind)}, "prohibition", '2026-11-20 10:50:00'],
                  [{(h, strength)}, "hard", '2026-11-20 10:50:00'],
                  [{(h, statement)}, "Do not modify migrations/", '2026-11-20 10:50:00'];
        add posit [{(h, statement)}, "Do not modify anything under migrations/ or schema/", '2026-11-20 10:55:00'];

        add posit +mw [{(+w, item), (s, session)}, "registered", '2026-11-20 11:00:00']
                    and assert by dev with 100% at '2026-11-20 11:00:00',
                  [{(w, item_key)}, "W", '2026-11-20 11:00:00'],
                  [{(w, kind)}, "goal", '2026-11-20 11:00:00'],
                  [{(w, strength)}, "graded", '2026-11-20 11:00:00'],
                  [{(w, statement)}, "Acceptance: test_payment_retry passes", '2026-11-20 11:00:00'],
                  [{(w, statement)}, "Acceptance: the whole integration suite passes", '2026-11-20 11:00:00'];

        search [{(?s, session_key)}, "sess-1", *] return ?s;
    "#;
    let sets = db.execute(setup).unwrap();
    let s = sets.last().unwrap().cell(0, "s").unwrap().text.clone();
    (db, s)
}

/// key -> sorted list of "source:certainty"
fn by_key(set: &CollectedResultSet, extra: &[&str]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in 0..set.rows.len() {
        let key = set.cell(r, "key").unwrap().text.trim_matches('"').to_string();
        let parts: Vec<String> = extra
            .iter()
            .map(|c| set.cell(r, c).unwrap().text.trim_matches('"').to_string())
            .collect();
        out.entry(key).or_default().push(parts.join(":"));
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

const ASSERTION_SLOT_AS_OF: &str = r#"
    search ?m = [{(?i, item), (?s, session)}, "registered", *],
           [{(?i, item_key)}, ?key, *],
           [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t,
           [{(?src, name)}, ?src_name, *]
    where ?s = $session
    return ?key, ?src_name, ?c, ?asserted;
"#;

#[test]
fn as_of_on_assertion_slots_keeps_latest_position_including_withdrawals() {
    let (db, s) = scenario();
    let opts = |t: &str| ExecutionOptions::default().bind("t", time(t)).bind("session", thing(&db, &s));

    let now = run(&db, "assertion slots as of 12:00", ASSERTION_SLOT_AS_OF, opts("2026-11-20 12:00:00")).unwrap();
    let now = by_key(now.last().unwrap(), &["src_name", "c"]);
    assert_eq!(now["P"], vec!["developer:42:0%"], "revocation visible as latest 0%");
    assert_eq!(now["C"], vec!["developer:42:-100%", "model:qwen3-coder@2026-10:80%"]);
    assert_eq!(now["G"], vec!["model:qwen3-coder@2026-10:80%", "tool:pytest:-90%"]);
    assert_eq!(now["H"], vec!["developer:42:100%"]);

    let before = run(&db, "assertion slots as of 10:15", ASSERTION_SLOT_AS_OF, opts("2026-11-20 10:15:00")).unwrap();
    let before = by_key(before.last().unwrap(), &["src_name", "c"]);
    assert_eq!(before["P"], vec!["developer:42:100%"], "P was still granted at 10:15");
    assert_eq!(before["C"], vec!["model:qwen3-coder@2026-10:80%"]);
    assert!(!before.contains_key("H"));
}

#[test]
fn in_effect_drops_withdrawals_but_keeps_negative_positions() {
    let (db, s) = scenario();
    let script = r#"
        search ?m = [{(?i, item), (?s, session)}, "registered", *] in effect $t, $t
                 via [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
               [{(?i, item_key)}, ?key, *],
               [{(?src, name)}, ?src_name, *]
        where ?s = $session
        return ?key, ?src_name, ?c;
    "#;
    let sets = run(
        &db,
        "in effect 12:00",
        script,
        ExecutionOptions::default().bind("t", time("2026-11-20 12:00:00")).bind("session", thing(&db, &s)),
    )
    .unwrap();
    let got = by_key(sets.last().unwrap(), &["src_name", "c"]);
    assert!(!got.contains_key("P"), "a 0% withdrawal removes P from in-effect results: {got:?}");
    assert_eq!(got["C"], vec!["developer:42:-100%", "model:qwen3-coder@2026-10:80%"]);
}

#[test]
fn register_read_returns_latest_wording_and_equal_time_conflicts() {
    let (db, s) = scenario();
    let script = r#"
        search ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
               [{(?i, item_key)}, ?key, *],
               [{(?i, kind)}, ?kind, *] as of $t,
               [{(?i, strength)}, ?strength, *] as of $t,
               [{(?i, statement)}, ?text, ?worded] as of $t
        where ?s = $session
        return ?key, ?kind, ?strength, ?text, ?worded, ?introduced;
    "#;
    let sets = run(
        &db,
        "register read",
        script,
        ExecutionOptions::default().bind("t", time("2026-11-20 12:00:00")).bind("session", thing(&db, &s)),
    )
    .unwrap();
    let got = by_key(sets.last().unwrap(), &["text"]);
    assert_eq!(got["H"], vec!["Do not modify anything under migrations/ or schema/"]);
    assert_eq!(got["W"].len(), 2, "equal-time conflicting statements both survive");
}

#[test]
fn meta_assertion_joins_through_the_assertion_posit() {
    let (db, s) = scenario();
    // Extractor ascertains the developer's assertion on H with 85%.
    db.execute(
        r#"
        search [{(?ext, name)}, "extractor:qwen", *] or add posit [{(+ext, name)}, "extractor:qwen", @NOW];
        search [{(?dev, name)}, "developer:42", *] or add posit [{(+dev, name)}, "developer:42", @NOW];
        search [{(?h, item_key)}, "H", *] or add posit [{(+h, item_key)}, "H", @NOW];
        search [{(?s, session_key)}, "sess-1", *] or add posit [{(+s, session_key)}, "sess-1", @NOW];
        add posit +mh [{(h, item), (s, session)}, "registered", '2026-11-20 10:50:00'];
        add posit +a [{(mh, posit), (dev, ascertains)}, 100%, '2026-11-20 10:50:00'];
        add posit [{(a, posit), (ext, ascertains)}, 85%, '2026-11-20 10:50:07'];
        "#,
    )
    .unwrap();
    let script = r#"
        search ?m = [{(?i, item), (?s, session)}, "registered", *],
               [{(?i, item_key)}, ?key, *],
               ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t,
               [{(?a, posit), (?meta, ascertains)}, ?mc, ?learned] as of $t,
               [{(?meta, name)}, ?meta_name, *]
        where ?s = $session
        return ?key, ?c, ?meta_name, ?mc, ?learned;
    "#;
    // Known by 10:50:07, but not at 10:50:05 (bitemporal: what could the proxy know?).
    for (t, expect) in [("2026-11-20 10:50:05", 0), ("2026-11-20 12:00:00", 1)] {
        let sets = run(
            &db,
            &format!("meta at {t}"),
            script,
            ExecutionOptions::default().bind("t", time(t)).bind("session", thing(&db, &s)),
        )
        .unwrap();
        assert_eq!(sets.last().unwrap().rows.len(), expect, "at {t}");
    }
}

#[test]
fn as_of_accepts_a_variable_bound_by_another_pattern() {
    let (db, s) = scenario();
    db.execute(
        r#"
        search [{(?s, session_key)}, "sess-1", *] or add posit [{(+s, session_key)}, "sess-1", @NOW];
        add posit [{(+e, epoch), (s, session)}, "model_change", '2026-11-20 10:15:00'],
                  [{(+e2, epoch), (s, session)}, "model_change", '2026-11-20 12:00:00'];
        "#,
    )
    .unwrap();
    let script = r#"
        search [{(?e, epoch), (?s, session)}, *, ?at],
               ?m = [{(?i, item), (?s, session)}, "registered", *],
               [{(?i, item_key)}, ?key, *],
               [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
               [{(?src, authority)}, "authoritative", *]
        where ?s = $session and ?c > 0%
        return ?at, ?key, ?c;
    "#;
    let sets = run(&db, "as of ?at", script, ExecutionOptions::default().bind("session", thing(&db, &s))).unwrap();
    let set = sets.last().unwrap();
    let rows: Vec<(String, String)> = (0..set.rows.len())
        .map(|r| (set.cell(r, "at").unwrap().text.clone(), set.cell(r, "key").unwrap().text.clone()))
        .collect();
    println!("{rows:?}");
    assert!(rows.contains(&("2026-11-20 10:15:00".into(), "\"P\"".into())), "P active at the 10:15 epoch");
    assert!(!rows.contains(&("2026-11-20 12:00:00".into(), "\"P\"".into())), "P revoked by the 12:00 epoch");
    assert!(rows.contains(&("2026-11-20 12:00:00".into(), "\"H\"".into())));
}
