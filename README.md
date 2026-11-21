# handover

Ensure proper LLM handovers using a constraint register.

`handover` is a Rust proxy in front of any OpenAI-compatible next hop (vllm-router,
SGLang gateway, LiteLLM, a bare vLLM, a cloud endpoint). It keeps the permissions,
prohibitions, commitments and goals a developer set during a coding session alive
when the answering model changes, as described in
Rönnbäck (2026), *Bounds on user repair after context loss in AI handoffs*. The only
durable store is [Positorium](https://github.com/Roenbaeck/positorium).

What it does:

1. **Keeps a constraint register per session** in Positorium. Every item has a
   source, time, signed certainty and kind. Its status is derived, never stored:
   active, revoked, superseded, disputed or uncertain.
2. **Extracts in the background on the hot KV cache** (Option B). When a turn
   yields to the developer, it sends the exact prefix the worker just served plus
   a short extraction tail.
3. **Injects a handoff packet only at a boundary**: a model change, compaction, a
   fork or rewind, or an unplanned substitution by the next hop. Sessions that
   never cross a boundary are forwarded byte-identically. Within an epoch the same
   packet bytes are re-injected, even after a restart.
4. **Enforces hard constraints on tool calls** in the stream: block, hold with one
   question, or warn, whatever the model does.
5. **Keeps subagents apart from the main conversation.** A subagent or side call
   runs in a delegated lane that the main register governs, without forking the
   session, opening epochs or breaking the main conversation's prefix cache.
   Nothing a parent model tells a subagent becomes a developer decision.
6. **Audits its own duties** with saved Traqula constraints (C1–C5) and exposes
   green/amber/red per session.

The design is in [PLAN.md](PLAN.md). The Positorium validation and benchmarks are in
[spike/FINDINGS.md](spike/FINDINGS.md).

## Layout

| Crate | Contents |
| --- | --- |
| `handover-core` | Canonical conversation and digest chain, request classification, status policy, packet rendering (four strategies), enforcement scopes, harness profiles, extractor-output verification, extraction prompt |
| `handover-protocol` | SSE framing, Chat Completions rewriting, streaming tee with per-call tool buffering, Anthropic Messages and OpenAI Responses adapters |
| `handover-register` | The register on Positorium: schema, one-command writes with identity parameters, branch-anchored reads, saved-constraint audits with readiness, restart snapshots |
| `handover-proxy` | The proxy binary: request lifecycle, epochs, Option A/B/C extraction, enforcement, admin API, metrics |
| `handover-mock` | A scripted OpenAI-compatible worker: SSE, tool calls, scripted extraction, prefix-cache simulation, faults |
| `handover-eval` | Scenario runner that scores packet strategies and enforcement |

`profiles/` holds harness profiles (Claude Code, Codex, OpenCode, generic),
`scenarios/` the evaluation scenarios, and `deploy/` the Dockerfile, the Tier 1
compose stack, configs and the air-gap bundle script.

## Build and test

```bash
cargo test --workspace
```

The end-to-end tests (`crates/handover-proxy/tests/e2e.rs`) run the proxy in front of
in-process mock workers. They cover the paper's coding case: a deploy permission is
revoked, then after a model switch the successor's deploy call is blocked and the
packet cites the revocation. They also cover:

- byte-identical passthrough when no boundary is crossed (body hashes compared at the worker);
- Option A at a boundary, and Option B reusing the hot prefix;
- holds for uncertain items;
- prompt injection through tool output;
- compaction and forks;
- subagent lanes, Claude Code's compaction requests and changed system prompts;
- tool removal, denials and plan tools;
- unplanned substitution and new weights under the same model name;
- dropping forbidden tools for a whole epoch;
- overload backoff, 3 MiB sessions, 24 concurrent sessions on a persistent store;
- the Messages and Responses ingresses;
- idle eviction and restart, with byte-identical packet re-injection that the worker's prefix cache confirms.

## Run locally

```bash
cargo run -p handover-mock -- --listen 127.0.0.1:9001 --worker w1
```

```bash
cargo run -p handover-proxy -- --upstream http://127.0.0.1:9001 --listen 127.0.0.1:8443 --store ./register.store
```

```bash
curl -s localhost:8443/v1/chat/completions -H 'X-Session-ID: demo' -H 'content-type: application/json' \
  -d '{"model":"model-a","messages":[{"role":"user","content":"Do not push to main.\n#mock:text OK."}]}'
```

```bash
curl -s localhost:8443/v1/sessions/demo
```

Lines starting with `#mock:` script the mock worker: `#mock:text`, `#mock:tool <name> <json>`,
`#mock:extract <delta-json>`, `#mock:fault 429|503|slow:<ms>|rename:<model>|drop`.
Without `#mock:extract` lines, the mock extracts simple "do not…" and "you may…"
sentences heuristically. Set `HANDOVER_MOCK_DUMP_DIR` to a directory to save every request body the
mock receives, which shows what a real harness sends through the proxy.

For production, use a config file (`handover-proxy --config handover.toml`). See
[deploy/handover.example.toml](deploy/handover.example.toml).

### Endpoints

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/chat/completions` | OpenAI Chat Completions ingress (stream and non-stream) |
| `POST /v1/messages` | Anthropic Messages ingress (Claude Code), translated to Chat Completions |
| `POST /v1/responses` | OpenAI Responses ingress (Codex, `store: false`), translated to Chat Completions |
| `GET /v1/models` | Passed through to the next hop |
| `GET /v1/sessions`, `GET /v1/sessions/{key}` (admin token) | Sessions, branches, epochs, register items with status, audits |
| `POST /v1/sessions/{key}/items` | Record an explicit decision as the authoritative developer, e.g. `{"op":"add","kind":"prohibition","strength":"hard","statement":"No force pushes","enforce":{"ops":["exec"],"commands":["push.*--force"]}}` or `{"op":"revoke","item":"P1"}` |
| `POST /v1/sessions/{key}/audit` | Run (and record) the audits now |
| `GET /v1/sessions/{key}/packet` | The current epoch's exact packet bytes |
| `POST /admin/traqula` | Read-only Traqula (search/explain) against the live store, with budgets (operator token) |
| `GET /metrics`, `GET /healthz` | Prometheus counters, liveness |

Responses carry `x-handover-status` (green/amber/red), `x-handover-session` and
`x-handover-epoch`, plus `x-handover-lane` for a subagent's or side call's request.

### Credentials

Each `[[developer]]` has two tokens:

- **`token_sha256`, the inference token.** The harness uses it for the model
  endpoints.
- **`admin_token_sha256`, the admin token.** It is the only token the
  `/v1/sessions…` endpoints accept. Keep it away from the harness: an agent
  holding it could record "developer" decisions and lift its own constraints.

`/admin/traqula` needs `operator_token_sha256`.

Session keys are namespaced per developer, so two developers sending the same
`X-Session-ID` get separate sessions. The client's `Authorization` is not
forwarded to the next hop unless `forward_client_auth = true`.

With no developers configured, every request is anonymous. The proxy then
refuses to listen beyond loopback unless `allow_anonymous = true` is set, as the
dev stack does. Session views stay open in that mode, but explicit decisions
need the top-level `admin_token_sha256`: the agent's shell can reach a loopback
listener as easily as the developer can.

Anything the developer's user account can read, the agent running under that
account can read too. Keep the admin token where the harness cannot read it,
for example under another user account.

### Harness setup

| Harness | Point it at the proxy | Session key |
| --- | --- | --- |
| OpenCode | custom OpenAI-compatible provider, base URL `http://proxy:8443/v1` | `X-Session-ID` header in the provider options |
| Claude Code | `ANTHROPIC_BASE_URL=http://proxy:8443` | `ANTHROPIC_CUSTOM_HEADERS="X-Session-ID: …"`, or the session part of `metadata.user_id` (automatic) |
| Codex | a model provider with `base_url = "http://proxy:8443/v1"`, `wire_api = "responses"` or `"chat"` | `http_headers = { "X-Session-ID" = "…" }` |

Send `X-Handover-Harness: claude-code|codex|opencode` (or set `default_profile`), so
tool calls are mapped to actions and harness-injected text is labeled correctly.
Without a session key, the proxy falls back to a weak key. A weak key does not
survive compaction, so those sessions are always amber.

## Evaluation

```bash
cargo run -p handover-eval -- scenarios/
```

The runner replays each scenario under the four packet strategies:
`status_aware_core`, `surface_rich`, `chronological` and `none`. It reports:

- hard items cited as in force;
- revoked or superseded items marked as such;
- stale items (presented without their status);
- actions that ended as expected;
- packet size;
- audit verdicts and the session's status.

Enforcement stops forbidden actions under every strategy. Only the status-aware
packet tells the successor what changed, and audit C2 catches the surface-rich
packet's stale summary.

Hot-path overhead, measured with a persistent store on an M1 Pro (`STORE` picks
a store directory, `SAME=1` reuses one session instead of opening a new one per request):

```bash
STORE=$(mktemp -d)/register.store SAME=1 cargo run --release -p handover-eval --bin latency
```

| Measure | Result |
| --- | --- |
| Added delay per SSE event | 1–6 µs |
| First event, continuing session | +0.19 ms |
| First event, a new session's first request | +25 ms (one durable commit) |

## Deploy

```bash
docker compose -f deploy/dev.yml up --build
```

This starts the Tier 1 stack: the proxy, a real vllm-router and three mock
workers. The router is built from its repository's Dockerfile unless
`VLLM_ROUTER_IMAGE` names a (pinned) image. `deploy/bundle.sh` vendors the crates,
builds the linux/arm64 images and writes one zstd bundle for the air gap.

## Status

Phases 0–8 of the [roadmap](PLAN.md#17-roadmap) are implemented and tested against
the mock (Tier 1). Claude Code 2.1.330 in print mode runs through the proxy,
including subagents, compaction and a model switch. Not yet done:

- Tier 2 (real small models), the rest of Tier 3 (interactive Claude Code, Codex,
  OpenCode) and on-Spark validation;
- Linux fsync and memory numbers.

## License

Dual-licensed under [Apache 2.0](LICENSE.Apache-2.0) or [MIT](LICENSE.MIT), at your option.

SPDX-License-Identifier: Apache-2.0 OR MIT
