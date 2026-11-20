# PLAN: handover

A Rust proxy that keeps a session's governing constraints alive when the
answering model changes, using Positorium as its only durable store.

> Status: design plan. The Phase 1 spike is done. The Traqula in §8.3 and §11
> is validated against `positorium = "=0.3.1-beta.6"` (18 tests, benchmarks to
> 1M posits). See [spike/FINDINGS.md](spike/FINDINGS.md).

## 1. Purpose and scope

Developers run harnesses (Claude Code, Codex, OpenCode, ...) against a fleet of
NVIDIA DGX Sparks (GB10) that serve the same open-weight model. Between the
harness and the model sits a router that keeps sessions on the Spark that holds
their KV cache. The answering model can still change: a rolling upgrade, an
escalation to a stronger model, a fallback, or the harness compacting its own
history. A KV cache never crosses that boundary. What must cross it are the
**permissions, prohibitions, commitments and goals** the developer set earlier
in the session.

`handover` is a thin proxy in front of whatever router is used. It:

1. maintains a provenance-bearing **constraint register** per session, in
   Positorium;
2. updates the register in the background by **reusing the answering
   model's hot KV cache** (Option B from the design discussion), at almost no
   extra cost;
3. injects a status-aware **handoff packet** only when a boundary is crossed;
4. **enforces** machine-checkable hard constraints on tool calls
   deterministically, whatever the model does; and
5. **audits** its own handover duties with Positorium saved constraints, so
   "did the successor get every hard constraint?" has a recorded verdict.

The design follows Rönnbäck (2026), *Bounds on user repair after context loss in
AI handoffs* (Discover Informatics, doi:10.1007/s44564-026-00035-w). In the
paper's terms, the proxy's job is to make hard items **system-recoverable**
(drive the hard residual δᴴ to zero). The paper's coding case shows what
happens otherwise. A permission revocation that no system source holds has a
recovery ceiling of zero, so the user must repair it.

**Non-goals**

- KV-aware load balancing, prefill/decode placement and worker health. These
  belong to the next hop (vllm-router, SGLang gateway, Dynamo, llm-d, ...).
- Replacing the harness's own permission system. The proxy is defense in depth.
- Storing transcripts, or summarizing code or discussion. The register stays
  narrow (paper §3.1, and the "keep the register narrow" guardrail).

## 2. Ground rules

| # | Decision | Why |
| --- | --- | --- |
| D1 | **Rust only**: proxy, mock workers, evaluation tooling | One toolchain across the air gap. Positorium embeds natively |
| D2 | **Positorium is the only durable store.** All other state is an in-memory cache, rebuildable from Positorium or from the next request. No Redis, Postgres or SQLite | The data is evidence: sourced, timed, revisable and sometimes contradictory. The earlier "hybrid" advice assumed beta.3's serial execution. 0.3.1-beta.6 has concurrent snapshot reads and a single writer, and Positorium stays off the hot path anyway (§8.4) |
| D3 | **Router-agnostic downstream.** The contract with the next hop is OpenAI Chat Completions over HTTP + SSE, plus an optional affinity hint | Works with vllm-router, SGLang gateway, LiteLLM, Dynamo, a bare vLLM, or a cloud endpoint (§12) |
| D4 | **Invisible until needed.** A session that never crosses a boundary is forwarded byte-identically | Zero prompt edits means zero KV-cache impact in the common case |
| D5 | **Rewrite prompts only at "free moments"**: when the prefix cache is lost anyway (model change, compaction, branch). Within an epoch, re-apply byte-identical edits | Keeps prefixes stable for automatic prefix caching (APC) and cache-aware routing |
| D6 | **Hard constraints are enforced on actions, not only in prompts** | A prompt is advisory. Paper §3.3: a preserved item "must govern action" |
| D7 | **Tightening is cheap, loosening is expensive.** A new prohibition can come from uncertain evidence (it becomes *hold/confirm*). Revoking a prohibition or granting a permission needs an authoritative, verified quote | An extractor that is wrong or prompt-injected can only make the agent more cautious |
| D8 | **Never drop silently.** If unsure, keep the item as `uncertain` or `disputed` | Paper §3.3/§6.4. Status, not deletion |

## 3. Paper concepts and how the system realizes them

| Paper (§) | In `handover` |
| --- | --- |
| Inventory item ⟨content, source, time, status, confidence, role⟩ (§3.1) | One register **item** Thing. Content = `statement` posit. Source = the positor of an assertion. Time = appearance/assertion time. Confidence = signed certainty. Role = `kind` posit |
| Roles: memory, commitment, goal, permission, prohibition, user relation, artifact, role boundary, known limitation | Allowed `kind` values. v1 extracts permission, prohibition, commitment, goal and role boundary |
| Status: active, superseded, revoked, disputed, uncertain | **Derived** at query time from assertions (§8.2), never stored as a mutable field |
| Hard flag hᵢⱼ, weight wᵢⱼ | `strength` = `"hard"` or `"graded"`. Optional `weight` posit, used only by the evaluation harness |
| Authorized register update (§3.2) | Trust policy: only authoritative sources (developer, harness instructions) can establish or loosen hard items. The update history is kept, since Positorium is append-only |
| Recovery ceiling aᵢⱼ, recoverability Aᵢ (§3.4) | The register *is* the system source that holds revocations, so a revocation becomes recoverable |
| Six-step handoff protocol (§6.4) | 1–2: register lane. 3: packet builder. 4: enforcer and audits. 5: Option A fallback. 6: *hold* responses that ask one targeted question |
| Green / amber / red (§6.4) | Per-session status from audit verdicts (§11) |
| Branch-relative registers (§7.2) | Rewind or fork detection gives the new branch an inherited register (§6) |
| Prospective protocol, four packet conditions (§8.5) | Packet strategies are pluggable, so the proxy doubles as the pilot's apparatus (Phase 8) |

