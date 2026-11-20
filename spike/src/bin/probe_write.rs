//! Break down the cost of the typical extraction write script on an existing store.
//! Usage: cargo run --release --bin probe_write -- <store-dir>

use std::time::{Duration, Instant};

use positorium::{Database, ExecutionOptions};
use positorium_spike::time;

const LOOKUPS: &str = r#"
search [{(?dev, name)}, "developer:17", *] or add posit [{(+dev, name)}, "developer:17", @NOW];
search [{(?ext, name)}, "extractor:qwen", *] or add posit [{(+ext, name)}, "extractor:qwen", @NOW];
search [{(?s, session_key)}, "sess-17", *] or add posit [{(+s, session_key)}, "sess-17", @NOW];
search [{(?t, turn), (?s, session)}, 5, *] or add posit [{(+t, turn), (s, session)}, 5, @NOW];
"#;

const ITEM: &str = r#"
add posit +m [{(+i, item), (s, session)}, "registered", $at] and assert +a by dev with 100% at $at,
          [{(i, item_key)}, $key, $at], [{(i, kind)}, "prohibition", $at],
          [{(i, strength)}, "hard", $at], [{(i, statement)}, $text, $at],
          [{(i, cited_item), (t, cited_turn)}, $quote, $at];
"#;

const META: &str = "add posit [{(a, posit), (ext, ascertains)}, 85%, @NOW];";

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let db = Database::open(&dir).unwrap();
    let mut n = 0u32;
    let mut opts = || {
        n += 1;
        ExecutionOptions::default()
            .bind("at", time(&format!("2027-02-01 00:{:02}:{:02}.{:06}", n / 60 % 60, n % 60, n)))
            .bind("key", format!("probe#{n}"))
            .bind("text", "Do not run database migrations")
            .bind("quote", "no migrations from here")
    };
    for (label, script) in [
        ("lookups only (4 search-or-add hits)", LOOKUPS.to_string()),
        ("lookups + item command", format!("{LOOKUPS}{ITEM}")),
        ("lookups + item + meta (typical)", format!("{LOOKUPS}{ITEM}{META}")),
        ("one session lookup + item command", format!(
            r#"search [{{(?dev, name)}}, "developer:17", *] or add posit [{{(+dev, name)}}, "developer:17", @NOW];
               search [{{(?s, session_key)}}, "sess-17", *] or add posit [{{(+s, session_key)}}, "sess-17", @NOW];
               add posit +m [{{(+i, item), (s, session)}}, "registered", $at] and assert +a by dev with 100% at $at,
                         [{{(i, item_key)}}, $key, $at];"#)),
        ("item command, all new Things (no lookups)", r#"add posit +m [{(+i, item), (+s, session)}, "registered", $at] and assert +a by +dev with 100% at $at, [{(i, item_key)}, $key, $at];"#.to_string()),
    ] {
        let lat: Vec<Duration> = (0..20)
            .map(|_| {
                let o = opts();
                let t = Instant::now();
                db.execute_with_options(&script, o).unwrap();
                t.elapsed()
            })
            .collect();
        println!("{label:<45} median {:>9.2?}", median(lat));
    }
}
