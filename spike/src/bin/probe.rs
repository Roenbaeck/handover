//! Probe query variants against an existing benchmark store.
//!
//! Usage: cargo run --release --bin probe -- <store-dir>

use std::time::{Duration, Instant};

use positorium::{Database, Engine, ExecutionOptions};
use positorium_spike::time;

const VARIANTS: &[(&str, &str)] = &[
    (
        "A keyed, as of on every slot (PLAN sketch)",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *],
           [{(?i, kind)}, ?kind, *] as of $t,
           [{(?i, strength)}, ?strength, *] as of $t,
           [{(?i, statement)}, ?text, *] as of $t,
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t,
           [{(?src, authority)}, ?authority, *]
           return ?ikey, ?kind, ?strength, ?text, ?authority, ?c, ?asserted;"#,
    ),
    (
        "B keyed, no as of (raw history, reduce in Rust)",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *],
           [{(?i, kind)}, ?kind, ?kt],
           [{(?i, strength)}, ?strength, ?st],
           [{(?i, statement)}, ?text, ?tt],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
           [{(?src, authority)}, ?authority, *]
           return ?ikey, ?kind, ?kt, ?strength, ?st, ?text, ?tt, ?authority, ?c, ?asserted;"#,
    ),
    (
        "C keyed, as of only on the assertion slot",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *],
           [{(?i, kind)}, ?kind, *],
           [{(?i, strength)}, ?strength, *],
           [{(?i, statement)}, ?text, ?tt],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t,
           [{(?src, authority)}, ?authority, *]
           return ?ikey, ?kind, ?strength, ?text, ?tt, ?authority, ?c, ?asserted;"#,
    ),
    (
        "D items only (membership by session key)",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced]
           return ?i, ?m;"#,
    ),
    (
        "E session key lookup only",
        r#"search [{(?s, session_key)}, $key, *] return ?s;"#,
    ),
    (
        "F get-or-create turn 5 of a session (hit)",
        r#"search [{(?s, session_key)}, $key, *], [{(?t, turn), (?s, session)}, 5, *] return ?t;"#,
    ),
    (
        "G D + item_key",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *]
           return ?i, ?ikey;"#,
    ),
    (
        "H D + statement",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, statement)}, ?text, *]
           return ?i, ?text;"#,
    ),
    (
        "I D + assertion slot",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted]
           return ?i, ?src, ?c;"#,
    ),
    (
        "J D + assertion slot + authority",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
           [{(?src, authority)}, ?authority, *]
           return ?i, ?src, ?c, ?authority;"#,
    ),
    (
        "K D + item_key + kind + strength + statement",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *],
           [{(?i, kind)}, ?kind, *],
           [{(?i, strength)}, ?strength, *],
           [{(?i, statement)}, ?text, *]
           return ?ikey, ?kind, ?strength, ?text;"#,
    ),
    (
        "L D + assertion slot + session owner as authority",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
           [{(?s, session), (?src, developer)}, "owner", *]
           return ?i, ?src, ?c;"#,
    ),
    (
        "M full read without authority (resolve in Rust)",
        r#"search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *],
           [{(?i, kind)}, ?kind, *] as of $t,
           [{(?i, strength)}, ?strength, *] as of $t,
           [{(?i, statement)}, ?text, *] as of $t,
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t
           return ?ikey, ?kind, ?strength, ?text, ?src, ?c, ?asserted;"#,
    ),
];

fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let t = Instant::now();
    let db = Database::open(&dir).unwrap();
    println!("open: {:.2?}", t.elapsed());
    let explain_only: Option<String> = std::env::args().nth(2);
    for (label, script) in VARIANTS {
        let mut lat: Vec<Duration> = Vec::new();
        let mut rows = 0;
        for s in [0usize, 17, 42, 99, 140] {
            let opts = ExecutionOptions::default().bind("key", format!("sess-{s}")).bind("t", time("2028-01-01"));
            let t = Instant::now();
            let set = db.execute_one_with_options(script, opts).unwrap();
            lat.push(t.elapsed());
            rows = set.rows.len();
        }
        lat.sort();
        println!("{label:<52} median {:>9.2?}  rows {rows}", lat[2]);
        if explain_only.as_deref().is_some_and(|p| label.starts_with(p)) {
            let plan = Engine::new(&db)
                .execute_collect_multi_with_options(
                    &format!("explain {script}"),
                    ExecutionOptions::default().bind("key", "sess-0").bind("t", time("2028-01-01")),
                )
                .unwrap();
            for set in plan {
                for row in set.rows {
                    println!("    {}", row.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" | "));
                }
            }
        }
    }
}
