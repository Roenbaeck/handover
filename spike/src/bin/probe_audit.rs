//! Does `record by` work when the report is large? Usage: probe_audit <store-dir>
use std::time::Instant;
use positorium::{Database, ExecutionOptions};

fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let db = Database::open(&dir).unwrap();
    let saved = db
        .execute_one(
            r#"add constraint "C1 audit probe" {
    search [{(?s, session_key)}, $session_key, *],
           ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?s, session), (?src, developer)}, "owner", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
    where ?introduced < ?at and ?c > 0%
    return ?e, ?i, ?ep, ?m;
};"#,
        )
        .unwrap();
    let rev = saved.reference(0, "revision").unwrap();
    let t = Instant::now();
    let result = db.execute_with_options(
        r#"search [{(?proxy, name)}, "handover", *] or add posit [{(+proxy, name)}, "handover", @NOW];
           check constraint $rev record by proxy;"#,
        ExecutionOptions::default().bind("rev", rev).bind("session_key", "sess-17"),
    );
    match result {
        Ok(sets) => {
            let set = sets.last().unwrap();
            println!(
                "record by OK in {:.2?}; report {:.1} KiB; audit {:?}",
                t.elapsed(),
                set.cell(0, "report").unwrap().text.len() as f64 / 1024.0,
                set.cell(0, "audit").map(|c| c.text.clone())
            );
        }
        Err(e) => println!("record by FAILED after {:.2?}: {e}", t.elapsed()),
    }
}
