//! Roles and saved constraints (PLAN.md §8.1, §11), keyed on the branch.
//!
//! Every read and audit is anchored on the `branch_key` literal (spike R5, S21).
//! Authority in audits is the branch's `{(b, branch), (p, authority)}` set, which
//! is bound through `?b` and keeps the plan anchored.

pub const ROLES: &str = "add role session_key, session, developer, harness, branch, branch_key, parent, \
    turn, digest, served_model, item, item_key, kind, statement, strength, enforce, cited_turn, \
    replacement, replaced, complement, complement_of, name, authority, epoch, epoch_key, to_model, packet, packet_of, packet_item, \
    action, decision, matched_item, extraction, extracted_through, policy, manifest, delivery, previous;";

pub struct Audit {
    pub name: &'static str,
    pub source: &'static str,
}

pub const C1: Audit = Audit {
    name: "Handoff packets cite every active hard item",
    source: r#"add constraint "Handoff packets cite every active hard item" {
    search [{(?b, branch_key)}, $branch_key, *],
           ?ep = [{(?e, epoch), (?b, branch)}, *, ?at],
           ?m = [{(?i, item), (?b, branch)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *],
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?b, branch), (?src, authority)}, "authoritative", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
    where ?introduced < ?at and ?c > 0%
    return ?e, ?i, ?ep, ?m;
};"#,
};

pub const C2: Audit = Audit {
    name: "Revoked hard items are cited as revoked",
    source: r#"add constraint "Revoked hard items are cited as revoked" {
    search [{(?b, branch_key)}, $branch_key, *],
           ?ep = [{(?e, epoch), (?b, branch)}, *, ?at],
           ?m = [{(?i, item), (?b, branch)}, "registered", *],
           [{(?i, strength)}, "hard", *],
           [{(?m, posit), (?src, ascertains)}, ?c, ?revoked_at] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?earlier, ?granted_at],
           [{(?b, branch), (?src, authority)}, "authoritative", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, "revoked", *] }
    where ?c <= 0% and ?earlier > 0% and ?granted_at < ?revoked_at
    return ?e, ?i, ?ep, ?m;
};"#,
};

pub const C3: Audit = Audit {
    name: "No forwarded action matched an active hard item",
    source: r#"add constraint "No forwarded action matched an active hard item" {
    search [{(?b, branch_key)}, $branch_key, *],
           ?act = [{(?x, action), (?b, branch)}, *, ?at],
           [{(?x, decision)}, "forwarded", *],
           [{(?x, action), (?i, matched_item)}, "match", *],
           ?m = [{(?i, item), (?b, branch)}, "registered", *],
           [{(?i, strength)}, "hard", *],
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?b, branch), (?src, authority)}, "authoritative", *]
    where ?c > 0%
    return ?x, ?i, ?act, ?m;
};"#,
};

/// Complementary pairs: a packet that cites an item also cites what it requires.
pub const C4: Audit = Audit {
    name: "Required items are cited together",
    source: r#"add constraint "Required items are cited together" {
    search [{(?b, branch_key)}, $branch_key, *],
           ?ep = [{(?e, epoch), (?b, branch)}, *, ?at],
           [{(?i, item), (?b, branch)}, "registered", *],
           [{(?i, complement), (?c, complement_of)}, *, ?since],
           [{(?e, packet_of), (?i, packet_item)}, *, *]
    not exists { [{(?e, packet_of), (?c, packet_item)}, *, *] }
    where ?since < ?at
    return ?e, ?i, ?c, ?ep;
};"#,
};

/// D7: loosening is expensive. Every authoritative withdrawal of a hard
/// prohibition must carry the provenance record written with it (the cited
/// turn and quote, or the admin decision); a bare assertion is a violation.
pub const C5: Audit = Audit {
    name: "Every lifted hard prohibition has cited provenance",
    source: r#"add constraint "Every lifted hard prohibition has cited provenance" {
    search [{(?b, branch_key)}, $branch_key, *],
           ?m = [{(?i, item), (?b, branch)}, "registered", *],
           [{(?i, kind)}, "prohibition", *],
           [{(?i, strength)}, "hard", *],
           ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?t],
           [{(?b, branch), (?src, authority)}, "authoritative", *]
    not exists { [{(?i, cited_turn)}, *, ?t] }
    where ?c <= 0%
    return ?a, ?i, ?m;
};"#,
};

pub const AUDITS: [Audit; 5] = [C1, C2, C3, C4, C5];