## 4. Architecture

```text
 Developer harnesses (Claude Code / Codex / OpenCode / custom)
        │  OpenAI Chat Completions · Anthropic Messages · Responses (stateless)
        ▼
┌──────────────────────────────── handover (one Rust process) ────────────────────────────────┐
│ ingress adapters ─► session tracker ─► epoch manager ─► rewriter ─► forwarder ═╦═► client   │
│   (normalize,        (identity, digest    (free moment?   (packet bytes,       ║   (SSE)    │
│    classify human/   chain, compaction/   build packet)   affinity header,     ║            │
│    tool/harness)     branch detection)                    pinned model)        ╠═► enforcer │
│                                                                                ║ (tool calls)│
│                                                                                ╚═► tee ─┐   │
│  register lane: deterministic events · Option B extractor · Option A fallback ◄─────────┘   │
│        │ verify (quotes, roles, trust policy)                                               │
│        ▼                                                                                    │
│  register store: Positorium (embedded) — writer thread + snapshot readers                   │
│  auditor: saved Traqula constraints → green/amber/red · admin API · /metrics                │
└──────────────────────────────────────────────┬──────────────────────────────────────────────┘
                                               │ OpenAI Chat Completions + SSE
                                               │ (+ X-Session-ID, pinned `model`)
                                               ▼
     any next hop: vllm-router · SGLang gateway · LiteLLM · Dynamo · llm-d · bare vLLM · cloud
                                               │
                         ┌─────────────────────┼─────────────────────┐
                         ▼                     ▼                     ▼
                    Spark GB10 #1         Spark GB10 #2         Spark GB10 #n
```

The hot path (ingress, forwarder, tee) touches only in-memory session state.
Positorium is read at epoch changes, after register writes (to refresh
compiled enforcement rules) and by audits. It is written asynchronously by one
writer thread.

## 5. Request lifecycle

1. **Authenticate and resolve the session.** The bearer token maps to a
   developer, via a static map in config. The session key comes from, in
   order: an explicit `X-Session-ID` header, harness-native metadata via the
   harness profile, or a weak fallback digest. See §13.
2. **Normalize and digest.** Convert the request to a canonical conversation:
   ordered messages, with each content span labeled `human`, `harness`,
   `tool_result`, `assistant` or `system`. Compute a rolling digest chain
   `hᵢ = H(hᵢ₋₁ ‖ canon(msgᵢ))`. Compare it with the session's stored chain to
   classify the request as *continuation*, *compaction*, *branch/rewind* or
   *new session*.
3. **Decide the epoch.** If a handover trigger fired (§6) and this is a free
   moment, open a new **epoch**. Bring the register up to date (Option A if
   the watermark lags), build the handoff packet, and persist the epoch and
   the packet's exact bytes. Otherwise keep the current epoch.
4. **Rewrite deterministically.**
   `upstream_request = f(client_request, epoch)`:
   - append the epoch's packet bytes to the end of the system prompt, if the
     epoch has a packet. A session's first epoch has none (D4);
   - at an epoch change only, optionally drop tool definitions that an active
     hard item revokes. The cache is lost anyway;
   - set the affinity hint (`X-Session-ID: <session key>`) and pin `model` to
     the epoch's model.

   The same input must produce the same bytes, even after a proxy restart.
   That is why the packet bytes are stored in Positorium.
5. **Forward and tee.** Stream SSE back to the client as it arrives. Text and
   reasoning deltas pass through immediately. Tool-call deltas are buffered
   per call until complete, then checked by the enforcer (§10).
6. **Record.** At stream end, write the turn anchor (index, digest, time), the
   served model if it changed, and any enforcement decisions. These are small
   asynchronous writes.
7. **Schedule extraction.** If the turn yields to the human
   (`finish_reason != "tool_calls"`) and there is unextracted human-authored
   or assistant-commitment content, schedule Option B (§7.2).

## 6. Handover triggers

| Trigger | Detected by | KV cache | Action |
| --- | --- | --- | --- |
| Harness switches model | `request.model` ≠ epoch model | lost | New epoch with a packet. Option A first if the watermark lags |
| Proxy policy (escalation, drain, fallback) | the proxy chooses `model` | lost | Same |
| Next hop substituted the model | `response.model` ≠ pinned model (or configured header) | already lost | Record an *unplanned handover*. Enforcement still applies. New epoch on the next request |
| Weights swapped under the same name | served-name convention (`name@version`) or `/v1/models` `root` change | lost | Same as harness switch |
| Harness compaction | same session key, digest chain breaks before the first human turn | lost | New epoch with a packet. Items from dropped turns survive only through the register |
| Rewind / edit / fork | common prefix ends after turn *k* < known head | partly kept | New branch Thing that inherits items anchored at or before *k*. New epoch |
| Proxy restart | none | kept | Reload the session cache from Positorium and re-apply the stored packet bytes. Nothing changes upstream |
| Worker failover, same model | invisible (router) | lost on that worker | Nothing: same model, conversation intact |

## 7. Register lane

### 7.1 Deterministic events (no LLM)

These come from structured protocol traffic, via harness profiles:

