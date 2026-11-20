//! Q1: how the proxy writes register facts.
//!
//! Questions:
//! - Can every role name from PLAN.md §8.1 be declared and used?
//! - Can a posit binding (`+m`, `+a`) be recalled later in the *same* `add posit` command?
//! - How do we reference an already existing Thing (session, developer, item) in a
//!   later script? (Identity parameters since 0.3.2-beta.7; keyed get-or-create on first sight.)
//! - Does re-adding an identical proposition bind the canonical posit identity?
//! - Do value/time parameters carry JSON, long multi-line text, and sub-second times?
//! - How do `search … or add posit` lookups scope, and what does `strict mutations;` guard?

use positorium::{Database, ExecutionOptions, ExecutionParameter, ResultCellKind};
use positorium_spike::{ROLES, run, texts, thing, time};

/// Create the per-session Things once and return their store-qualified references,
/// the way the proxy caches them in Rust at session start.
fn session_things(db: &Database) -> (ExecutionParameter, ExecutionParameter, ExecutionParameter) {
    let set = db
        .execute_one(
            r#"add posit [{(+dev, name)}, "developer:42", '2026-11-20'],
                         [{(+ext, name)}, "extractor:qwen", '2026-11-20'],
                         [{(+s, session_key)}, "sess-1", '2026-11-20'];
               search [{(?dev, name)}, "developer:42", *], [{(?ext, name)}, "extractor:qwen", *],
                      [{(?s, session_key)}, "sess-1", *]
               return ?dev, ?ext, ?s;"#,
        )
        .unwrap();
    let get = |c: &str| thing(db, &set.cell(0, c).unwrap().text);
    (get("dev"), get("ext"), get("s"))
}

#[test]
fn all_plan_roles_can_be_declared() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    for role in ["session", "item", "kind", "statement", "packet", "action", "decision", "epoch"] {
        assert!(db.contains_role(role).unwrap(), "{role}");
    }
}

#[test]
fn posit_bindings_are_recallable_within_one_command() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    // Target m, its assertion a (via `and assert +a`), and the meta-assertion on a:
    // one command, one durable batch.
    let sets = run(
        &db,
        "same-command recall",
        r#"add posit [{(+dev, name)}, "developer:42", '2026-11-20'],
                     [{(+ext, name)}, "extractor:qwen", '2026-11-20'],
                     [{(+s, session_key)}, "sess-1", '2026-11-20'];
           add posit +m [{(+i, item), (s, session)}, "registered", '2026-11-20 10:00:00']
                       and assert +a by dev with 100% at '2026-11-20 10:00:00',
                     [{(i, statement)}, "Do not trigger a production deployment", '2026-11-20 10:00:00'],
                     [{(a, posit), (ext, ascertains)}, 85%, '2026-11-20 10:00:05'];
           search ?m = [{(?i, item), (?s, session)}, "registered", *],
                  ?a = [{(?m, posit), (?src, ascertains)}, ?c, *],
                  [{(?a, posit), (?meta, ascertains)}, ?mc, *],
                  [{(?src, name)}, ?src_name, *]
           return ?i, ?src_name, ?c, ?mc;"#,
        ExecutionOptions::default(),
    )
    .unwrap();
    let set = sets.last().unwrap();
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.cell(0, "c").unwrap().text, "100%");
    assert_eq!(set.cell(0, "mc").unwrap().text, "85%");
    assert_eq!(set.cell(0, "src_name").unwrap().text, "\"developer:42\"");
}

#[test]
fn identity_parameters_work_in_appearances() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let (_, _, s) = session_things(&db);

    // Mutation and search both take `($s, session)` directly.
    let sets = run(
        &db,
        "identity params",
        r#"add posit [{($s, session), (+i, item)}, "registered", '2026-11-20 10:00:00'];
           search ?m = [{($s, session), (?i, item)}, "registered", *] return ?m, ?i;"#,
        ExecutionOptions::default().bind("s", s.clone()),
    )
    .unwrap();
    assert_eq!(sets.last().unwrap().rows.len(), 1);

    // A reference from another store is rejected on entry.
    let foreign = ExecutionParameter::Thing { text: "00000000-0000-0000-0000-000000000000/1".into() };
    let err = db
        .execute_with_options(
            r#"add posit [{($s, session), (+i, item)}, "registered", @NOW];"#,
            ExecutionOptions::default().bind("s", foreign),
        )
        .unwrap_err();
    println!("foreign store reference -> {err}");

    // A well-formed reference to a Thing that does not exist is rejected on write.
    let missing = thing(&db, "999999");
    let err = db
        .execute_with_options(
            r#"add posit [{($s, session), (+i, item)}, "registered", @NOW];"#,
            ExecutionOptions::default().bind("s", missing),
        )
        .unwrap_err();
    println!("missing Thing reference -> {err}");
    let items = db.execute_one(r#"search [{(?i, item), (?s, session)}, *, *] return ?i;"#).unwrap();
    assert_eq!(items.rows.len(), 1, "neither rejected write added anything");
}

