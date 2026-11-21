# Phase 1 spike: Positorium as the register store

Validates PLAN.md §8 and §11 against the published crate
`positorium = "=0.3.3-beta.8"` (persistence feature, no defaults). The spike
first ran on 0.3.1-beta.6. Its feedback became
[Roenbaeck/positorium#6](https://github.com/Roenbaeck/positorium/issues/6)
(addressed in 0.3.2-beta.7) and
[#8](https://github.com/Roenbaeck/positorium/issues/8) (addressed in
0.3.3-beta.8). Numbers for each version are kept below. Each finding links to
the test or binary that demonstrates it.

```sh
cargo test --offline                                  # 21 tests, ~0.1 s
cargo run --release --bin bench -- <dir> 1700 200     # ~1M posits, ~3 min
cargo run --release --bin bench -- --reopen <dir>     # reopen in a fresh process
cargo run --release --bin probe -- <dir> [P]          # query variants (+ explain of one)
```

## Verdict

**Positorium works as the only store for the register.** Every semantic the
design needs is there:

- revocations are visible as a latest 0% position;
- −100% stays in effect;
- equal-time conflicts survive;
- meta-assertions give the bitemporal "what could the proxy know at T" view;
- saved constraints audit the handover duties with per-session readiness.

Since 0.3.2-beta.7:

- **an extraction is one command and one commit**, with existing Things passed
  as identity parameters;
- `strict mutations;` turns silent fan-out into an error;
- audit reports are about 8 KiB whatever the store size, so `record by` is
  cheap.

Since 0.3.3-beta.8 every query shape the design uses is sub-millisecond at 1M
posits, whether it is anchored on a key literal, an identity parameter in an
appearance, or `where ?s = $s` among other predicates (S21). Store size is
still bounded by rotation, only because of memory and reopen time.

## Semantics (tests/)

| # | Question | Answer | Test |
| --- | --- | --- | --- |
| S1 | Can all §8.1 role names be declared? | Yes, including `session`, `item`, `kind`, `action`, `decision` | `q1_writes::all_plan_roles_can_be_declared` |
| S2 | `$parameters` in appearances (`($branch, branch)`)? | **Yes, since 0.3.2-beta.7**, in both `add posit` and `search`, for Thing and Posit parameters. A reference from another store is rejected on entry, and an unknown identity is rejected on write. (0.3.1-beta.6: parse error) | `q1::identity_parameters_work_in_appearances`, `q1::a_revocation_targets_the_membership_posit_by_parameter` |
| S3 | How to reference an existing Thing? | Pass its cached identity as a parameter. On first sight (or after a restart), use a keyed get-or-create: `search [{(?s, session_key)}, $key, *] or add posit [{(+s, session_key)}, $key, $t];` promotes `s` into mutation scope | `q1::existing_things_are_referenced_by_get_or_create_on_a_key` |
| S4 | Recall a posit binding in the same command? | **Yes, since 0.3.2-beta.7.** `+m` and `and assert +a` can be recalled by later additions in the same `add posit`, so target, assertion and meta-assertion share one commit. (0.3.1-beta.6: `not bound in a prior command`) | `q1::posit_bindings_are_recallable_within_one_command`, `q1::an_extraction_is_one_atomic_command_with_identity_parameters` |
| S5 | Rebind an existing posit (e.g. to assert on it later)? | Pass it as a Posit parameter, `($m, posit)`, or re-add the identical proposition, which returns the canonical posit and creates nothing | `q1::readding_an_identical_proposition_binds_the_canonical_posit` |
| S6 | Parameters for packet text, JSON scopes, decimals, µs times? | Round-trip exactly (multi-line text with quotes, braces and backslashes; JSON as JSON literal; `0.30`; `'2026-11-20 14:03:07.123456'`) | `q1::value_and_time_parameters_round_trip` |
| S7 | Do lookups correlate with earlier bindings? | **No**, query variables are lexical. `search [{(?t, turn), (?s, session)}, 5, *]` after a session lookup matches turn 5 of *every* session. By default the next `add posit` fans out; **with `strict mutations;` it is rejected** (`'t' holds 2 identities`). Scope the lookup with `($s, session)` | `q1::search_or_add_scopes_lexically_and_strict_mutations_rejects_the_fan_out` |
| S8 | Closed vs. open appearance sets | `{(?e, epoch)}` matches only unary sets; `{(?e, epoch), ...}` also matches `{epoch, session}`. This was always documented; the spike's first round wrongly reported it as a hazard | `q1::closed_appearance_sets_match_exactly_and_open_sets_match_supersets` |
| S9 | Does `as of` on assertion slots expose revocations? | Yes. The latest position per (target, source), including 0%, at any cut | `q2_reads::as_of_on_assertion_slots_keeps_latest_position_including_withdrawals` |
| S10 | `in effect` vs. 0% and −100% | 0% removes the target from the effective set; −100% stays in effect. Use `as of` on the assertion slot to show revoked items | `q2::in_effect_drops_withdrawals_but_keeps_negative_positions` |
| S11 | Rewording and equal-time conflicts | `as of` returns the latest wording; two equal-time statements both survive (disputed wording) | `q2::register_read_returns_latest_wording_and_equal_time_conflicts` |
| S12 | Meta-assertions (extractor ascertains the developer's assertion) | Work. At 10:50:05 the proxy could not know an item extracted at 10:50:07; at 12:00 it could | `q2::meta_assertion_joins_through_the_assertion_posit` |
| S13 | `as of ?at` with `?at` bound by another pattern | Works, which is what the epoch audits need | `q2::as_of_accepts_a_variable_bound_by_another_pattern` |
| S14 | Do the §11 audits validate as saved constraints? | Yes: C1 (active hard items cited), C2 (revoked items cited as revoked), C3 (no forwarded action matched an active hard item) | `q3_constraints::*` |
| S15 | Verdicts without readiness | Any finding query using `as of`, `in effect`, `latest` or `not exists` reports `indeterminate` until a readiness declaration covers the check. All our audits use `as of`, so **readiness is required to ever see `violated` or `satisfied`** | `q3::c1_needs_readiness_to_turn_a_missing_citation_into_a_violation` |
| S16 | Per-session readiness | Works. The declaration value must equal `{"ready": {"parameters": {"session_key": {"kind": "literal", "text": "\"sess-1\""}}, "clock": null}}` and is tied to one constraint revision (roles `policy`, `manifest`, `delivery`, `previous` are fixed by the checker). Using it for another session fails. Since 0.3.2-beta.7, run, manifest entry and declaration are one command with `($rev, policy)` | `q3::readiness_is_bound_to_the_session_parameter` |
| S17 | `record by` | Stores the report as a posit about the revision, ascertained at 100% by the evaluator. Returns an `audit` column | `q3::record_by_saves_the_report_as_an_audit_posit` |
| S18 | Report size | Report version 2 carries `snapshot.digest`, `snapshot.count` and `read_posits`; `with full manifest` adds the sorted `snapshot.posits` with the same digest | `q3::reports_carry_a_digest_by_default_and_the_full_manifest_on_request` |
| S19 | Atomicity of one command | A command with a bad identity in its last addition commits nothing | `q1::an_extraction_is_one_atomic_command_with_identity_parameters` |
| S20 | `and assert … by $dev`? | **Yes, since 0.3.3-beta.8.** (0.3.2-beta.7: the source had to be `+x` or `x`; the explicit `+a [{(m, posit), ($dev, ascertains)}, 100%, $t]` is the same posit and still works) | grammar (`assertion_source`); used in `q1::an_extraction_is_one_…` |
| S21 | Which identity anchors steer the plan? | **All of them, since 0.3.3-beta.8**: a key literal, `($s, session)` in an appearance, and `where ?s = $s` among other predicates. At 1M posits `probe` P, Q, V and R take 0.17–0.40 ms. (0.3.2-beta.7: `where ?s = $s` steered only as the sole predicate, 270 ms with one more conjunct; `($s, session)` next to `authority` took 220 ms; the five-pattern audit 7.8 s at 100k) | `src/bin/probe.rs` N–X |

## Performance (src/bin/bench.rs)

Machine: Apple M1 Pro, 16 GB, APFS on internal SSD, macOS 27.2, release build.
The synthetic load is about 3 posits per turn, 200 turns per session (items,
assertions, meta-assertions, actions, epochs with ~1 KB packets). Linux and
arm64 numbers, especially fsync, still need measuring.

| Measure (p50) | ~9k posits | ~100k posits | ~1M posits | 0.3.1-beta.6 at ~1M |
| --- | ---: | ---: | ---: | ---: |
| Single-posit `add posit` (one durable commit) | 13.7 ms | 14.0 ms | 17.4 ms | 15.0 ms |
| **Extraction write, 1 command with identity parameters** | 15.3 ms | 15.7 ms | **15.8 ms** | — |
| Extraction write, old shape (3 keyed lookups + 2 commits) | 28.8 ms | 31.6 ms | 40.7 ms | 32 ms |
| Register read anchored on `session_key` | 1.15 ms | 1.12 ms | 2.66 ms | 2.65 ms |
| Same + `{(?src, authority)}` join | 1.02 ms | 1.09 ms | **2.40 ms** | 367 ms |
| PLAN sketch read (`where ?s = $session` only) | 0.88 ms | 0.87 ms | **2.07 ms** | 7.7 s at 100k |
| `($s, session)` in appearance + authority join | 3.06 ms | 20 ms | 244 ms | — |
| Audit C1 anchored on `session_key` | 1.42 ms | 1.59 ms | **2.20 ms** | 206 ms |
| Audit C1 with `($session, session)` | 1.74 ms | 16 ms | 356 ms | — |
| Audit C1, PLAN sketch (`where ?s = $session and …`) | 362 ms | 8.5 s | skipped | 6.8 s at 100k |
| Check report, default | 7.2 KiB | 8.4 KiB | **7.7 KiB** | 9.2 MiB |
| Check report, `with full manifest` | 107 KiB | 840 KiB | 9.0 MiB | — |
| Check + `record by` (disk per 10 audits) | 19 ms (+0.07 MiB) | 15 ms (+0.08 MiB) | 18 ms (+0.07 MiB) | +9 MiB per audit |
| Reads while the writer commits (4 readers) | 1.02 ms | 0.81 ms | 0.96 ms | 2.85 ms |
| Reopen (replay) in a fresh process | — | 0.56 s | 9.9 s | 7.8 s |
| RSS after reopen | — | 142 MiB | 1.19 GiB | 1.2 GiB |
| Store on disk | 1.1 MiB | 8.9 MiB | 89 MiB | 89 MiB |
| Bulk load rate (one script per session) | 11.4k posits/s | 12.7k posits/s | 12.9k posits/s | 14.2k posits/s |

The 1M p95 for the anchored read (47 ms) was disturbed by a compile running at
the same time. The p50s and the 100k run are clean.

### Re-run on 0.3.3-beta.8 (same machine and fixture, p50)

| Measure | ~100k posits | ~1M posits | 0.3.2-beta.7 at ~1M |
| --- | ---: | ---: | ---: |
| Extraction write, 1 command | 15.5 ms | 17.2 ms | 15.8 ms |
| Register read anchored on `session_key` | 0.86 ms | 0.84 ms | 2.66 ms |
| Same + `{(?src, authority)}` join | 0.83 ms | 0.91 ms | 2.40 ms |
| `($s, session)` in appearance + authority join | 0.69 ms | 0.83 ms | 244 ms |
| `where ?s = $s and ?c > 0%` (`probe` V) | 0.19 ms | 0.19 ms | 270 ms |
| Audit C1 anchored on `session_key` | 0.51 ms | 0.63 ms | 2.20 ms |
| Audit C1 with `($session, session)` | 0.64 ms | 0.62 ms | 356 ms |
| Audit C1, PLAN sketch (`where ?s = $session and …`) | 0.81 ms | 0.40 ms (`probe` R) | 8.5 s at 100k |
| Check report, default | 5.4 KiB | 5.7 KiB | 7.7 KiB |
| Reads while the writer commits (4 readers) | 0.70 ms | 0.72 ms | 0.96 ms |

Writes are unchanged (fsync-bound). Every read and audit shape is now below
1 ms at 1M posits, independent of how it is anchored.

What the numbers mean:

1. **Writes are fsync-bound, not size-bound.** One durable command costs about
   14–17 ms on this Mac at every store size. An extraction is now one command,
   so one writer handles about 60 extractions/s here, far above the expected
   one per human-facing turn. Group commit is not needed unless Linux fsync is
   much slower.
2. **Anchor reads and audits on the key literal.**
   `[{(?s, session_key)}, $session_key, *]` gives 1–3 ms at 1M with or without
   an authority join, so authority can now be joined in the query. A
   `where ?s = $s` anchor is as fast, but only while it is the sole predicate.
   Audits always have more predicates, so they stay keyed. `($s, session)` is
   the right form for *writes*, which are not planned. For reads it is fine
   unless a small global class is joined (S21).
3. **Authority in audits = session ownership.**
   `[{(?s, session), (?src, developer)}, "owner", *]` keeps the plan anchored
   through `?s` (`probe` U: 1.5 ms at 1M).
4. **Audit reports are constant-size.** About 8 KiB at every store size, and
   `record by` adds about 7 KiB on disk per audit. Every verdict can be
   recorded. `with full manifest` (9 MiB at 1M) is for forensic exports only.
5. **Memory and reopen time are about 1.2 KB and 10 µs per posit.** A 1M-posit
   store needs about 1.2 GiB resident and 10 s to start. These now decide
   rotation. Keep the proxy off the Sparks' unified memory.

## Design rules that follow

- **R1. Every Thing a later script must reference has a unique literal key** in
  a class-specific unary role (`session_key`, `branch_key`, `item_key`,
  `epoch_key`, positor `name`). The key anchors reads and audits, resolves
  identities after a restart, and survives store rotation. The proxy caches the
  identity in Rust and writes with identity parameters. Keyed get-or-create is
  only for first sight.
- **R2. Every mutating script starts with `strict mutations;`** (S7). A lookup
  inside a script is scoped by an identity parameter or a literal in its own
  search.
- **R3. Use `{(?x, role), ...}` to match a superset** (S8). Closed sets match
  exactly.
- **R4. Do not make turns lookup targets.** Citations and actions carry the turn
  index in a JSON value. Turn anchors (index, digest) are write-only and read
  back only at restart.
- **R5. Reads and audits start from a key literal** such as
  `[{(?b, branch_key)}, $branch_key, *]`. Since 0.3.3-beta.8 identity anchors are
  as fast (S21), so this is now a convention for readable, rotation-proof
  scripts rather than a performance requirement.
- **R6. One extraction = one command:** every item of the delta, its developer
  assertion, the extractor's meta-assertion and the branch watermark (S4,
  S19). The watermark moves exactly when the items are stored, so a retry is
  safe.
- **R7. Keep each live store small:** rotate at about 1M posits or monthly,
  whichever comes first, for memory and start-up time. Write turn anchors only
  at human-facing turns and epochs, not on every tool-loop request.
- **R8. Run audits at epoch changes and on a schedule, and record every
  verdict** (`record by`, about 7 KiB each). Use `with full manifest` only for
  exports.

## Feedback for Positorium

Issue [#6](https://github.com/Roenbaeck/positorium/issues/6) listed six items.
All of them are addressed in 0.3.2-beta.7 and verified here:

| # | Item | Status |
| --- | --- | --- |
| 1 | Planner literal selectivity and identity anchors | Fixed for key literals and a lone `where ?s = $s`. Residual gaps in S21 |
| 2 | Report embeds full manifest | Fixed: report v2, `with full manifest` on request (S18) |
| 3 | Identity parameters in appearances | Fixed (S2); `and assert by $x` still not accepted (S20) |
| 4 | Silent search-or-add fan-out | Fixed: `strict mutations;` (S7). The partial-set part was a misreading (S8) |
| 5 | Same-command posit recall | Fixed (S4) |
| 6 | Docs (`in effect`), linux/arm64 build | Fixed upstream; not exercised here |

Issue [#8](https://github.com/Roenbaeck/positorium/issues/8) reported the
remaining planner gaps (S21) and `and assert … by $param` (S20). Both are fixed
in 0.3.3-beta.8 and verified above. Nothing is outstanding.

## Still open after the spike

- fsync, reopen and RSS on Linux arm64 (the DGX Spark OS) and on the proxy host.
- Branch inheritance (fork/rewind) queries: walking parents in Rust is assumed
  and not yet exercised.
- Harness-sourced authority in audits (project rules from system prompts).
  Likely a second `union` branch, `[{(?s, session), (?src, instructed_by)}, "harness", *]`.
- Store rotation mechanics: dump and import of still-active sessions, and
  identity remapping of keys (keys are literals, so they survive remapping;
  cached identities must be re-resolved).