- a tool appears in or disappears from `tools[]` (the harness changed
  permissions). The positor is the harness;
- a tool result signals denial, e.g. a harness-specific "user rejected" or
  "permission denied" pattern. The positor is the developer, with a verified
  structural source;
- a system or developer instruction diff (project rules changed);
- plan or todo tool calls (`update_plan`, `TodoWrite`, ...), recorded as
  *goal* candidates from an advisory source.

### 7.2 Option B: background extraction on the hot KV cache

- **When:** the session yields to the human (`finish_reason ∈ {stop, length}`).
  Debounce by about 1.5 s. Cancel if a new request for the session arrives,
  since that request carries the new turn, and reschedule after it. Fast tool
  loops never trigger extraction.
- **What:** send to the same downstream:
  `upstream_messages(turn N)` + the streamed assistant reply + one extraction
  message containing the current register (short item handles, statuses) and
  instructions. The bytes up to the reply equal what the worker just
  processed, so APC hits it. Only the extraction tail (a few hundred tokens)
  is prefilled.
- **Routing to the hot worker:** forward the same `X-Session-ID`. With
  vllm-router `consistent_hash` this is deterministic, because `X-Session-ID`
  is its first-priority hash key. With `cache_aware` the long shared prefix
  routes it to the same worker unless the router is in imbalanced mode. See §12.
- **Output:** `response_format: json_schema`, `temperature: 0`, small
  `max_tokens`. Schema (abridged):

  ```json
  {"deltas": [{
    "op": "add|restate|supersede|revoke|reaffirm|dispute",
    "item": "new|<handle>", "replaces": "<handle>|null",
    "kind": "permission|prohibition|commitment|goal|role_boundary",
    "strength": "hard|graded",
    "statement": "...",
    "enforce": {"ops": ["write"], "paths": ["migrations/**"]},
    "source_message": 17, "quote": "exact words from that message",
    "confidence": 0.9
  }]}
  ```

- **Measure it:** record `usage.prompt_tokens_details.cached_tokens` (vLLM
  needs `--enable-prompt-tokens-details`; verify). This shows directly whether
  Option B hit the cache on the Sparks.
- **Contention:** cap background concurrency globally, at about one per Spark.
  Use vLLM `priority` scheduling for background requests if available (verify).
  Skip extraction while the downstream signals overload (HTTP 429/503).

### 7.3 Verification before writing

The model's output is a proposal, not a fact. The proxy checks:

1. `quote` occurs verbatim in message `source_message`, after whitespace
   normalization. Otherwise downgrade to `uncertain` with an advisory positor.
2. The span containing the quote is labeled `human` (or `system`/`harness`
   for project rules). Text in tool results or injected reminders never
   counts as the developer, which blocks prompt injection through fetched
   pages or logs.
3. The trust policy (D7). Loosening ops (`revoke` of a prohibition, a new
   permission, `supersede` that weakens) need checks 1–2 to pass with an
   authoritative source. Tightening ops are accepted with any confidence.
   Low-confidence ones become `uncertain`, which enforces as *hold*.
4. `enforce` compiles (globs and regexes). If not, the item is prompt-only.
5. Writes are idempotent. Re-adding the same proposition returns the same
   posit, so retries after partial failure are safe.

### 7.4 Option A: synchronous fallback at a boundary

If a boundary is crossed while `extracted_through < head`, run the same
extraction synchronously before building the packet, with a deadline of about
3–5 s. Use the outgoing model if it is still reachable (planned switch, hot
cache). Otherwise use the incoming model on the request's own history. On
timeout, the packet says "register may be incomplete for turns k..N". The
audit's readiness is then not met, so verdicts are `indeterminate` (§11).

### 7.5 Option C: sidecar extractor (optional)

The `extractor` upstream is configurable: `same_route` (Option B, default) or
`dedicated {url, model}`, for a small model reading only
`[register + last exchange]`. Use it where background calls must not touch the
serving workers, or where the next hop has no affinity.

### 7.6 Explicit decisions

An internal admin endpoint, `POST /v1/sessions/{key}/items`, lets harness hooks,
an MCP server or a human record decisions directly as an authoritative
positor. This bypasses prose extraction for important choices.

## 8. Positorium data model

### 8.1 Things and roles

Shared role names are reused across classes, following the cookbook's
anti-pattern advice. `evidence`, `violations` and similar words are Traqula
keywords inside constraint bodies, so they are avoided as role names.

Three Traqula facts shape this table (spike S2, S7, S8):

- appearances accept only script variables, not `$parameters`;
- query variables are lexical, so a lookup cannot correlate with an earlier
  binding;
- a lookup must match the stored appearance set exactly.

So **every Thing a later script must reference carries a unique literal key**
in a class-specific unary role, and writes resolve it with a self-anchored
keyed get-or-create (rules R1–R4 in the findings). Turns are never lookup
targets.

