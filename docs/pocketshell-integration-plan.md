# PocketShell integration plan and heru event-format alignment

Status: planning document, 2026-08-26. Nothing here is implemented; the aplexer repo currently
contains only `spec.md`. This document makes spec.md sections 22–23 (PocketShell integration,
engine/profile extraction) concrete against the *actual current code* of the two client repos,
and proposes adopting heru's `UnifiedEvent` envelope for the `a watch --jsonl` event stream
(spec.md section 19).

Sources read for this plan:

- `spec.md` in this repo (sections 1, 8, 9, 15–23, 27).
- PocketShell (Android): `/home/alexey/git/pocketshell` — Kotlin app + host-side Python CLI in
  `tools/pocketshell/src/pocketshell/`.
- PocketShell Desktop: `/home/alexey/git/pocketshell-desktop` — VS Code fork, TypeScript, backend
  logic in `src/**` mirrored into `extensions/pocketshell/src/backend/**`.
- heru: `https://github.com/alexeygrigorev/heru` (cloned at commit `7278523`, 2026-06-27) —
  `README.md`, `heru/types.py`, `heru/base.py`, `heru/profiles.py`, `tests/contract/`.

---

## Part 1 — Integration plan for pocketshell and pocketshell-desktop

### 1.1 What PocketShell (Android) actually does today

The important structural fact: **the Android app already delegates almost all agent logic to a
host-side Python CLI** (`pocketshell`, shipped from `tools/pocketshell/` in the same repo, itself
descended from an earlier `heru` host CLI). The phone holds a warm SSH lease and drives tmux via a
`tmux -CC` control-mode channel plus one-shot `exec` calls. Everything spec section 22 says
PocketShell should "stop owning" is concretely located as follows:

| Concern | Where it lives today |
| --- | --- |
| tmux session create/kill/list | `app/.../projects/FolderListGateway.kt` (2.5k lines); prefers `tmuxctl create-detached --mem` (cgroup memory caps), falls back to raw `tmux new-session -A -d` |
| tmux attach / live updates | `shared/core-tmux/.../TmuxClient.kt` — writes `tmux -CC new-session -A -s '<name>'` into an SSH shell, parses control-mode notifications |
| Session naming conventions | `app/.../sessions/TmuxSessionCreation.kt`, `SessionNameDerivation.kt` (sanitize, derive from folder basename, `-2`/`-3` collision suffixes) |
| Engine registry | `tools/pocketshell/src/pocketshell/engines.py` — `EngineManifest {id, family, harness, label, provider_mark, launch, usage_provider, enabled, available, ...}`, `LaunchSpec {argv, skip_permissions_argv, env_unset, env_set, profile_env, profile}`; built-ins `claude, codex, opencode, grok`; user overrides in `~/.config/pocketshell/engines.yaml`; a forced-union `PROVIDER_ENV_UNSET_VARS` list (~110 provider-key env vars) that custom engines cannot opt out of |
| Profile discovery | `tools/pocketshell/src/pocketshell/profiles.py` — `Profile {name, engine, config_dir, default, env}`; auto-discovery of `~/.<name>` dirs via marker files + name hints (`"laude"`, `"odex"`), merged with explicit `~/.config/pocketshell/profiles.yaml` |
| Launch construction | Phone builds the string `pocketshell agent <id> --dir '<dir>' [--no-skip-permissions] [--profile '<name>']` (`SessionTypePickerSheet.kt::buildRegistryAgentCommand`), tmux `send-keys` types it; host side `agents.py::launch_agent` does `build_env()` + `build_argv()` then `execvpe` |
| Agent identity | tmux user options `@ps_agent_kind`, `@ps_agent_profile`, written by `agents.py::record_agent_kind` |
| Agent semantic state | tmux user options `@ps_agent_state`, `@ps_agent_state_updated_at`, written by **agent hooks** installed by `hooks.py` (Claude `Stop`/`Notification` hooks, Codex `notify`, a generated OpenCode JS plugin — each shells `tmux set-option`); read back via batched `tmux list-sessions -F` and mapped in `SessionAgentState.kt` (`Idle / WaitingForInput / Working / Unknown` + staleness grace) |
| Structured host API | `tools/pocketshell/src/pocketshell/daemon.py` — Unix-socket JSON-RPC (`sessions.list`, `agents.kind_for_panes`, `tree.*`, `jobs.*`, `usage.fetch`, ...) with per-method TTL caches |

