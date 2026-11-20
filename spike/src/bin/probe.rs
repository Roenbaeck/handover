//! Probe query variants against an existing benchmark store.
//!
//! Usage: cargo run --release --bin probe -- <store-dir> [variant-letter-to-explain]

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
    (
        "N ($s, session) membership only",
        r#"search ?m = [{(?i, item), ($s, session)}, "registered", ?introduced] return ?i, ?m;"#,
    ),
    (
        "O ($s, session) membership + assertion slot",
        r#"search ?m = [{(?i, item), ($s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted]
           return ?i, ?src, ?c;"#,
    ),
    (
        "P ($s, session) membership + assertion slot + authority",
        r#"search ?m = [{(?i, item), ($s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
           [{(?src, authority)}, ?authority, *]
           return ?i, ?src, ?c, ?authority;"#,
    ),
    (
        "Q where ?s = $s, membership + assertion slot + authority",
        r#"search ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
           [{(?src, authority)}, ?authority, *]
           where ?s = $s
           return ?i, ?src, ?c, ?authority;"#,
    ),
    (
        "R C1 PLAN body as a search (where ?s = $s, as of ?at, not exists)",
        r#"search ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?src, authority)}, "authoritative", *]
           not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
           where ?s = $s and ?introduced < ?at and ?c > 0%
           return ?e, ?i, ?ep, ?m;"#,
    ),
    (
        "S R without the authority join",
        r#"search ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at
           not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
           where ?s = $s and ?introduced < ?at and ?c > 0%
           return ?e, ?i, ?ep, ?m;"#,
    ),
    (
        "T C1 body with ($s, session), owner authority",
        r#"search ?ep = [{(?e, epoch), ($s, session)}, *, ?at],
           ?m = [{(?i, item), ($s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{($s, session), (?src, developer)}, "owner", *]
           not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
           where ?introduced < ?at and ?c > 0%
           return ?e, ?i, ?ep, ?m;"#,
    ),
    (
        "U C1 body keyed on session_key (revised)",
        r#"search [{(?s, session_key)}, $key, *],
           ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?s, session), (?src, developer)}, "owner", *]
           not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
           where ?introduced < ?at and ?c > 0%
           return ?e, ?i, ?ep, ?m;"#,
    ),
    (
        "V Q plus a second where conjunct (?c > 0%)",
        r#"search ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
           [{(?src, authority)}, ?authority, *]
           where ?s = $s and ?c > 0%
           return ?i, ?src, ?c, ?authority;"#,
    ),
    (
        "W R with only where ?s = $s (other predicates dropped)",
        r#"search ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?src, authority)}, "authoritative", *]
           not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
           where ?s = $s
           return ?e, ?i, ?ep, ?m;"#,
    ),
    (
        "X Q with ?s in two patterns (epoch + membership)",
        r#"search ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted],
           [{(?src, authority)}, ?authority, *]
           where ?s = $s
           return ?e, ?i, ?src, ?c, ?authority;"#,
    ),
];

fn session_thing(db: &Database, key: &str) -> positorium::ExecutionParameter {
    let set = db
        .execute_one_with_options(
            "search [{(?s, session_key)}, $key, *] return ?s;",
            ExecutionOptions::default().bind("key", key),
        )
        .unwrap();
    positorium_spike::thing(db, &set.cell(0, "s").unwrap().text)
}

fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let t = Instant::now();
    let db = Database::open(&dir).unwrap();
    println!("open: {:.2?}", t.elapsed());
    let explain_only: Option<String> = std::env::args().nth(2);
    let large = db.execute_one("search ?p = [*, *, *] return ?p;").unwrap().rows.len() > 200_000;
    for (label, script) in VARIANTS {
        if large && label.starts_with('R') {
            println!("{label:<52} skipped (about 8 s at 100k posits, grows faster than linearly)");
            continue;
        }
        let mut lat: Vec<Duration> = Vec::new();
        let mut rows = 0;
        for s in [0usize, 17, 42, 99, 140] {
            let key = format!("sess-{s}");
            let opts = ExecutionOptions::default()
                .bind("s", session_thing(&db, &key))
                .bind("key", key)
                .bind("t", time("2028-01-01"));
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
                    ExecutionOptions::default()
                        .bind("s", session_thing(&db, "sess-0"))
                        .bind("key", "sess-0")
                        .bind("t", time("2028-01-01")),
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
