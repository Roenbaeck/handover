//! Q4: performance of the register on a persistent store.
//!
//! Usage:
//!   bench <store-dir> <sessions> [turns-per-session]   generate a store and measure
//!   bench --reopen <store-dir>                          reopen in a fresh process
//!
//! Each synthetic session has `turns` turns (turn anchor + digest), an item every
//! 10 turns (membership + developer assertion + 5 facts + extractor meta-assertion),
//! an enforced action every 20 turns (3 posits) and 2 epochs with a ~1 KB packet and
//! a citation. About 3 posits per turn.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use positorium::{Database, ExecutionOptions, ExecutionParameter};
use positorium_spike::{ROLES, thing, time};

/// PLAN.md §11 sketch: identity filter in `where`, authority as a positor posit.
const C1_PLAN: &str = r#"add constraint "C1 plan sketch" {
    search ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?src, authority)}, "authoritative", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
    where ?s = $session and ?introduced < ?at and ?c > 0%
    return ?e, ?i, ?ep, ?m;
};"#;

/// Revised: anchored on the session key literal, authority = session owner.
const C1_REVISED: &str = r#"add constraint "C1 revised" {
    search [{(?s, session_key)}, $session_key, *],
           ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?s, session), (?src, developer)}, "owner", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
    where ?introduced < ?at and ?c > 0%
    return ?e, ?i, ?ep, ?m;
};"#;

/// PLAN.md §8.3 sketch: identity filter in `where`, authority joined.
const READ_PLAN: &str = r#"
    search ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?key, *],
           [{(?i, kind)}, ?kind, *] as of $t,
           [{(?i, strength)}, ?strength, *] as of $t,
           [{(?i, statement)}, ?text, *] as of $t,
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t,
           [{(?src, authority)}, ?authority, *]
    where ?s = $session
    return ?key, ?kind, ?strength, ?text, ?authority, ?c, ?asserted;
"#;

/// Revised: anchored on the session key literal; authority resolved in Rust.
const READ_REVISED: &str = r#"
    search [{(?s, session_key)}, $key, *],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *],
           [{(?i, kind)}, ?kind, *] as of $t,
           [{(?i, strength)}, ?strength, *] as of $t,
           [{(?i, statement)}, ?text, *] as of $t,
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t
    return ?ikey, ?kind, ?strength, ?text, ?src, ?c, ?asserted;
"#;

/// 0.3.2-beta.7: the session Thing goes straight into the appearance, authority joined.
const READ_PARAM: &str = r#"
    search ?m = [{(?i, item), ($s, session)}, "registered", ?introduced],
           [{(?i, item_key)}, ?ikey, *],
           [{(?i, kind)}, ?kind, *] as of $t,
           [{(?i, strength)}, ?strength, *] as of $t,
           [{(?i, statement)}, ?text, *] as of $t,
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?asserted] as of $t,
           [{(?src, authority)}, ?authority, *]
    return ?ikey, ?kind, ?strength, ?text, ?authority, ?c, ?asserted;
"#;