| Appearance set | Value | Time | Meaning |
| --- | --- | --- | --- |
| `{(s, session_key)}` | `"7f3c…"` | first seen | external session identity (key) |
| `{(s, session), (d, developer)}` | `"owner"` | first seen | who owns the session. **Audits use this as the authority** |
| `{(s, harness)}` | `"claude-code"` | first seen | harness profile |
| `{(b, branch_key)}` | `"7f3c…#0"` | fork time | branch key (R1) |
| `{(b, branch), (s, session)}` | parent turn index | fork time | branch of a session (root branch = index 0) |
| `{(t, turn), (b, branch)}` | `12` | turn arrival | turn anchor. Write-only, human-facing turns and epochs only (R4, R7) |
| `{(t, digest)}` | `"sha256:…"` | turn arrival | chain digest at that turn |
| `{(b, served_model)}` | `"qwen3-coder@2026-11"` | on change only | which model answered |
| `{(i, item_key)}` | `"7f3c…#P3"` | introduction | item key (R1) |
| `{(i, item), (b, branch)}` | `"registered"` | introduction | **assertion target** for the item |
| `{(i, kind)}` | `"prohibition"` | introduction | paper role |
| `{(i, statement)}` | `"Do not trigger a production deployment"` | per wording | rewording appends; history kept |
| `{(i, strength)}` | `"hard"` | introduction | hard / graded |
| `{(i, enforce)}` | `{"ops":["exec"],"commands":["deploy.*prod"]}` | introduction | machine-checkable scope (JSON) |
| `{(i, cited_turn)}` | `{"turn":12,"quote":"no prod deploys from here"}` | turn time | provenance span. The turn index is a value, not a lookup (R4) |
| `{(n, replacement), (o, replaced)}` | `"supersedes"` | change time | supersession link |
| `{(a, complement), (c, complement_of)}` | `"requires"` | introduction | all-or-nothing pairs (paper §6.1) |
| `{(p, name)}`, `{(p, authority)}` | `"developer:42"`, `"authoritative"` | — | positors (the name is the key). `authority` is resolved in Rust from a cached map, never joined in a query (finding 2) |
| `{(e, epoch_key)}` | `"7f3c…#e2"` | epoch start | epoch key (R1) |
| `{(e, epoch), (b, branch)}` | `"model_change"` | epoch start | handover epoch |
| `{(e, to_model)}` | `"…"` | epoch start | successor model |
| `{(e, packet)}` | exact packet text | epoch start | bytes re-injected all epoch (exact round-trip verified, S6) |
| `{(e, packet_of), (i, packet_item)}` | `"hard-active"`, `"revoked"`, ... | epoch start | what the packet cited, and as what |
| `{(x, action), (b, branch)}` | `{"turn":44,"op":"write","paths":[…]}` | turn time | a proposed tool call |
| `{(x, decision)}` | `"forwarded"`, `"blocked"`, `"held"` | decision time | enforcement result |
| `{(x, action), (i, matched_item)}` | `"match"` | decision time | which item matched |
| `{(r, extraction), (b, branch)}` | `{"through":12,"model":"…","cached_tokens":81234,"ms":640}` | run time | extraction run; also the readiness *delivery* (§11) |
| `{(b, extracted_through)}` | `12` | run time | watermark, read with `as of` |

### 8.2 Assertions and derived status

Sources hold positions on the item's membership posit
`m = {(i, item), (b, branch)} → "registered"`:

- the developer says "don't deploy to prod": developer asserts `m` at **+100%**
  at the turn's time;
- the developer later lifts it: developer asserts `m` at **0%** (withdrawn).
  The assertion slot `{(m, posit), (developer, ascertains)}` keeps both, and
  `as of` on that slot returns the latest, 0%;
- the developer rejects a model-proposed commitment: developer asserts the
  model's item at **−100%**;
- **extraction provenance is a meta-assertion**: the extractor asserts the
  developer's *assertion posit* at its confidence, e.g. 85%, at extraction
  time. Assertions are posits, so they can be asserted. This keeps "who holds
  the constraint" (developer) separate from "who claims they hold it"
  (extractor) and when the proxy learned it. The result is bitemporal, which
  lets an audit ask "what could the proxy have known at the handover?"

Status is computed in Rust from rows read at a cut T. It is a versioned
application policy, which matches Positorium's stance that preference is
query policy:

| Status | Rule (authoritative = positor with `authority = "authoritative"`) |
| --- | --- |
| active | latest authoritative certainty > 0, no authoritative opposition, not replaced |
| revoked | an authoritative source was > 0 earlier and is now 0 or < 0 |
| superseded | the target of a `replacement` link whose new item is active |
| disputed | effective positions of opposite sign that policy cannot rank (two authoritative sources, or advisory sources with no authoritative position), or equal-time conflicting statements |
| uncertain | only advisory positors, or the extractor's meta-certainty is below threshold θₑ |

### 8.3 Traqula (validated)

Record a verified extraction delta (spike R6). `$turn_time` is the developer's
turn and `@NOW` is extraction time. The lookups are keyed get-or-creates, each
anchored by its own literal. The item command is one atomic batch. The
meta-assertion needs a second command, because a posit binding is only
recallable in a later command. About 30 ms in total (two fsyncs).

```traqula
search [{(?dev, name)}, $developer, *] or add posit [{(+dev, name)}, $developer, @NOW];
search [{(?ext, name)}, $extractor, *] or add posit [{(+ext, name)}, $extractor, @NOW];
search [{(?b, branch_key)}, $branch_key, *] or add posit [{(+b, branch_key)}, $branch_key, @NOW];
add posit +m [{(+i, item), (b, branch)}, "registered", $turn_time]
            and assert +a by dev with 100% at $turn_time,
          [{(i, item_key)}, $item_key, $turn_time],
          [{(i, kind)}, "prohibition", $turn_time],
          [{(i, statement)}, $statement, $turn_time],
          [{(i, strength)}, "hard", $turn_time],
          [{(i, enforce)}, $enforce_json, $turn_time],
          [{(i, cited_turn)}, $cite_json, $turn_time];
add posit [{(a, posit), (ext, ascertains)}, 85%, @NOW],
          [{(b, extracted_through)}, $through, @NOW];
```