There is no reference to aplexer anywhere in the repo; `heru` appears only as vestigial naming
(DB columns `heruInstalled` / `heruLastDetectedAt`, historical docs note).

### 1.2 What PocketShell Desktop actually does today

The desktop fork is much less built out on the agent side, and *also* depends on the same remote
`pocketshell` host CLI (installed by `src/integrations/bootstrap/bootstrap-manager.ts`):

| Concern | Where it lives today |
| --- | --- |
| tmux lifecycle | `src/tmux/client.ts` (`TmuxClient`, speaks tmux control mode directly over an `ssh2` shell channel), `src/tmux-ui/tmux-session-manager.ts`; uses windows, panes, `splitWindow`, `capture-pane`, hex-chunked `send-keys -H` |
| Engine "registry" | **Hardcoded 3-value enum**: `src/agents/types.ts` `AgentType {claude, codex, opencode}` + `AGENT_METADATA {name, binary}`, with the same union type re-declared locally in at least four other modules |
| Profiles | **None.** Only two unused JSON blob columns (`claude_profiles_json` / `codex_profiles_json` in `src/ssh/data/host-store.ts`) that nothing parses |
| Launch construction | `src/sessions/create-session.ts::buildAgentStartCommand` → `pocketshell agent <binary> --dir <quoted>`; orchestration in `extensions/pocketshell/src/feature/sessions/session-launcher.ts` |
| State / attribution | Three ad-hoc mechanisms: tmux control-mode `%output` events; `ConversationAttributionService` running an inline `ps`/`pgrep` BFS shell script plus a PSV feed from `pocketshell agent-detections --psv`; and remote JSONL transcript tailing via `stat`+`dd` on a 2s `setInterval` (`src/agents/conversation/session-reader.ts`) |
| SSH transport | `src/ssh/connection/ssh-client.ts` (`ssh2`), warm connection pool; everything remote is one-shot `conn.exec("<shell string>")` |
| Session identity | tmux **session name** (also in the assistant tool surface `src/assistant/assistant-tools.ts`: `list_sessions`, `start_session`, `send_prompt_to_session`, ...) |

Structural caveat: the backend logic exists **twice** — canonical in `src/**` and a byte-identical
mirror in `extensions/pocketshell/src/backend/**`. Every integration change lands in both.

No references to `tmuxctl`, `aplexer`, or `heru` anywhere in the fork's own source.

### 1.3 Evaluating spec.md's Phase A ("Aplexer owns engines/profiles first, tmux still hosts terminals")

The spec's suggested first slice **still makes sense, and the codebases make it even cheaper than
the spec assumes** — with one refinement:

> **The natural integration seam is the `pocketshell` host CLI, not the two client apps.**

Both clients already funnel every agent-launch and most agent-metadata operations through remote
`pocketshell <subcommand>` invocations. Neither client constructs real agent argv itself; the
Android app's Kotlin engine/profile "gateways" are UI-facing mirrors of the host CLI's registry.
Therefore Phase A does not require touching the tmux/`-CC` code, the SSH transports, or the
terminal paths in either client at all. Aplexer can become authoritative for engines, profiles,
and launch preparation by slotting in *underneath* `pocketshell agent` / `engines list` /
`profiles list`, and both clients inherit it on the same day.

A second observation strengthens this: **Phase A needs none of aplexer's PTY/worker/cgroup
machinery.** The engine registry, profile discovery, launch-env preparation, and `--json` output
are pure metadata + process-exec logic. This slice can ship long before the persistent-PTY
milestones, which is exactly the right order given the Rust runtime is currently unwritten.

### 1.4 Phased plan

#### Phase 0 — aplexer prerequisites (this repo, Rust)