/// 0.3.2-beta.7: C1 anchored on the session Thing parameter, authority = owner.
const C1_PARAM: &str = r#"add constraint "C1 identity parameter" {
    search ?ep = [{(?e, epoch), ($session, session)}, *, ?at],
           ?m = [{(?i, item), ($session, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{($session, session), (?src, developer)}, "owner", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
    where ?introduced < ?at and ?c > 0%
    return ?e, ?i, ?ep, ?m;
};"#;

/// PLAN.md §8.3 extraction: one command, existing Things as identity parameters.
const EXTRACTION_ONE: &str = r#"
    add posit +m [{(+i, item), ($s, session)}, "registered", $at],
              [{(i, item_key)}, $key, $at], [{(i, kind)}, "prohibition", $at],
              [{(i, strength)}, "hard", $at], [{(i, statement)}, $text, $at],
              [{(i, cited_turn)}, $cite, $at],
              +a [{(m, posit), ($dev, ascertains)}, 100%, $at],
              [{(a, posit), ($ext, ascertains)}, 85%, @NOW],
              [{($s, extracted_through)}, $through, @NOW];
"#;

fn ts(session: usize, turn: usize) -> String {
    // One turn per 7 seconds, sessions offset by an hour; sub-second precision.
    let secs = 36_000 + session * 3_600 + turn * 7;
    let (d, rem) = (secs / 86_400, secs % 86_400);
    format!(
        "'{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}'",
        2026 + d / 336,
        1 + (d / 28) % 12,
        1 + d % 28,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        (session * 7919 + turn * 104_729) % 1_000_000
    )
}

fn session_script(s: usize, turns: usize) -> String {
    let mut out = String::new();
    let dev = s % 50;
    writeln!(
        out,
        r#"search [{{(?dev, name)}}, "developer:{dev}", *] or add posit [{{(+dev, name)}}, "developer:{dev}", @NOW], [{{(dev, authority)}}, "authoritative", @NOW];
search [{{(?ext, name)}}, "extractor:qwen", *] or add posit [{{(+ext, name)}}, "extractor:qwen", @NOW];"#
    )
    .unwrap();
    out.push_str("add posit ");
    let mut parts: Vec<String> = Vec::new();
    let t0 = ts(s, 0);
    parts.push(format!(r#"[{{(+s, session_key)}}, "sess-{s}", {t0}]"#));
    parts.push(format!(r#"[{{(s, session), (dev, developer)}}, "owner", {t0}]"#));
    for t in 0..turns {
        let at = ts(s, t);
        parts.push(format!(r#"[{{(+t{t}, turn), (s, session)}}, {t}, {at}]"#));
        parts.push(format!(r#"[{{(t{t}, digest)}}, "sha256:{s:08x}{t:08x}", {at}]"#));
        if t % 10 == 5 {
            let strength = if t % 20 == 5 { "hard" } else { "graded" };
            parts.push(format!(
                r#"+m{t} [{{(+i{t}, item), (s, session)}}, "registered", {at}] and assert +a{t} by dev with 100% at {at}"#
            ));
            parts.push(format!(r#"[{{(i{t}, item_key)}}, "sess-{s}#{t}", {at}]"#));
            parts.push(format!(r#"[{{(i{t}, kind)}}, "prohibition", {at}]"#));
            parts.push(format!(r#"[{{(i{t}, strength)}}, "{strength}", {at}]"#));
            parts.push(format!(r#"[{{(i{t}, statement)}}, "Do not modify files under area-{t}/ in session {s}", {at}]"#));
            parts.push(format!(r#"[{{(i{t}, enforce)}}, {{"ops":["write"],"paths":["area-{t}/**"]}}, {at}]"#));
        }
        if t % 20 == 15 {
            parts.push(format!(r#"[{{(+x{t}, action), (s, session)}}, {{"turn":{t},"op":"write","paths":["area-5/x.sql"]}}, {at}]"#));
            parts.push(format!(r#"[{{(x{t}, decision)}}, "blocked", {at}]"#));
            parts.push(format!(r#"[{{(x{t}, action), (i5, matched_item)}}, "match", {at}]"#));
        }
    }
    let packet = "x".repeat(1000);
    for (n, t) in [(1, turns / 2), (2, turns - 1)] {
        let at = ts(s, t);
        parts.push(format!(r#"[{{(+e{n}, epoch), (s, session)}}, "model_change", {at}]"#));
        parts.push(format!(r#"[{{(e{n}, packet)}}, "{packet}", {at}]"#));
        parts.push(format!(r#"[{{(e{n}, packet_of), (i5, packet_item)}}, "hard-active", {at}]"#));
    }
    out.push_str(&parts.join(",\n"));
    out.push_str(";\n");
    let metas: Vec<String> = (0..turns)
        .filter(|t| t % 10 == 5)
        .map(|t| format!(r#"[{{(a{t}, posit), (ext, ascertains)}}, 85%, {}]"#, ts(s, t + 1)))
        .collect();
    if !metas.is_empty() {
        out.push_str("add posit ");
        out.push_str(&metas.join(",\n"));
        out.push_str(";\n");
    }
    out
}

fn rss_mib() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

fn dir_mib(path: &std::path::Path) -> f64 {
    std::fs::read_dir(path)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum::<u64>() as f64
        / 1048576.0
}

fn pct(mut v: Vec<Duration>) -> String {
    v.sort();
    let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    format!("p50 {:>9.2?}  p95 {:>9.2?}  max {:>9.2?}  (n={})", p(0.5), p(0.95), p(1.0), v.len())
}

fn posit_count(db: &Database) -> usize {
    db.execute_one("search ?p = [*, *, *] return ?p;").unwrap().rows.len()
}

fn timed<F: FnMut(usize)>(n: usize, mut f: F) -> Vec<Duration> {
    (0..n)
        .map(|i| {
            let t = Instant::now();
            f(i);
            t.elapsed()
        })
        .collect()
}

fn check_summary(db: &Database, script: &str, opts: ExecutionOptions) -> (Duration, String, usize, u64) {
    let t = Instant::now();
    let set = db.execute_one_with_options(script, opts).unwrap();
    let elapsed = t.elapsed();
    let text = &set.cell(0, "report").unwrap().text;
    let v: serde_json::Value = serde_json::from_str(text).unwrap();
    (elapsed, v["verdict"].as_str().unwrap().to_string(), text.len(), v["work_units"].as_u64().unwrap_or(0))
}

fn reopen(dir: &str) {
    let before = rss_mib();
    let t = Instant::now();
    let db = Database::open(dir).unwrap();
    let elapsed = t.elapsed();
    let after = rss_mib();
    println!(
        "reopen (fresh process): {elapsed:.2?}, RSS {before:.1} MiB -> {after:.1} MiB, posits {}",
        posit_count(&db)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args[1] == "--reopen" {
        return reopen(&args[2]);
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let sessions: usize = args[2].parse().unwrap();
    let turns: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(200);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }

    println!("== {sessions} sessions x {turns} turns ({})", dir.display());
    let db = Database::open(&dir).unwrap();
    db.execute(ROLES).unwrap();
    let start = Instant::now();
    let per_session = timed(sessions, |s| {
        db.execute(&session_script(s, turns)).unwrap_or_else(|e| panic!("session {s}: {e}"));
    });
    let gen_elapsed = start.elapsed();
    let posits = posit_count(&db);
    println!(
        "generated {posits} posits in {gen_elapsed:.2?} ({:.0} posits/s); per-session script {}",
        posits as f64 / gen_elapsed.as_secs_f64(),
        pct(per_session)
    );
    println!("store on disk {:.1} MiB; RSS after generation {:.1} MiB", dir_mib(&dir), rss_mib());

    let single = timed(100, |n| {
        db.execute_with_options(
            "add posit [{(+t, turn), (+s, session)}, $n, @NOW];",
            ExecutionOptions::default().bind("n", n as i64),
        )
        .unwrap();
    });
    println!("single-posit add (one fsync):          {}", pct(single));

    // Revised extraction write: two keyed lookups, item batch + and-assert, meta-assertion.
    let typical = timed(100, |n| {
        let s = (n * 7919) % sessions;
        db.execute_with_options(
            &format!(
                r#"search [{{(?dev, name)}}, "developer:{d}", *] or add posit [{{(+dev, name)}}, "developer:{d}", @NOW];
                   search [{{(?ext, name)}}, "extractor:qwen", *] or add posit [{{(+ext, name)}}, "extractor:qwen", @NOW];
                   search [{{(?s, session_key)}}, "sess-{s}", *] or add posit [{{(+s, session_key)}}, "sess-{s}", @NOW];
                   add posit +m [{{(+i, item), (s, session)}}, "registered", $at] and assert +a by dev with 100% at $at,
                             [{{(i, item_key)}}, $key, $at], [{{(i, kind)}}, "prohibition", $at],
                             [{{(i, strength)}}, "hard", $at], [{{(i, statement)}}, $text, $at],
                             [{{(i, cited_turn)}}, $cite, $at];
                   add posit [{{(a, posit), (ext, ascertains)}}, 85%, @NOW];"#,
                d = s % 50
            ),
            ExecutionOptions::default()
                .bind("at", time(&format!("2030-01-01 00:{:02}:{:02}.{:06}", n / 60, n % 60, n)))
                .bind("key", format!("bench#{n}"))
                .bind("text", "Do not run database migrations")
                .bind("cite", ExecutionParameter::Literal { text: r#"{"turn":5,"quote":"no migrations from here"}"#.into() }),
        )
        .unwrap();
    });
    println!("extraction write, old (3 lookups + 2 commits): {}", pct(typical));

    let lookup = |script: &str, key: String| {
        let set = db
            .execute_one_with_options(script, ExecutionOptions::default().bind("key", key))
            .unwrap();
        thing(&db, &set.cell(0, "x").unwrap().text)
    };
    let ext = lookup(r#"search [{(?x, name)}, $key, *] return ?x;"#, "extractor:qwen".into());
    let ctx: Vec<(ExecutionParameter, ExecutionParameter)> = (0..100)
        .map(|n| {
            let s = (n * 7919) % sessions;
            (
                lookup(r#"search [{(?x, session_key)}, $key, *] return ?x;"#, format!("sess-{s}")),
                lookup(r#"search [{(?x, name)}, $key, *] return ?x;"#, format!("developer:{}", s % 50)),
            )
        })
        .collect();
    let one = timed(100, |n| {
        db.execute_with_options(
            EXTRACTION_ONE,
            ExecutionOptions::default()
                .bind("s", ctx[n].0.clone())
                .bind("dev", ctx[n].1.clone())
                .bind("ext", ext.clone())
                .bind("at", time(&format!("2031-01-01 00:{:02}:{:02}.{:06}", n / 60, n % 60, n)))
                .bind("key", format!("bench1#{n}"))
                .bind("text", "Do not run database migrations")
                .bind("cite", ExecutionParameter::Literal { text: r#"{"turn":5,"quote":"no migrations from here"}"#.into() })
                .bind("through", n as i64),
        )
        .unwrap();
    });
    println!("extraction write, 1 command + identity params: {}", pct(one));

    let sample: Vec<usize> = (0..50).map(|n| (n * 7919) % sessions).collect();
    let unanchored_ok = std::env::var("SKIP_PLAN_SKETCH").is_err();

    let read_revised = timed(sample.len(), |n| {
        db.execute_one_with_options(
            READ_REVISED,
            ExecutionOptions::default().bind("t", time("2040-01-01")).bind("key", format!("sess-{}", sample[n])),
        )
        .unwrap();
    });
    println!("register read, revised:                {}", pct(read_revised));

    let read_keyed_authority = timed(10, |n| {
        db.execute_one_with_options(
            &READ_REVISED.replace("return ?ikey", "[{(?src, authority)}, ?authority, *] return ?ikey"),
            ExecutionOptions::default().bind("t", time("2040-01-01")).bind("key", format!("sess-{}", sample[n])),
        )
        .unwrap();
    });
    println!("register read, keyed + authority join: {}", pct(read_keyed_authority));

    let things: Vec<ExecutionParameter> = sample
        .iter()
        .take(10)
        .map(|s| {
            let set = db
                .execute_one_with_options(
                    "search [{(?s, session_key)}, $key, *] return ?s;",
                    ExecutionOptions::default().bind("key", format!("sess-{s}")),
                )
                .unwrap();
            thing(&db, &set.cell(0, "s").unwrap().text)
        })
        .collect();
    if unanchored_ok {
        let read_plan = timed(10, |n| {
            db.execute_one_with_options(
                READ_PLAN,
                ExecutionOptions::default().bind("t", time("2040-01-01")).bind("session", things[n].clone()),
            )
            .unwrap();
        });
        println!("register read, PLAN sketch:            {}", pct(read_plan));
    }
    let read_param = timed(10, |n| {
        db.execute_one_with_options(
            READ_PARAM,
            ExecutionOptions::default().bind("t", time("2040-01-01")).bind("s", things[n].clone()),
        )
        .unwrap();
    });
    println!("register read, ($s, session) + authority: {}", pct(read_param));

    let rev = db.execute_one(C1_REVISED).unwrap().reference(0, "revision").unwrap();
    let mut lat = Vec::new();
    let mut last = (String::new(), 0, 0);
    for s in sample.iter().take(10) {
        let (d, verdict, bytes, work) = check_summary(
            &db,
            "check constraint $rev;",
            ExecutionOptions::default().bind("rev", rev.clone()).bind("session_key", format!("sess-{s}")),
        );
        lat.push(d);
        last = (verdict, bytes, work);
    }
    println!(
        "check C1 revised:                      {}  report {:.1} KiB  work {}  verdict {}",
        pct(lat),
        last.1 as f64 / 1024.0,
        last.2,
        last.0
    );
    // The C1 sketch grows roughly quadratically with the store (8.5 s at 100k); skip it at 1M.
    if unanchored_ok && posits <= 200_000 {
        let rev = db.execute_one(C1_PLAN).unwrap().reference(0, "revision").unwrap();
        let (d, verdict, bytes, work) = check_summary(
            &db,
            "check constraint $rev;",
            ExecutionOptions::default().bind("rev", rev).bind("session", things[0].clone()),
        );
        println!("check C1 PLAN sketch:                  {d:.2?}  report {:.1} KiB  work {work}  verdict {verdict}", bytes as f64 / 1024.0);
    }
    let rev_param = db.execute_one(C1_PARAM).unwrap().reference(0, "revision").unwrap();
    let lat: Vec<Duration> = things
        .iter()
        .map(|t| {
            check_summary(&db, "check constraint $rev;", ExecutionOptions::default().bind("rev", rev_param.clone()).bind("session", t.clone())).0
        })
        .collect();
    println!("check C1 ($session, session):          {}", pct(lat));
    let (d, _, bytes, _) = check_summary(
        &db,
        "check constraint $rev with full manifest;",
        ExecutionOptions::default().bind("rev", rev.clone()).bind("session_key", format!("sess-{}", sample[0])),
    );
    println!("check C1 revised, with full manifest:  {d:.2?}  report {:.1} KiB", bytes as f64 / 1024.0);
    let disk_before = dir_mib(&dir);
    let recorded = timed(10, |n| {
        db.execute_with_options(
            r#"search [{(?x, name)}, "handover", *] or add posit [{(+x, name)}, "handover", @NOW];
               check constraint $rev record by x;"#,
            ExecutionOptions::default().bind("rev", rev.clone()).bind("session_key", format!("sess-{}", sample[n])),
        )
        .unwrap();
    });
    println!(
        "check C1 revised + record by:          {}  disk +{:.2} MiB for 10 audits",
        pct(recorded),
        dir_mib(&dir) - disk_before
    );

    // Concurrent revised reads while the writer appends.
    let db = std::sync::Arc::new(db);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|r| {
            let (db, stop, key) = (db.clone(), stop.clone(), format!("sess-{}", sample[r]));
            std::thread::spawn(move || {
                let mut lat = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let t = Instant::now();
                    db.execute_one_with_options(
                        READ_REVISED,
                        ExecutionOptions::default().bind("t", time("2040-01-01")).bind("key", key.clone()),
                    )
                    .unwrap();
                    lat.push(t.elapsed());
                }
                lat
            })
        })
        .collect();
    let writes = timed(100, |n| {
        db.execute_with_options(
            "add posit [{(+t, turn), (+s, session)}, $n, @NOW];",
            ExecutionOptions::default().bind("n", n as i64),
        )
        .unwrap();
    });
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let lat: Vec<Duration> = readers.into_iter().flat_map(|h| h.join().unwrap()).collect();
    println!("writes with 4 concurrent readers:      {}", pct(writes));
    println!("  reads during those writes:           {}", pct(lat));
}
