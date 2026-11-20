//! Q1: how the proxy writes register facts.
//!
//! Questions:
//! - Can every role name from PLAN.md §8.1 be declared and used?
//! - Can a posit binding (`+m`) be recalled later in the *same* `add posit` command?
//! - How do we reference an already existing Thing (session, developer, item) in a
//!   later script, given that appearances accept only script variables?
//! - Does re-adding an identical proposition bind the canonical posit identity?
//! - Do value/time parameters carry JSON, long multi-line text, and sub-second times?

use positorium::{Database, ExecutionOptions, ResultCellKind};
use positorium_spike::{ROLES, certainty, run, texts, thing, time};


#[test]
fn all_plan_roles_can_be_declared() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    for role in ["session", "item", "kind", "statement", "packet", "action", "decision", "epoch"] {
        assert!(db.contains_role(role).unwrap(), "{role}");
    }
}

#[test]
fn posit_binding_is_not_recallable_within_one_command() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let err = db
        .execute(
            r#"add posit [{(+dev, name)}, "developer:42", '2026-11-20'],
                         [{(+s, session_key)}, "sess-1", '2026-11-20'];
               add posit +m [{(+i, item), (s, session)}, "registered", '2026-11-20 10:00:00'],
                         [{(m, posit), (dev, ascertains)}, 100%, '2026-11-20 10:00:00'];"#,
        )
        .unwrap_err();
    println!("same-command posit recall -> {err}");
    assert!(err.to_string().contains("not bound in a prior command"));
}

#[test]
fn and_assert_plus_one_command_for_the_meta_assertion() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    // Command 2 is one atomic batch: membership m + its developer assertion a + item facts.
    // Command 3 adds the extractor's meta-assertion on a (needs a prior-command binding).
    let script = r#"
        add posit [{(+dev, name)}, "developer:42", '2026-11-20'],
                  [{(+ext, name)}, "extractor:qwen", '2026-11-20'],
                  [{(+s, session_key)}, "sess-1", '2026-11-20'];
        add posit +m [{(+i, item), (s, session)}, "registered", '2026-11-20 10:00:00']
                    and assert +a by dev with 100% at '2026-11-20 10:00:00',
                  [{(i, statement)}, "Do not trigger a production deployment", '2026-11-20 10:00:00'],
                  [{(i, strength)}, "hard", '2026-11-20 10:00:00'];
        add posit [{(a, posit), (ext, ascertains)}, 85%, '2026-11-20 10:00:05'];
        search ?m = [{(?i, item), (?s, session)}, "registered", *],
               ?a = [{(?m, posit), (?src, ascertains)}, ?c, *],
               [{(?a, posit), (?meta, ascertains)}, ?mc, *],
               [{(?src, name)}, ?src_name, *],
               [{(?meta, name)}, ?meta_name, *]
        return ?i, ?m, ?a, ?src_name, ?c, ?meta_name, ?mc;
    "#;
    let sets = run(&db, "same-command binding", script, ExecutionOptions::default()).unwrap();
    let set = sets.last().unwrap();
    assert_eq!(set.rows.len(), 1);
    assert_eq!(set.cell(0, "c").unwrap().text, "100%");
    assert_eq!(set.cell(0, "mc").unwrap().text, "85%");
    assert_eq!(set.cell(0, "src_name").unwrap().text, "\"developer:42\"");
}

#[test]
fn appearance_parameters_are_rejected() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    let created = db.execute_one(r#"add posit [{(+s, session_key)}, "sess-1", @NOW]; search [{(?s, session_key)}, "sess-1", *] return ?s;"#).unwrap();
    let s = created.cell(0, "s").unwrap().text.clone();
    let err = db
        .execute_with_options(
            r#"add posit [{($s, session), (+i, item)}, "registered", @NOW];"#,
            ExecutionOptions::default().bind("s", thing(&db, &s)),
        )
        .unwrap_err();
    println!("parameter in add-posit appearance -> {err}");
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
        add posit +m [{(i, item), (s, session)}, "registered", '2026-11-20 10:00:00'];
        add posit [{(m, posit), (dev, ascertains)}, 0%, '2026-11-20 11:00:00'];
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
            .bind("enforce", positorium::ExecutionParameter::Literal { text: enforce.into() })
            .bind("w", positorium::ExecutionParameter::Literal { text: "0.30".into() }),
    )
    .unwrap();
    let set = sets.last().unwrap();
    let stored: String = serde_json::from_str(&set.cell(0, "p").unwrap().text).unwrap();
    assert_eq!(stored, packet, "packet bytes must survive exactly");
    assert_eq!(set.cell(0, "en").unwrap().text, enforce);
    assert_eq!(set.cell(0, "en").unwrap().kind, ResultCellKind::Literal);
    assert_eq!(set.cell(0, "w").unwrap().text, "0.30");
    println!("time cell: {}", set.cell(0, "t1").unwrap().text);
    let _ = certainty; // used elsewhere
}

#[test]
fn search_or_add_does_not_correlate_with_earlier_bindings_and_fans_out() {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    db.execute(
        r#"add posit [{(+s1, session_key)}, "sess-1", '2026-11-20'], [{(+s2, session_key)}, "sess-2", '2026-11-20'];
           add posit [{(+t1, turn), (s1, session)}, 5, '2026-11-20 10:00:00'],
                     [{(+t2, turn), (s2, session)}, 5, '2026-11-20 10:00:00'];"#,
    )
    .unwrap();
    // Intended: "turn 5 of sess-1". ?s inside the second search is a fresh lexical variable.
    db.execute(
        r#"search [{(?s, session_key)}, "sess-1", *] or add posit [{(+s, session_key)}, "sess-1", @NOW];
           search [{(?t, turn), (?s, session)}, 5, *] or add posit [{(+t, turn), (s, session)}, 5, @NOW];
           add posit [{(+i, cited_item), (t, cited_turn)}, "quote", '2026-11-20 10:01:00'];"#,
    )
    .unwrap();
    let cites = db.execute_one(r#"search ?c = [{(?i, cited_item), (?t, cited_turn)}, *, *] return ?c;"#).unwrap();
    println!("citations written: {}", cites.rows.len());
    assert_eq!(cites.rows.len(), 2, "fan-out: the citation was attached to turn 5 of BOTH sessions");

    // Correct: the lookup carries its own literal anchor, so it is correlated within one search.
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    db.execute(
        r#"add posit [{(+s1, session_key)}, "sess-1", '2026-11-20'], [{(+s2, session_key)}, "sess-2", '2026-11-20'];
           add posit [{(+t1, turn), (s1, session)}, 5, '2026-11-20 10:00:00'],
                     [{(+t2, turn), (s2, session)}, 5, '2026-11-20 10:00:00'];
           search [{(?s, session_key)}, "sess-1", *] or add posit [{(+s, session_key)}, "sess-1", @NOW];
           search [{(?k, session_key)}, "sess-1", *], [{(?t, turn), (?k, session)}, 5, *]
               or add posit [{(+t, turn), (s, session)}, 5, @NOW];
           add posit [{(+i, cited_item), (t, cited_turn)}, "quote", '2026-11-20 10:01:00'];"#,
    )
    .unwrap();
    let cites = db.execute_one(r#"search ?c = [{(?i, cited_item), (?t, cited_turn)}, *, *] return ?c;"#).unwrap();
    assert_eq!(cites.rows.len(), 1);
}