Build the minimum aplexer surface Phase A consumes. All of this is already in spec v1 scope
except one gap:

- `a engines --json` — port of `engines.py`: built-ins `claude/codex/opencode/grok`, TOML user
  overrides, availability detection, and — critically — the forced-union provider-key
  `env_unset` safeguard semantics from PocketShell (spec §8.2 mentions "provider-key environment
  stripping" but not the *cannot-opt-out* property; carry it over explicitly).
- `a profiles --json` — port of `profiles.py` discovery (marker files, sibling-dir name hints,
  explicit config wins collisions, never expose env/secrets in listings).
- **Gap — a launch-resolution command that does not create an aplexer session.** Spec §16 has
  `start` (creates an aplexer-hosted session) but Phase A launches inside *tmux*. Needed:
  something like `a launch-exec <engine> [--profile p] [--no-skip-permissions] [--dir d] `
  (resolve argv+env, `execvpe` in place — a drop-in for `agents.py::launch_agent`) and/or
  `a launch-spec ... --json` (print `{argv, env_set, env_unset, cwd}` for a wrapper to apply).
  Without this, Phase A cannot exist. Recommend adding it to spec §16.
- Config migration: a documented mapping (or one-shot converter) from
  `~/.config/pocketshell/{engines,profiles}.yaml` to aplexer's TOML (spec §8.3/§9.3 shapes are
  already near-isomorphic to `EngineManifest`/`Profile`).

#### Phase A — aplexer owns engines/profiles/launch; tmux still hosts terminals

Changes in `/home/alexey/git/pocketshell` (host-side Python only; **zero Kotlin changes** if
output shapes are preserved):

- `tools/pocketshell/src/pocketshell/agents.py::launch_agent` → thin shim that `exec`s
  `a launch-exec ...` (or applies `a launch-spec --json`). Keep writing `@ps_agent_kind` /
  `@ps_agent_profile` tmux options for now — the Kotlin readers still depend on them.
- `engines.py` / `profiles.py` list paths → delegate to `a engines --json` / `a profiles --json`
  (subprocess; the Python `aplexer` client package is *not* required for this).
- `daemon.py` TTL caches keep working unchanged on top of the delegated data.
- Later in the phase, optionally swap subprocess calls for the `aplexer` Python package once it
  exists (spec §21) — transport change only, no behavior change.

Changes in `/home/alexey/git/pocketshell-desktop`:

- For desktop this phase is a **fill-in, not an extraction** — it has no registry to extract.
  Replace hardcoded `AgentType`/`AGENT_METADATA` consumption in the launcher/creation flows with
  an engine list fetched via `conn.exec("pocketshell engines list --json")` (which Phase A makes
  aplexer-backed), and add a profile picker backed by `profiles list --json`. Consolidate the
  four duplicate local `AgentType` unions while touching them. Wire `--profile` through
  `buildAgentStartCommand` (`src/sessions/create-session.ts`, mirrored copy too).
- Delete or wire out the dead `claude_profiles_json`/`codex_profiles_json` columns.

Value delivered: one authoritative engine/profile/launch registry across Android, desktop, and
the CLI; desktop gains profiles for the first time; the provider-key safeguard is enforced in one
place; sets up the metadata model Phase B needs.

#### Phase B — aplexer PTY runtime hosts selected sessions; both backends supported

Requires aplexer milestones through PTY persistence, workspace/tag identity, cgroups/OOM,
`snapshot --json`, `attach`, and (for live UX parity) `a watch --jsonl`.