To assert on an existing item later (revoke, reaffirm, dispute), resolve it
by `item_key`. Then **re-add its identical membership proposition** to rebind
`+m`: same branch, value and introduction time. This returns the canonical
posit and creates nothing (S5).

Read a branch's register at cut `$t` (2.7 ms at 1M posits). The anchor is the
key literal, and authority is resolved in Rust:

```traqula
search [{(?b, branch_key)}, $branch_key, *],
       ?m = [{(?i, item), (?b, branch)}, "registered", ?introduced],
       [{(?i, item_key)}, ?key, *],
       [{(?i, kind)}, ?kind, *] as of $t,
       [{(?i, strength)}, ?strength, *] as of $t,
       [{(?i, statement)}, ?text, *] as of $t,
       ?a = [{(?m, posit), (?src, ascertains)}, ?certainty, ?asserted] as of $t
return ?key, ?kind, ?strength, ?text, ?src, ?certainty, ?asserted;
```

Two shapes that work but do not scale. Filtering the branch with
`where ?b = $branch` took 7.7 s at 100k posits. Joining
`[{(?src, authority)}, ?authority, *]` took 367 ms at 1M. The planner orders
patterns by structural candidate count and ignores literal selectivity.

### 8.4 Running Positorium inside the proxy

- **One owner.** The proxy process owns `register.store`. The crate is added
  with `--no-default-features --features persistence`. Share one
  `Arc<Database>`.
- **Writes** go through one dedicated writer thread fed by a channel. The
  crate API is synchronous. Each mutation command is one durable commit:
  13–15 ms on the M1 Pro dev machine at any store size, with Linux/arm64 yet
  to be measured. An extraction is 2 commits (about 30 ms), so one writer
  handles about 30 extractions/s. The expected load is roughly one per
  human-facing turn. Add group commit only if Linux fsync is much slower.
- **Reads** run in `spawn_blocking` with `ExecutionOptions { timeout,
  query_limits, now }`. Read-only scripts get a consistent snapshot and run
  concurrently with the writer: 2.9 ms p50 at 1M posits while commits run.
  Every read is anchored on a key literal (§8.3).
- **Caches** (session chain head, epoch packet bytes, compiled enforcement
  rules, positor → authority map) are derived. On a cache miss after a
  restart, rebuild from Positorium.
- **Backups** use `positorium-store backup` against a stopped writer, or a
  scheduled maintenance window. The crate builds the binary for linux/arm64
  from source. Release archives cover only x86-64 Linux, macOS and Windows.
- **Size and retention.** Measured cost is about 1.2 KB resident and 7.7 µs of
  reopen time per posit: 1M posits means 1.2 GiB RSS and an 8 s start.
  Positorium is append-only with no delete, so **rotate the live store at
  about 1M posits or monthly**, whichever comes first. Carry still-active
  sessions forward with `dump`/`import`; keys are literals, so they survive
  identity remapping. Write turn anchors only at human-facing turns and
  epochs, which cuts posit volume several-fold. Run the proxy on its own host,
  not in a Spark's unified memory.
- **Scaling.** One process handles thousands of concurrent SSE streams. If one
  is not enough, shard sessions across proxy instances, each with its own
  store, behind an L7 hash on `X-Session-ID`. Positorium itself stays
  single-owner. There is no replication: HA is active/passive with backups.
- **Exploration.** Positorium's HTTP server cannot open a store the proxy owns.
  Open a backup copy in Query Studio, or use the proxy's internal read-only
  Traqula endpoint, which enforces budgets.

## 9. Handoff packet

Rendered deterministically from the register at the epoch's cut. It is
appended to the system prompt inside a delimited block and stored verbatim in
`{(e, packet)}`. The order follows paper §6.4: hard invariants and unresolved
goals come before anything stylistic.

```text
<handover-register epoch="e-1832" from="model:qwen3-coder@2026-10" register-complete-through="turn 41">
You are continuing an existing session. These items govern it. Earlier
turns may be missing from your context; this register is authoritative.

HARD — in force (do not violate; ask before any action near them)
  [P1] Do not trigger a production deployment. (developer, turn 12: "no prod deploys from here")
REVOKED / SUPERSEDED — do not resume
  [P2] ~~May run database migrations~~ revoked by developer, turn 30
  [C3] ~~Rewrite retry logic as a queue~~ superseded by C5
UNRESOLVED — confirm with the developer before relying on these
  [G4] Use asyncpg (proposed by previous model; disputed by test output, turn 38)
COMMITMENTS AND GOALS
  [C5] Fix duplicate retries within the existing retry module.
  [G6] Acceptance: tests/integration/test_payment_retry.py passes.
ATTRIBUTION
  Work listed above was performed by the previous model. Do not claim to
  remember running tools that you cannot see in this context.
</handover-register>
```

Complementary pairs (`requires`) are always emitted together or not at all.
A token budget truncates graded items first, never hard ones.

## 10. Enforcement

- **Harness profiles** map tool calls to normalized actions
  `{op: read|write|exec|network|other, paths, command, tool}`: Claude Code
  `Bash`/`Edit`/`Write`/`Read`, Codex `shell`/`apply_patch` (paths parsed
  from patch headers), OpenCode `bash`/`edit`/`write`, and MCP tools as
  `other` plus the tool name. Profiles are TOML, with code for the few
  formats that need parsing.
