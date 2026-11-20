# Phase 1 spike: Positorium as the register store

Validates PLAN.md §8 and §11 against the published crate
`positorium = "=0.3.1-beta.6"` (persistence feature, no defaults). Each finding
links to the test or binary that demonstrates it.

```sh
cargo test --offline                                  # 18 tests, ~0.1 s
cargo run --release --bin bench -- <dir> 1700 200     # ~1M posits, ~2 min
cargo run --release --bin bench -- --reopen <dir>     # reopen in a fresh process
cargo run --release --bin probe -- <dir> [J]          # query variants (+ explain)
```

## Verdict

**Positorium works as the only store for the register.** Every semantic the
design needs is there:

- revocations are visible as a latest 0% position;
- −100% stays in effect;
- equal-time conflicts survive;
- meta-assertions give the bitemporal "what could the proxy know at T" view;
- saved constraints audit the handover duties with per-session readiness.

Four of the plan's sketches had to change: how existing Things are referenced
in writes, how reads and audits are anchored, how authority is joined, and how
large audit reports get. Store size has to be bounded by rotation.

## Semantics (tests/)

| # | Question | Answer | Test |
| --- | --- | --- | --- |
| S1 | Can all §8.1 role names be declared? | Yes, including `session`, `item`, `kind`, `action`, `decision` | `q1_writes::all_plan_roles_can_be_declared` |
| S2 | `$parameters` in appearances (`($branch, branch)`)? | **No.** Appearances accept only `+x`/`x` in `add posit` and `?x` in `search`. Parameters work in value, time and `where` positions | `q1::appearance_parameters_are_rejected` |
| S3 | How to reference an existing Thing? | Keyed get-or-create: `search [{(?s, session_key)}, $key, *] or add posit [{(+s, session_key)}, $key, $t];` promotes `s` into mutation scope for later commands | `q1::existing_things_are_referenced_by_get_or_create_on_a_key` |
| S4 | Recall a posit binding in the same command? | **No** (`variable 'm' is not bound in a prior command`). Use `add posit +m [...] and assert +a by dev with 100% at $t, [...]` (one atomic batch), then a second command for the extractor's meta-assertion on `a` | `q1::posit_binding_is_not_recallable_within_one_command`, `q1::and_assert_plus_one_command_for_the_meta_assertion` |
| S5 | Rebind an existing posit (e.g. to assert on it later)? | Re-add the identical proposition. It returns the canonical posit, creates nothing new and binds `+m` | `q1::readding_an_identical_proposition_binds_the_canonical_posit` |
| S6 | Parameters for packet text, JSON scopes, decimals, µs times? | Round-trip exactly (multi-line text with quotes, braces and backslashes; JSON as JSON literal; `0.30`; `'2026-11-20 14:03:07.123456'`) | `q1::value_and_time_parameters_round_trip` |
| S7 | **Hazard:** do lookups correlate with earlier bindings? | **No.** Query variables are lexical. `search [{(?t, turn), (?s, session)}, 5, *]` after a session lookup matches turn 5 of *every* session, and the next `add posit` fans out over all of them | `q1::search_or_add_does_not_correlate_with_earlier_bindings_and_fans_out` |
| S8 | **Hazard:** partial appearance-set lookups | `{(?e, epoch)}` does not match a stored `{(e, epoch), (s, session)}`. The search misses and the fallback silently creates a junk Thing | found in `q3::c1_…` (see comment there) |
| S9 | Does `as of` on assertion slots expose revocations? | Yes. The latest position per (target, source), including 0%, at any cut | `q2_reads::as_of_on_assertion_slots_keeps_latest_position_including_withdrawals` |
| S10 | `in effect` vs. 0% and −100% | 0% removes the target from the effective set; −100% stays in effect. Use `as of` on the assertion slot to show revoked items | `q2::in_effect_drops_withdrawals_but_keeps_negative_positions` |
| S11 | Rewording and equal-time conflicts | `as of` returns the latest wording; two equal-time statements both survive (disputed wording) | `q2::register_read_returns_latest_wording_and_equal_time_conflicts` |
| S12 | Meta-assertions (extractor ascertains the developer's assertion) | Work. At 10:50:05 the proxy could not know an item extracted at 10:50:07; at 12:00 it could | `q2::meta_assertion_joins_through_the_assertion_posit` |
| S13 | `as of ?at` with `?at` bound by another pattern | Works, which is what the epoch audits need | `q2::as_of_accepts_a_variable_bound_by_another_pattern` |
| S14 | Do the §11 audits validate as saved constraints? | Yes: C1 (active hard items cited), C2 (revoked items cited as revoked), C3 (no forwarded action matched an active hard item) | `q3_constraints::*` |
| S15 | Verdicts without readiness | Any finding query using `as of`, `in effect`, `latest` or `not exists` reports `indeterminate` until a readiness declaration covers the check. All our audits use `as of`, so **readiness is required to ever see `violated` or `satisfied`** | `q3::c1_needs_readiness_to_turn_a_missing_citation_into_a_violation` |
| S16 | Per-session readiness | Works. The declaration value must equal `{"ready": {"parameters": {"session_key": {"kind": "literal", "text": "\"sess-1\""}}, "clock": null}}`. It is tied to one constraint revision (roles `policy`, `manifest`, `delivery`, `previous` are fixed by the checker). Using it for another session fails | `q3::readiness_is_bound_to_the_session_parameter` |
| S17 | `record by` | Stores the report as a posit about the revision, ascertained at 100% by the evaluator. Returns an `audit` column | `q3::record_by_saves_the_report_as_an_audit_posit` |

## Performance (src/bin/bench.rs)

Machine: Apple M1 Pro, 16 GB, APFS on internal SSD, macOS 27.2, release build.
The synthetic load is about 3 posits per turn, 200 turns per session (items,
assertions, meta-assertions, actions, epochs with ~1 KB packets). Linux and
arm64 numbers, especially fsync, still need measuring.

| Measure | ~9k posits | ~100k posits | ~1M posits |
| --- | ---: | ---: | ---: |
| Single-posit `add posit` (one durable commit) p50 | 13.8 ms | 13.0 ms | 15.0 ms |
| Extraction write (keyed lookups + item batch + meta; 2 commits) p50 | — | 28 ms | 32 ms |
| Register read, **revised** (anchored on `session_key` literal, no authority join) p50 | — | 0.97 ms | 2.65 ms |
| Register read, keyed but joining `{(?src, authority)}` p50 | 6 ms | 46 ms | 367 ms |
| Register read, **PLAN sketch** (`where ?s = $session`) p50 | 123 ms | 7.7 s | skipped |
| Audit check C1, **revised** p50 | 5 ms | 17 ms | 206 ms |
| Audit check C1, PLAN sketch | 79 ms | 6.8 s | skipped |
| Check report size | 63 KiB | 832 KiB | **9.2 MiB** |
| Reads p50 while the writer commits (4 readers) | — | 0.84 ms | 2.85 ms |
| Reopen (replay) in a fresh process | 28 ms | 0.42 s | **7.8 s** |
| RSS after reopen | — | 135 MiB | **1.2 GiB** |
| Store on disk | 0.3 MiB | 8.9 MiB | 89 MiB |
| Bulk load rate (one script per session) | 3.3k posits/s | 14.7k posits/s | 14.2k posits/s |

What the numbers mean:

1. **Writes are fsync-bound, not size-bound.** One durable command costs about
   13–15 ms on this Mac at every store size. An extraction is 2 commands, so
   about 30 ms, which caps a single writer at roughly 30 extractions/s here.
   That is far above the expected load (one extraction per human-facing turn).
   Group commit is only worth building if Linux numbers are much worse.
2. **Anchor every query on a literal-valued unary key pattern.** The planner
   orders patterns by structural candidate count and ignores literal-value
   selectivity. `where ?s = $session` is applied after a store-wide scan
   (8 s at 100k). Joining a small global class such as `{(?src, authority)}`
   makes the plan start there and cross-join (`pattern_order [4,1,2,3]` in
   `probe … J`). Starting from `[{(?s, session_key)}, $key, *]` and resolving
   authority in Rust gives 1–3 ms.
3. **Authority in audits = session ownership.**
   `[{(?s, session), (?src, developer)}, "owner", *]` is bound through `?s`,
   so the plan stays anchored (`probe … L`: 0.69 ms vs. 31 ms).
4. **Audit cost and report size grow with the whole store.** Every report
   includes the store's full posit manifest. At 1M posits a check takes about
   200 ms and returns 9.2 MiB. `record by` stores that report (+9 MiB on disk
   per recorded audit). The 16 MiB storage-frame limit is reached at roughly
   1.8M posits.
5. **Memory and reopen time are about 1.2 KB and 7.7 µs per posit.** A 1M-posit
   store needs about 1.2 GiB resident and 8 s to start. Keep the proxy off the
   Sparks' unified memory, and bound the live store by rotation.

## Design rules that follow

- **R1. Every Thing a later script must reference gets a unique literal key**
  in a class-specific unary role (`session_key`, `branch_key`, `item_key`,
  `epoch_key`, positor `name`). Writes resolve it with a keyed get-or-create.
- **R2. Every lookup is self-anchored by a literal in its own search** (S7).
  Never rely on an earlier binding inside a `search … or add posit`.
- **R3. Look up by the unary key slot, never by a partial appearance set** (S8).
- **R4. Do not make turns lookup targets.** Citations and actions carry the turn
  index in a JSON value. Turn anchors (index, digest) are write-only and read
  back only at restart.
- **R5. Reads and audits start from `[{(?s, session_key)}, $session_key, *]`.**
  Authority is resolved in Rust for reads, and through session ownership in
  audits.
- **R6. One extraction = 2 commands:** item facts + membership + `and assert`
  (atomic), then the meta-assertion. A crash in between leaves an item with an
  unconfirmed extraction, which derives as `uncertain`. Retries are idempotent
  (S5).
- **R7. Keep each live store small:** rotate at about 1M posits or monthly,
  whichever comes first. Write turn anchors only at human-facing turns and
  epochs, not on every tool-loop request.
- **R8. Run audits at epoch changes, not continuously.** Record (`record by`)
  only non-`satisfied` verdicts, or write a compact summary posit instead
  (verdict, findings, report digest).

## Feedback for Positorium

These would simplify the proxy, and they are general enough to be worth
considering upstream:

1. **Identity parameters in appearances** (`($session, session)` in `add posit`
   and `search`). They would remove the keyed lookups (R1–R3) and the fan-out
   hazard (S7), and allow references to posits by reference
   (`check constraint`/readiness writes currently need a search-or-add with a
   sentinel fallback; see `declare_ready` in q3).
2. **Planner: use literal-value selectivity**, and treat an identity-parameter
   equality in `where` as an anchor. Both turn into seconds-long scans today
   (finding 2).
3. **Constraint reports: an option for a digest-only or read-scoped manifest.**
   The full store manifest makes reports O(store), and `record by` hits the
   16 MiB frame limit at about 1.8M posits (finding 4).
4. **A warning when `search … or add posit` binds more than one row** in a
   script that later mutates. This is S7's silent fan-out.
5. Optional: allow recalling a posit binding later in the same `add posit`
   command (S4). That would make extraction one atomic command.

## Still open after the spike

- fsync, reopen and RSS on Linux arm64 (the DGX Spark OS) and on the proxy host.
- Branch inheritance (fork/rewind) queries: walking parents in Rust is assumed
  and not yet exercised.
- Harness-sourced authority in audits (project rules from system prompts).
  Likely a second `union` branch, `[{(?s, session), (?src, instructed_by)}, "harness", *]`.
- Store rotation mechanics: dump and import of still-active sessions, and
  identity remapping of keys (keys are literals, so they survive remapping).