/// The extraction write from PLAN.md §8.3: every item of one extraction, its developer
/// assertion, the extractor's meta-assertion and the branch watermark in one command.
const EXTRACTION: &str = r#"
    add posit +m [{(+i, item), ($b, branch)}, "registered", $turn_time],
              [{(i, item_key)}, $item_key, $turn_time],
              [{(i, kind)}, "prohibition", $turn_time],
              [{(i, statement)}, $statement, $turn_time],
              [{(i, strength)}, "hard", $turn_time],
              [{(i, enforce)}, $enforce_json, $turn_time],
              [{(i, cited_turn)}, $cite_json, $turn_time],
              +a [{(m, posit), ($dev, ascertains)}, 100%, $turn_time],
              [{(a, posit), ($ext, ascertains)}, 85%, $now],
              [{($b, extracted_through)}, $through, $now];
"#;

#[test]
fn an_extraction_is_one_atomic_command_with_identity_parameters() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let (dev, ext, s) = session_things(&db);
    // The branch is created once per session (here: the session Thing doubles as branch).
    let opts = |through: &str, stmt: &str| {
        ExecutionOptions::default()
            .bind("b", s.clone())
            .bind("dev", dev.clone())
            .bind("ext", ext.clone())
            .bind("item_key", "sess-1#1")
            .bind("statement", stmt)
            .bind("enforce_json", ExecutionParameter::Literal { text: r#"{"ops":["exec"],"commands":["deploy.*prod"]}"#.into() })
            .bind("cite_json", ExecutionParameter::Literal { text: r#"{"turn":3,"quote":"no prod deploys"}"#.into() })
            .bind("turn_time", time("2026-11-20 10:00:00"))
            .bind("now", time("2026-11-20 10:00:05"))
            .bind("through", ExecutionParameter::Literal { text: through.into() })
    };
    db.execute_with_options(EXTRACTION, opts("3", "Do not trigger a production deployment")).unwrap();

    // The explicit `{posit, ascertains}` posit is the same thing `and assert` writes.
    let read = r#"search [{($b, branch), (?i, item)}, "registered", *],
                         ?m = [{(?i, item), ($b, branch)}, "registered", *],
                         ?a = [{(?m, posit), (?src, ascertains)}, ?c, *],
                         [{(?a, posit), (?meta, ascertains)}, ?mc, *],
                         [{($b, extracted_through)}, ?through, *]
                  return ?i, ?c, ?mc, ?through;"#;
    let sets = run(&db, "extraction", read, ExecutionOptions::default().bind("b", s.clone())).unwrap();
    let set = sets.last().unwrap();
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.cell(0, "c").unwrap().text, "100%");
    assert_eq!(set.cell(0, "mc").unwrap().text, "85%");

    // A bad identity in the last addition rejects the whole command before anything is
    // written (parameters are checked on entry; the command is one durable batch).
    let bad = opts("4", "Never touch migrations/").bind("ext", thing(&db, "999999"));
    let err = db.execute_with_options(EXTRACTION, bad).unwrap_err();
    println!("failed batch -> {err}");
    let statements = db.execute_one(r#"search [{(?i, statement)}, ?st, *] return ?st;"#).unwrap();
    assert_eq!(statements.rows.len(), 1, "nothing from the failed batch was committed");
}

#[test]
fn existing_things_are_referenced_by_get_or_create_on_a_key() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let get_or_create = r#"
        search [{(?s, session_key)}, $key, *] or add posit [{(+s, session_key)}, $key, $t];
        search [{(?dev, name)}, $dev_name, *] or add posit [{(+dev, name)}, $dev_name, $t];
        add posit +m [{(+i, item), (s, session)}, "registered", $t]
            and assert by dev with 100% at $t;
        search [{(?s, session_key)}, $key, *], [{(?i, item), (?s, session)}, *, *] return ?s, ?i;
    "#;
    let opts = |t: &str| {
        ExecutionOptions::default()
            .bind("key", "sess-1")
            .bind("dev_name", "developer:42")
            .bind("t", time(t))
    };
    let first = run(&db, "first", get_or_create, opts("2026-11-20 10:00:00")).unwrap();
    let second = run(&db, "second", get_or_create, opts("2026-11-20 10:05:00")).unwrap();
    // Same session Thing both times, two distinct items.
    let s1 = texts(first.last().unwrap(), "s");
    let s2 = texts(second.last().unwrap(), "s");
    assert_eq!(s1.len(), 1);
    assert_eq!(s2.len(), 2);
    assert!(s2.iter().all(|s| s == &s1[0]));
    let sessions = db.execute_one(r#"search [{(?s, session_key)}, *, *] return ?s;"#).unwrap();
    assert_eq!(sessions.rows.len(), 1, "get-or-create must not duplicate the session");
}

#[test]
fn readding_an_identical_proposition_binds_the_canonical_posit() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let setup = db
        .execute_one(
            r#"add posit [{(+s, session_key)}, "sess-1", '2026-11-20'],
                         [{(+i, item_key)}, "sess-1#1", '2026-11-20'];
               add posit +m [{(i, item), (s, session)}, "registered", '2026-11-20 10:00:00'];
               search ?m = [{(?i, item), (?s, session)}, "registered", *] return ?m;"#,
        )
        .unwrap();
    let m_first = setup.cell(0, "m").unwrap().text.clone();

    // Later script: re-resolve Things by key, re-add the same proposition, assert on it.
    let later = r#"
        search [{(?s, session_key)}, "sess-1", *] or add posit [{(+s, session_key)}, "sess-1", @NOW];
        search [{(?i, item_key)}, "sess-1#1", *] or add posit [{(+i, item_key)}, "sess-1#1", @NOW];
        search [{(?dev, name)}, "developer:42", *] or add posit [{(+dev, name)}, "developer:42", @NOW];
        add posit +m [{(i, item), (s, session)}, "registered", '2026-11-20 10:00:00'],
                  [{(m, posit), (dev, ascertains)}, 0%, '2026-11-20 11:00:00'];
        search ?m = [{(?i, item), (?s, session)}, "registered", *] return ?m;
    "#;
    let sets = run(&db, "re-add", later, ExecutionOptions::default()).unwrap();
    let ms = texts(sets.last().unwrap(), "m");
    assert_eq!(ms, vec![m_first.clone()], "no second membership posit");
    let assertions = db
        .execute_one(r#"search [{(?m, posit), (?src, ascertains)}, ?c, ?t] return ?m, ?c, ?t;"#)
        .unwrap();
    assert_eq!(texts(&assertions, "m"), vec![m_first]);
}

#[test]
fn value_and_time_parameters_round_trip() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let packet = "<handover-register epoch=\"e-1\">\nHARD — in force\n  [P1] Do not deploy; quote: \"no prod deploys\" ; } ] \\ \n</handover-register>\n";
    let enforce = r#"{"ops":["exec"],"commands":["deploy.*prod"]}"#;
    let sets = run(
        &db,
        "params",
        r#"add posit [{(+e, epoch), (+s, session)}, "model_change", $t],
                     [{(e, packet)}, $packet, $t],
                     [{(+i, enforce)}, $enforce, $t],
                     [{(i, weight)}, $w, $t];
           search [{(?e, packet)}, ?p, ?t1], [{(?i, enforce)}, ?en, ?t2], [{(?i, weight)}, ?w, *]
           return ?p, ?t1, ?en, ?w;"#,
        ExecutionOptions::default()
            .bind("t", time("2026-11-20 14:03:07.123456"))
            .bind("packet", packet)
            .bind("enforce", ExecutionParameter::Literal { text: enforce.into() })
            .bind("w", ExecutionParameter::Literal { text: "0.30".into() }),
    )
    .unwrap();
    let set = sets.last().unwrap();
    let stored: String = serde_json::from_str(&set.cell(0, "p").unwrap().text).unwrap();
    assert_eq!(stored, packet, "packet bytes must survive exactly");
    assert_eq!(set.cell(0, "en").unwrap().text, enforce);
    assert_eq!(set.cell(0, "en").unwrap().kind, ResultCellKind::Literal);
    assert_eq!(set.cell(0, "w").unwrap().text, "0.30");
    println!("time cell: {}", set.cell(0, "t1").unwrap().text);
}

const TWO_SESSIONS_WITH_TURN_5: &str = r#"
    add posit [{(+s1, session_key)}, "sess-1", '2026-11-20'], [{(+s2, session_key)}, "sess-2", '2026-11-20'];
    add posit [{(+t1, turn), (s1, session)}, 5, '2026-11-20 10:00:00'],
              [{(+t2, turn), (s2, session)}, 5, '2026-11-20 10:00:00'];
"#;

fn citations(db: &Database) -> usize {
    db.execute_one(r#"search ?c = [{(?i, cited_item), (?t, cited_turn)}, *, *] return ?c;"#)
        .unwrap()
        .rows
        .len()
}

#[test]
fn search_or_add_scopes_lexically_and_strict_mutations_rejects_the_fan_out() {
    // Intended: "turn 5 of sess-1". ?s inside the second search is a fresh lexical variable.
    let uncorrelated = r#"
        search [{(?s, session_key)}, "sess-1", *] or add posit [{(+s, session_key)}, "sess-1", @NOW];
        search [{(?t, turn), (?s, session)}, 5, *] or add posit [{(+t, turn), (s, session)}, 5, @NOW];
        add posit [{(+i, cited_item), (t, cited_turn)}, "quote", '2026-11-20 10:01:00'];"#;

    // Default mode: unchanged semantics, the citation fans out to both sessions.
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    db.execute(TWO_SESSIONS_WITH_TURN_5).unwrap();
    db.execute(uncorrelated).unwrap();
    assert_eq!(citations(&db), 2, "fan-out: the citation was attached to turn 5 of BOTH sessions");

    // Strict mode: the write that consumes the two-identity binding is rejected.
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    db.execute(TWO_SESSIONS_WITH_TURN_5).unwrap();
    let err = db.execute(&format!("strict mutations;\n{uncorrelated}")).unwrap_err();
    println!("strict mutations -> {err}");
    assert_eq!(citations(&db), 0);

    // Correct: scope the lookup with the session's identity parameter.
    let s = db.execute_one(r#"search [{(?s, session_key)}, "sess-1", *] return ?s;"#).unwrap();
    let s = thing(&db, &s.cell(0, "s").unwrap().text);
    db.execute_with_options(
        r#"strict mutations;
           search [{(?t, turn), ($s, session)}, 5, *] or add posit [{(+t, turn), ($s, session)}, 5, @NOW];
           add posit [{(+i, cited_item), (t, cited_turn)}, "quote", '2026-11-20 10:01:00'];"#,
        ExecutionOptions::default().bind("s", s),
    )
    .unwrap();
    assert_eq!(citations(&db), 1);
}

#[test]
fn closed_appearance_sets_match_exactly_and_open_sets_match_supersets() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    db.execute(r#"add posit [{(+e, epoch), (+s, session)}, "model_change", '2026-11-20 10:15:00'];"#).unwrap();
    let closed = db.execute_one(r#"search [{(?e, epoch)}, *, *] return ?e;"#).unwrap();
    let open = db.execute_one(r#"search [{(?e, epoch), ...}, *, *] return ?e;"#).unwrap();
    assert_eq!(closed.rows.len(), 0, "{{(?e, epoch)}} matches only unary appearance sets");
    assert_eq!(open.rows.len(), 1, "{{(?e, epoch), ...}} also matches {{epoch, session}}");
}

#[test]
fn a_revocation_targets_the_membership_posit_by_parameter() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let (dev, _, s) = session_things(&db);
    let set = db
        .execute_with_options(
            r#"add posit +m [{(+i, item), ($s, session)}, "registered", '2026-11-20 10:00:00'],
                         [{(m, posit), ($dev, ascertains)}, 100%, '2026-11-20 10:00:00'];
               search ?m = [{(?i, item), ($s, session)}, "registered", *] return ?m;"#,
            ExecutionOptions::default().bind("s", s.clone()).bind("dev", dev.clone()),
        )
        .unwrap();
    let m = positorium_spike::posit(&db, &set.last().unwrap().cell(0, "m").unwrap().text);
    let sets = run(
        &db,
        "revoke by $m",
        r#"strict mutations;
           add posit [{($m, posit), ($dev, ascertains)}, 0%, '2026-11-20 10:30:00'];
           search [{($m, posit), (?src, ascertains)}, ?c, *] as of '2026-11-20 11:00:00' return ?c;"#,
        ExecutionOptions::default().bind("m", m).bind("dev", dev),
    )
    .unwrap();
    assert_eq!(texts(sets.last().unwrap(), "c"), vec!["0%"]);
}