- **Matching:** compiled `enforce` scopes from items that are **hard, active
  or uncertain, and authoritative or tightening**. Globs and regexes are
  compiled once per register change. There is no Positorium query per tool
  call.
- **Streaming:** buffer each tool call's deltas until the call completes
  (the next index starts, or `finish_reason` arrives), then decide:
  - `forward`: emit the buffered deltas unchanged;
  - `block` (active hard item): drop that call and emit a short assistant
    text naming the item ("Blocked by session constraint P1: no production
    deployments. Ask the developer if this has changed."). If no calls
    remain, finish with `stop`;
  - `hold` (uncertain or disputed hard item): the same, phrased as the one
    targeted confirmation question the paper recommends;
  - `warn` (configurable per kind): forward, and record.
- **Record** every decision as `action`, `decision` and `matched_item` posits.
  Audits depend on this record.

## 11. Audit and session status

Saved Traqula constraints (`add constraint` / `check constraint`) make the
proxy's handover duties checkable, and every check comes with witnesses and
an input manifest. Validated form, anchored on the key literal, with the
session owner as authority:

```traqula
add constraint "Handoff packets cite every active hard item" {
    search [{(?s, session_key)}, $session_key, *],
           ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?s, session), (?src, developer)}, "owner", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
    where ?introduced < ?at and ?c > 0%
    return ?e, ?i, ?ep, ?m;
};
```

Constraints:

1. Every packet cites every hard item active at its epoch (above; validated).
2. Every hard item revoked before an epoch is cited as `revoked`. This is the
   paper's stale-summary failure (validated).
3. No action was forwarded while it matched a hard item active at that time.
   This checks consistency between recorded decisions and register state, and
   catches races and policy bugs (validated).
4. Every `requires` pair is cited together or not at all.
5. Every loosening change has an authoritative positor with a cited quote (D7).

Harness-sourced authority (project rules from system prompts) needs a second
`union` branch. Not yet exercised.

**Readiness is required, not optional.** Any finding query that uses `as of`,
`in effect`, `latest` or `not exists` reports `indeterminate` until a readiness
declaration covers the check. Every audit above uses `as of`. A declaration is
tied to one constraint revision and one parameter scope: its value must be
exactly `{"ready": {"parameters": {"session_key": …}, "clock": null}}`. When
an extraction run brings the watermark to the epoch's turn, the proxy writes a
manifest entry with the run posit as its `delivery`, plus one declaration per
audit revision. Before that, checks honestly return `indeterminate`.

**Cost.** One check takes 17 ms at 100k posits and 206 ms at 1M. Every report
embeds the full store manifest: 0.8 MiB at 100k, 9.2 MiB at 1M. `record by`
stores that report, and the 16 MiB frame limit is reached at about 1.8M posits.
So: run audits at epoch changes, not continuously. Record (`record by`) only
non-`satisfied` verdicts, or write a compact summary posit (verdict, findings,
report digest) instead. Store rotation (§8.4) keeps reports small.

**Session status** (paper §6.4), exposed as the `x-handover-status` response
header, on the admin API and in metrics:

- green: all checks satisfied, watermark at head;
- amber: indeterminate checks (watermark lag, uncertain or disputed hard
  items, weak session identity), or a hold is pending;
- red: a violated check, or an unplanned handover with a hard item that could
  not be cited.

Run checks after each epoch change and on a schedule. With `record by`, the
proxy also saves the report as an audit posit.

## 12. Router-agnostic downstream

**What the next hop must provide:** OpenAI `POST /v1/chat/completions` with SSE,
and it must honor `model`. That is all. Everything else degrades gracefully:

| Next hop | Affinity for Option B | Model change detection | Notes |
| --- | --- | --- | --- |
| vllm-router `--policy consistent_hash` | **Deterministic**: `X-Session-ID` is its first hash key (then `X-User-ID`, ..., body `user`, finally a hash of the body) | pinned `model` + `response.model` | Recommended. If the target worker is unhealthy it falls back to the first healthy worker. That is a cache miss, not an error |
| vllm-router `cache_aware` (its default) | Probable: approximate per-worker, per-model character-prefix tree with a match-ratio threshold. Imbalanced mode routes to the least loaded worker instead | same | Option B still works, and costs a full prefill on a miss. `cached_tokens` shows it |
| vllm-router `sticky_least_loaded`, `rendezvous_hash`, `kv_aware` | verify in Tier 1 | same | Listed in the CLI. Behavior to characterize |
| SGLang model gateway, Dynamo KV router, llm-d | prefix/KV-aware, expected to work | same | Same request shape |
| LiteLLM or another model-choosing router | may vary | **post hoc** via `response.model` | Prefer pinning the model in the proxy. Otherwise unplanned handovers are handled on the next request, and enforcement covers the gap |
| Bare vLLM (single worker) | trivially sticky | same | Simplest Tier 1/2 setup |
| Plain L4/round-robin balancer | none | same | Configure `extractor = dedicated` (Option C) or Option A only |
| Cloud OpenAI-compatible endpoint | provider prompt caching | same | Useful for Tier 3 development only |

**Affinity strategies** are configured per upstream: `header` (default, sends
`X-Session-ID`), `prefix` (no header, relies on identical bytes) or `none`.

**Not a vllm-router plugin.** Its WASM middleware is `OnRequest`-only, runs
under a 100 ms deadline, and fails closed. It cannot see responses, keep
state or make calls, so the register cannot live there. This confirms the
choice of an external proxy (Path 1).

## 13. Harness-facing protocols and session identity

| Harness | Wire protocol | Session key | Notes (verify per harness version) |
| --- | --- | --- | --- |
| OpenCode | Chat Completions | header via provider options | first adapter, passthrough |
| Claude Code | Anthropic Messages | `ANTHROPIC_CUSTOM_HEADERS="X-Session-ID: …"`, or the session part of `metadata.user_id` | Tool results arrive as `tool_result` blocks inside *user* messages, and reminders are injected into user text. The profile must label these as non-human |
| Codex | Responses (stateless `store: false`) or Chat | `http_headers` in the provider config | Injected environment context needs labeling. Reject `previous_response_id` (stateful) in v1 |

Ingress adapters convert to the canonical conversation (for inspection,
digests and extraction). Forwarding passes the original protocol through
when the next hop supports it. Otherwise it translates to Chat Completions,
since vllm-router documents only `/v1/chat/completions`. Translation is
Phase 6. v1 asks harnesses to use Chat Completions where they can.

Without an explicit or native session key, the fallback key is
`H(developer ‖ system prompt ‖ first human message)`. It does not survive
compaction, so such sessions are always amber.

## 14. Repository layout

```text
handover/
  Cargo.toml                 # workspace; rust-toolchain pinned (Positorium is edition 2024)
  crates/
    handover-core/           # canonical conversation, digest chain, triggers, epochs,
                             # status policy, packet rendering, enforcement matcher
    handover-protocol/       # Chat / Messages / Responses adapters, SSE parse+emit, tee
    handover-register/       # Positorium schema bootstrap, typed writes, queries,
                             # saved constraints, writer thread
    handover-proxy/          # bin: axum/hyper server, forwarder, extractor, admin API, metrics
    handover-mock/           # bin: scripted OpenAI-compatible worker (SSE, tool calls,
                             # canned extraction JSON, cached_tokens simulation, faults)
    handover-eval/           # bin: scenario runner, paper-style metrics, packet strategies
  spike/                     # Phase 1 Positorium spike: tests, benchmarks, FINDINGS.md
  profiles/                  # harness profiles (TOML)
  scenarios/                 # conversation fixtures incl. the paper's coding case
  deploy/                    # compose files, arm64 Dockerfile, air-gap bundle script
```

Main dependencies: `tokio`, `axum`/`hyper`, `reqwest` (rustls), `serde_json`,
`sha2`, `globset`, `regex`, `tracing`, `metrics`, and
`positorium = { version = "=0.3.1-beta.6", default-features = false, features = ["persistence"] }`.

## 15. Configuration sketch

```toml
[listen]
addr = "0.0.0.0:8443"

[store]
path = "/var/lib/handover/register.store"

[[upstream]]
name = "fleet"
base_url = "http://vllm-router:8000"
affinity = { kind = "header", name = "X-Session-ID" }
models = ["qwen3-coder@2026-11"]

[extractor]
mode = "same_route"            # or "dedicated" with url/model (Option C)
debounce_ms = 1500
max_concurrent = 4
deadline_ms_on_handover = 4000 # Option A
min_meta_certainty = 0.7       # below → uncertain

[policy]
authoritative = ["developer", "harness"]
enforce_kinds = ["prohibition", "role_boundary"]
on_uncertain_hard = "hold"
packet_strategy = "status_aware_core"   # eval: surface_rich | chronological | none

[[developer]]
token_sha256 = "…"
label = "developer:42"
```

## 16. Development and testing (connected side vs. air gap)

About 90% of the work needs no Spark, because every component speaks
OpenAI-compatible HTTP/SSE.

**Tier 1: deterministic, seconds (coding agents iterate here).**
`docker compose -f deploy/dev.yml up` starts the proxy, a **real
vllm-router** (multi-arch image), and three `handover-mock` workers. The mocks:

- stream realistic SSE: content, reasoning, tool calls, `finish_reason`;
- log `(worker, X-Session-ID, prefix hash)` at `/_mock/log`, so tests can
  assert stickiness;
- simulate a prefix cache and report `usage.prompt_tokens_details.cached_tokens`;
- answer extraction requests with scripted deltas, including malformed,
  injected and misattributed ones;
- support fault injection: kill, slow, 429/503, rename the served model.

Tests: byte-stability (golden upstream requests across restarts), digest
chain classification, Option B timing and cancellation, verification rules,
streaming enforcement, packet rendering, Positorium round-trips on a temp
store, saved-constraint verdicts. Property tests: `f(request, epoch)` is
deterministic, and the identity holds when there is no epoch packet.

**Tier 2: real small models.** Replace the mocks with two small
OpenAI-compatible servers (e.g. llama.cpp `llama-server`, or vLLM on CPU)
running different models A and B. Validate on real templates:

- the extraction request shape: one that renders an **identical prefix**
  (tools present, a `tool_choice` that does not change rendering) *and* gives
  schema-valid JSON. Check `cached_tokens`;
- reasoning-content stripping in templates, which shifts the cached prefix;
- extraction quality on the scenario corpus;
- the A→B packet effect.

**Tier 3: real harnesses.** Point Claude Code, Codex and OpenCode (custom base
URL plus the session header) at the proxy, with cloud stand-ins behind it.
Use synthetic repositories only. Check fast tool loops, mid-session switches,
compaction, rewind, and the developer experience of blocks and holds.

**Crossing the air gap.**

- Build `linux/arm64` images on the connected side
  (`docker buildx build --platform linux/arm64`). Positorium compiles from
  source inside the builder. Also build `positorium-store` for backups.
- `cargo vendor` plus a source-replacement config gives reproducible offline
  rebuilds. Pin vllm-router's arm64 image digest.
- Smoke-test the arm64 stack under emulation, then
  `docker save … | zstd > bundle.tar.zst`, and `docker load` inside the enclave.

**Validate on the Sparks** (cannot be simulated): guided JSON reliability of
the chosen model under vLLM's structured-output backend; APC hit rate and
contention of Option B on the GB10's unified memory (tune `--max-num-seqs`,
debounce, background concurrency); tool-call and reasoning parser quirks of
the exact vLLM version and chat template; router affinity under real load.

