//! Helpers shared by the spike tests and benchmarks.

use positorium::{CollectedResultSet, Database, ExecutionOptions, ExecutionParameter, Result};

/// Every role named in PLAN.md §8.1, plus the four role names Positorium's
/// readiness checker requires (`policy`, `manifest`, `delivery`, `previous`).
pub const ROLES: &str = "add role session_key, session, developer, harness, branch, turn, digest, \
    served_model, item, item_key, kind, statement, strength, enforce, weight, cited_item, cited_turn, \
    replacement, replaced, complement, complement_of, name, authority, epoch, to_model, packet, \
    packet_of, packet_item, action, decision, matched_item, extraction, extracted_through, \
    policy, manifest, delivery, previous;";

/// Render a result set as aligned text for `--nocapture` exploration.
pub fn dump(label: &str, set: &CollectedResultSet) {
    println!("--- {label} ({} rows) ---", set.rows.len());
    println!("{}", set.columns.join(" | "));
    for row in &set.rows {
        let cells: Vec<String> = row.iter().map(|c| format!("{:?}:{}", c.kind, c.text)).collect();
        println!("{}", cells.join(" | "));
    }
}

/// Store-qualified Thing reference for a numeric identity from this store.
pub fn thing(db: &Database, id: &str) -> ExecutionParameter {
    ExecutionParameter::Thing { text: format!("{}/{}", db.store_id(), id) }
}

/// Store-qualified Posit reference for a numeric identity from this store.
pub fn posit(db: &Database, id: &str) -> ExecutionParameter {
    ExecutionParameter::Posit { text: format!("{}/{}", db.store_id(), id) }
}

/// Time parameter from a Traqula time token body (without quotes).
pub fn time(text: &str) -> ExecutionParameter {
    ExecutionParameter::Time { text: format!("'{text}'") }
}

/// Certainty parameter such as `100%` or `-100%`.
pub fn certainty(text: &str) -> ExecutionParameter {
    ExecutionParameter::Literal { text: text.to_string() }
}

/// Execute and return every result set, printing them.
pub fn run(db: &Database, label: &str, script: &str, options: ExecutionOptions) -> Result<Vec<CollectedResultSet>> {
    let sets = db.execute_with_options(script, options)?;
    for (i, set) in sets.iter().enumerate() {
        dump(&format!("{label} #{i}"), set);
    }
    Ok(sets)
}

/// Column values of a result set as plain text, row-major.
pub fn texts(set: &CollectedResultSet, column: &str) -> Vec<String> {
    (0..set.rows.len())
        .filter_map(|r| set.cell(r, column).map(|c| c.text.clone()))
        .collect()
}