- Android: add an aplexer session source next to the tmux one. `FolderListGateway` learns to
  merge `a snapshot --json` (via SSH exec) into the folder tree; attach becomes an SSH PTY
  channel running `a attach <workspace>:<tag>` instead of the `-CC` client; live updates come
  from one long-lived SSH exec channel running `a watch --jsonl`, replacing `-CC` notifications
  for aplexer-hosted sessions. `tmuxctl create-detached --mem` memory caps are subsumed by
  aplexer's native per-session cgroups. `HostTmuxSessionListParser` / name-derivation /
  collision-suffix code is bypassed for aplexer sessions — workspace+tag is authoritative
  (spec §22.1's folder UI maps directly).
- Android agent state: today's hook-written `@ps_agent_state` mechanism needs an aplexer
  equivalent — see "state ingestion" in Open questions; spec §20's adapter list does not yet
  include hook-push, which is PocketShell's proven mechanism.
- Desktop: add an aplexer-backed session provider beside `TmuxSessionManager`; assistant tools
  (`list_sessions`, `start_session`, ...) gain workspace:tag addressing. Aplexer metadata makes
  `ConversationAttributionService`'s `ps`/`pgrep` BFS and the PSV detection feed unnecessary for
  aplexer sessions — declared engine/profile/workspace replace inference (spec's second core
  invariant). **Caveat:** desktop actively uses tmux windows/panes/splits; aplexer v1 explicitly
  has none, so desktop either keeps split layouts tmux-only, drops them for aplexer sessions, or
  rebuilds splits client-side (multiple attached aplexer sessions in VS Code editor groups).
  This is the largest Phase B/C product question for desktop.
- Both: remember to land desktop changes in both `src/**` and the
  `extensions/pocketshell/src/backend/**` mirror.

#### Phase C — aplexer default; tmux path demoted

- Default new sessions to aplexer in both clients; keep tmux read-only support (or a one-shot
  migration: recreate sessions under aplexer — running agent processes cannot be moved between
  PTY owners, so migration means restart, acceptable given agents' `--resume` support).
- Retire from PocketShell: `tmuxctl` invocations, `@ps_*` user options, name-derivation and
  collision code, `agents_kind.py`/`cgroup_agents.py` inference, PSV detection feed (desktop).
- Explicit leftovers that do NOT move to aplexer (non-goals in spec §27): `jobs.py` recurring
  pings (uses `tmuxctl`; needs a new home or stays on tmux), `serve.py`, usage reporting
  (`quse`/`pocketshell usage`), the cards push feed, conversation-transcript parsing.

### 1.5 What is genuinely blocked on unbuilt aplexer features

Honestly: **everything is blocked on aplexer code existing** — the repo is spec-only today. In
dependency order:

| Needed for | Missing aplexer piece |
| --- | --- |
| Phase A | engine registry, profile discovery, `--json` listing, and the **launch-resolution/exec command absent from the spec** (§1.4 Phase 0) |
| Phase A (nice-to-have) | Python `aplexer` package (spec §21) — not strictly required; subprocess JSON suffices |
| Phase B | PTY runtime, workers, workspace/tag identity, cgroup/OOM isolation, `snapshot --json`, `attach`, `send`, `capture` |
| Phase B live UX | `a watch --jsonl` (spec §19 — schema sketched only; see Part 2), long-lived-SSH-channel behavior: reconnect, generation-gap detection, snapshot fallback |
| Phase B agent state | an agent-state *ingestion* path for hooks (not in spec §20's derivation list) |
| Phase B/C desktop | any story for panes/splits (v1 non-goal) |
| Multi-host | nothing beyond §22.3's "SSH exec + SSH PTY" — acceptable, but there is no daemon, so `watch` and every snapshot are per-SSH-channel; PocketShell's warm-lease model (`SshLeaseManager`, "no new connection" rule D21) fits this well |

---

## Part 2 — Common event format: adopting heru's `UnifiedEvent`

### 2.1 What heru's format actually is (found, not inferred)

heru has an **explicit, documented, contract-tested schema** — this mapping is against a real
schema, not an inference:

- `README.md` § "Unified Event Schema" documents the envelope.
- `heru/types.py` defines it as the pydantic model `UnifiedEvent` (public API contract).
- `tests/contract/test_types_contract.py` pins the shape; changing it is semver-major by the
  repo's own policy.

The envelope (one JSON object per stdout line, serialized with
`model_dump_json(exclude_none=True)` — i.e. **null fields are omitted**):

```python
class UnifiedEvent(BaseModel):
    kind: Literal["message","tool_call","tool_result","error","usage","status","continuation"]
    engine: str                      # required; emitting engine name
    sequence: int = 0                # zero-based event order within the run
    timestamp: str                   # ISO-8601 UTC, second precision (utcnow())
    role: Literal["assistant","user","system"] | None = None
    content: str = ""
    tool_name: str | None = None
    tool_input: str | None = None
    tool_output: str | None = None
    error: str | None = None
    usage_delta: dict[str, str|int|bool|None] = {}
    continuation_id: str | None = None   # engine-native session/thread id
    raw: dict[str, object] = {}          # original provider-native payload
    metadata: dict[str, str|int|bool|None] = {}
```

Related heru models worth knowing: `ResourceLimitEvent` (`resource: "memory"|..., reason,
observed_signal, exit_code, memory_mb, ...`) — normalized OOM/limit details, but attached to
stage reports, **not** part of the JSONL stream; `RuntimeEngineContinuation`; and heru's own
`profiles.toml` `LaunchProfile {name, engine, command, env, unset_env, preflight}`.

### 2.2 The honest structural mismatch

heru's stream is a **per-run, conversation-level** event stream from a *headless* agent
invocation (`heru codex "<prompt>"`): messages, tool calls, usage. Aplexer's sketched stream
(spec §19) is a **host-level session-lifecycle** stream for *interactive, long-lived* TUI
sessions: created/activity/state/oom/exited/deleted. These are different layers. Aplexer v1 will
emit **none** of heru's conversation kinds, and heru has **no** lifecycle kinds. So "same format"
can honestly mean: **aplexer adopts heru's envelope (field names, types, serialization, transport
conventions) and expresses its lifecycle events inside it** — it cannot mean event-type-level
identity today.

Two ways to fit lifecycle events into the closed `kind` literal:

- **Option 1 (recommended, default below):** map onto existing kinds — `"status"` for normal
  lifecycle transitions, `"error"` for failure ones — with the precise aplexer event name in
  `metadata.event`. Requires **no heru change**; every line is schema-valid `UnifiedEvent` and
  existing heru consumers can at least parse and display it.
- **Option 2:** propose adding a `"lifecycle"` kind (and perhaps first-class `workspace`/`tag`
  fields) to heru. Cleaner, but it is a semver-major break of heru's contract
  (`tests/contract/` + README policy) and drags heru's other consumer (litehive) into the
  change. Decide with the heru repo, not unilaterally here.

### 2.3 Envelope conventions aplexer adopts

- JSONL, one `UnifiedEvent` object per line; omit-null serialization.
- `timestamp`: ISO-8601 UTC with second precision (replacing spec §19's sketched epoch ints,
  e.g. `"at":1787739912` → `"timestamp":"2026-08-26T12:00:00+00:00"`).
- `sequence`: zero-based monotonically increasing counter **per `a watch` stream** — this
  satisfies spec §19's gap-detection requirement, but note the semantic shift from heru
  (per-run) to aplexer (per-stream); document it in aplexer's schema notes. Additionally carry
  the global snapshot `generation` in `metadata.generation` so clients can fall back to
  `a snapshot --json` coherently.
- `engine`: the session's aplexer engine id (`claude`, `codex`, `opencode`, `grok`) for agent
  sessions. It is a plain `str` in the model (not a literal), so aplexer ids that heru doesn't
  ship adapters for (`grok`) are wire-legal. For shell/process sessions there is no engine —
  proposal: `"aplexer"` as the emitting-component value (decision flagged below).
- `continuation_id`: **reserved** for the engine-native session id (what `--resume` takes), if
  aplexer ever learns it. The aplexer session ULID is *not* a continuation id — it goes in
  `metadata.session_id`. Conflating these would be the tempting-but-wrong mapping.
- `raw`: aplexer's own native event object (mirroring heru's "original provider payload"
  semantics — here aplexer is the provider).
- Aplexer-model fields with no heru field — `session_id`, `workspace`, `tag`, `profile`, session
  kind, state, reason, generation — all ride in `metadata`, which is contract-legal (flat dict of
  scalars; all these values are scalars). This is the price of Option 1: they are schema-blessed
  but untyped.

### 2.4 Event-by-event mapping

Common fields on every aplexer-emitted event: `engine`, `sequence`, `timestamp`, `raw`, and
`metadata` containing at least `{event, session_id, workspace, tag, generation}` (+ `profile`,
`session_kind` where meaningful; key is `session_kind`, not `kind`, to avoid colliding with the
envelope's `kind`).

| aplexer event (spec §19) | heru `kind` | field mapping | notes / gaps |
| --- | --- | --- | --- |
| `session.created` | `status` | `metadata.event="session.created"`; `metadata`: `session_id`, `workspace`, `tag`, `profile`, `session_kind` (`agent`/`shell`/`process`); `content` = human summary (e.g. `"created pocketshell:review (codex/zai)"`) | heru has no workspace/tag/profile/session-kind concepts at all — metadata-only |
| `session.activity` | `status` | `metadata.event="session.activity"`; `timestamp` = activity time (replaces sketched `at`) | high-frequency; consider coalescing before emit — heru streams have no rate conventions |
| `agent.state` | `status` | `metadata.event="agent.state"`, `metadata.state` = spec §20 vocab (`starting/running/waiting/idle/exited/oom/error/unknown`); `content` = state string for display | heru has **no** agent-semantic-state field. Its `SubagentStatus` (`created/running/completed/failed/blocked/interrupted`) is a different vocabulary for a different concept (pipeline subagents) — do not conflate; keep aplexer's vocabulary, in `metadata` |
| `session.oom` | `error` | `error` = human reason (e.g. `"workload killed: cgroup memory limit"`); `metadata.event="session.oom"`; flatten `ResourceLimitEvent`-shaped scalars into `metadata` (`resource="memory"`, `memory_mb`, `observed_signal`) and/or nest the full shape in `raw` | `ResourceLimitEvent` is heru's normalized shape for exactly this, but it is not a stream event there — reusing its *field names* inside `metadata`/`raw` is alignment, not compliance |
| `session.exited` | `status` (normal exit) | `metadata.event="session.exited"`, `metadata.reason` (`exit`/`signal`/`killed`), `metadata.exit_code` | judgment call: abnormal exits could be `kind:"error"` instead; recommend `status` always, reserving `error` for `oom`/internal errors, so the `event`→`kind` mapping stays deterministic |
| `session.deleted` | `status` | `metadata.event="session.deleted"`, `metadata.session_id` | no heru analogue; pure metadata event |

Example lines:

```json
{"kind":"status","engine":"codex","sequence":0,"timestamp":"2026-08-26T12:00:00+00:00","content":"created pocketshell:review (codex/zai)","raw":{"type":"session.created","id":"019d..."},"metadata":{"event":"session.created","session_id":"019d...","workspace":"/home/alexey/git/pocketshell","tag":"review","profile":"zai","session_kind":"agent","generation":1842}}
{"kind":"status","engine":"codex","sequence":1,"timestamp":"2026-08-26T12:05:11+00:00","content":"waiting","raw":{"type":"agent.state","id":"019d...","state":"waiting"},"metadata":{"event":"agent.state","session_id":"019d...","state":"waiting","generation":1843}}
{"kind":"error","engine":"claude","sequence":2,"timestamp":"2026-08-26T12:09:40+00:00","error":"workload killed: cgroup memory limit","raw":{"type":"session.oom","id":"019e..."},"metadata":{"event":"session.oom","session_id":"019e...","resource":"memory","generation":1844}}
```

### 2.5 What deliberately does not map

- **heru's conversation kinds** (`message`, `tool_call`, `tool_result`, `usage`,
  `continuation`): aplexer v1 emits none. Both clients currently parse agent transcript JSONL
  themselves (Android `shared/core-agents/` parsers; desktop `src/agents/conversation/parsers/`).
  A *future* aplexer feature could tail transcripts and emit true heru conversation events per
  session — that would make the "one common format" vision real end-to-end and let both clients
  delete their duplicate parsers — but spec §27 lists agent conversation storage as a v1
  non-goal. Flagged as an open direction, not planned here.
- **Transport**: heru = stdout of a finite run; aplexer = long-lived `a watch --jsonl` (over an
  SSH channel for PocketShell). Envelope reuse only; run-lifecycle assumptions (e.g. heru's
  final `continuation` event) do not transfer.
- **Engine vocabularies** overlap but differ: heru ships `codex, claude, copilot, gemini,
  opencode, goz`; aplexer plans `claude, codex, opencode, grok`. Wire-legal (plain `str`) but
  documentation/tooling that hard-codes heru's list will not know `grok`.
- `role`, `tool_*`, `usage_delta` are meaningless for lifecycle events and are simply omitted
  (which omit-null serialization makes natural).

---

## Open questions / risks

1. **Missing spec feature blocking Phase A:** a launch-resolution command
   (`a launch-exec` / `a launch-spec --json`) that prepares engine+profile argv/env *without*
   creating an aplexer session, so tmux-hosted Phase A launches can delegate to aplexer. Spec §16
   has no such command; needs a spec addition before Phase A is possible.
2. **Agent-state ingestion:** PocketShell's working mechanism is agent hooks pushing state
   (`tmux set-option @ps_agent_state`). Spec §20 only lists derivation (process/output/logs).
   Aplexer likely needs a push endpoint (e.g. `a state-report <session> waiting`, callable from
   Claude/Codex/OpenCode hooks) to reach parity; whether hook installation itself (today
   `hooks.py`) moves into aplexer's workspace-preparation is undecided.
3. **Desktop panes/splits vs aplexer v1's no-panes model** — the biggest product-level conflict
   found. Options (keep splits tmux-only / drop for aplexer sessions / client-side splits) need a
   product decision; nothing in the spec addresses it.
4. **heru schema evolution ownership:** Option 1 (status/error + `metadata.event`) needs no heru
   change; a first-class `lifecycle` kind (Option 2) is a heru semver-major break affecting
   litehive. Who decides, and is heru's contract the right long-term home for host-lifecycle
   events at all? Not resolvable from code alone.
5. **Three profile stores in the ecosystem:** `~/.config/pocketshell/profiles.yaml`,
   `~/.config/heru/profiles.toml` (different shape: `command`/`unset_env_file`/`preflight`), and
   the `~/git/.agents` repo. If aplexer becomes authoritative, does it also generate/serve heru's
   `profiles.toml`, or do aplexer and heru intentionally keep separate profile namespaces?
   The field shapes are close but not isomorphic (aplexer/pocketshell: `config_dir` + marker
   discovery; heru: explicit command/preflight overlays).
6. **SSH/remote model:** aplexer is a local per-user runtime with no daemon; PocketShell is
   inherently remote. Per-query SSH exec + a long-lived `watch` channel fits PocketShell's
   warm-lease design, but: `watch` reconnect/gap semantics, snapshot latency without a daemon
   (spec §15.1 defers a control process pending profiling), and battery cost of a persistent
   channel on Android are unvalidated.
7. **Session identity migration:** both clients (and desktop's assistant tools + Android's
   DB/notification plumbing) key on tmux session *names*; aplexer keys on workspace:tag + ULID.
   The mapping/migration for existing sessions and stored references is undesigned. Live agent
   processes cannot be transplanted between tmux and aplexer — Phase C migration implies
   restart-with-`--resume`.
8. **Non-migrating features need homes:** `jobs.py` recurring pings (built on `tmuxctl`), cards
   push feed, `usage`/`quse` — out of aplexer scope by spec §27; their tmux dependence outlives
   Phase C unless separately rehomed.
9. **Provider-key safeguard semantics:** carry PocketShell's forced-union, non-optional
   `PROVIDER_ENV_UNSET_VARS` behavior into aplexer's engine config explicitly — spec §8.2's
   "provider-key environment stripping" bullet under-specifies it, and a declarative TOML
   override mechanism could accidentally make it optional.
10. **Desktop's mirrored backend** (`src/**` ⇄ `extensions/pocketshell/src/backend/**`): every
    integration change must land twice; risk of drift during the migration unless the mirror is
    automated/verified in CI (not checked as part of this plan).