## 17. Roadmap

Each phase ends with something runnable and an exit check.

| Phase | Deliverable | Exit criteria |
| --- | --- | --- |
| 0 | Workspace, `handover-mock`, transparent Chat Completions proxy with SSE tee, Tier 1 compose with vllm-router | Byte-identical passthrough. Added p50 latency below 1 ms per chunk. Stickiness visible in mock logs |
| 1 | **Positorium spike** ✅ (`spike/`, [FINDINGS](spike/FINDINGS.md)): semantics validated, sketches corrected, benchmarks to 1M posits. Rotation set at about 1M posits or monthly. *Remaining:* Linux/arm64 fsync, reopen and RSS; branch-inheritance queries; harness authority in audits | Done on macOS. Linux numbers come with the Phase 0 compose stack |
| 2 | Sessions, digest chain, turn anchors, served-model tracking, trigger detection (§6) | Scenario tests classify continuation, compaction, branch and switch correctly. Restart rebuilds state |
| 3 | Deterministic events, admin decision API, status policy, packet rendering, epochs with byte-stable re-injection | Golden packets. Upstream bytes are stable across a restart within an epoch |
| 4 | Option B extractor with verification. Option A fallback. Optional Option C | Mock suite passes, including injection and misattribution cases. Tier 2: cache hit on extraction confirmed by `cached_tokens` |
| 5 | Enforcement: harness profiles, streaming buffer, block/hold/warn, action records | The paper's coding case: the successor's production-deploy tool call is blocked after a model switch |
| 6 | Audits: saved constraints, readiness from the watermark, green/amber/red, metrics | All five constraints give the expected verdicts on fixtures, including `indeterminate` under watermark lag |
| 7 | Protocol adapters: Anthropic Messages, stateless Responses, translation to Chat | Claude Code and Codex sessions work end to end in Tier 3 |
| 8 | Evaluation harness: packet strategies matching the paper's four conditions, violation scoring from sandbox action traces | Reproducible report on the scenario corpus |
| 9 | Spark deployment and tuning (§16) | The on-Spark validation checklist passes |

## 18. Open questions and risks

- **Session identity** depends on harness configuration. Without it,
  compaction looks like a new session. Mitigation: ship per-harness setup
  snippets, and flag weak-identity sessions amber.
- **Human vs. injected text** inside "user" messages differs per harness and
  version. Quote verification is only as good as span labeling. Profiles need
  regression fixtures captured from each harness.
- **Extraction quality** on the local open-weight model. D7 and D8 limit the
  damage. Measure precision and recall of hard items in Tier 2 and Phase 8.
- **Prefix identity** for Option B (tools, `tool_choice`, `response_format`,
  reasoning stripping) is template-specific. Verify in Tier 2 and on the Sparks.
- **Positorium beta**: format and language may change across betas (exact
  version pin; `dump`/`import` for migration). Single owner process. No delete,
  so rotation is needed. Fsync per command.
- **Query-shape sensitivity** (spike): the planner ignores literal selectivity.
  An unanchored or authority-joined query can be 100–3000× slower than its
  anchored form. Keep all Traqula in `handover-register`. Add a regression
  benchmark that fails if any production query exceeds its budget at 1M
  posits. The upstream fixes are listed in the spike's feedback section.
- **Search-or-add hazards** (spike S7, S8): silent fan-out across sessions and
  silent junk Things from partial patterns. Generate every write from typed
  Rust builders that only emit self-anchored unary-key lookups, and test that
  each write changes exactly the expected number of posits.
- **Unplanned model substitution** by a model-choosing next hop can't be
  prevented, only detected. Enforcement covers the gap for machine-checkable
  items.
- **Privacy**: the register stores verbatim quotes of developer prose. Define
  retention and access, and keep the admin API internal.
- **Branch inheritance** needs ancestor walking, which Traqula leaves to the
  client. Walk branch parents in Rust; the depth is shallow.

## 19. References

- Rönnbäck, L. (2026). Bounds on user repair after context loss in AI handoffs.
  *Discover Informatics*. https://link.springer.com/article/10.1007/s44564-026-00035-w
- Positorium 0.3.1-beta.6: https://github.com/Roenbaeck/positorium — see
  `docs/guides/RUST.md`, `docs/reference/TRAQULA.md` (assertions, `in effect`,
  saved constraints), `docs/design/CONSTRAINTS.md`, `docs/guides/OPERATIONS.md`.
- vllm-router: https://github.com/vllm-project/router — `docs/load_balancing/README.md`,
  `docs/cli_reference.md` (policies, consistent-hash key order, WASM middleware limits).
- Design discussion: "LLM Load-Balancing and Model Handover" (Google AI Studio,
  2026-11-20). Sections 7–12 adopt its Option B and Path 1. Its Redis/Postgres
  hybrid and Python mocks are replaced per D1/D2.
