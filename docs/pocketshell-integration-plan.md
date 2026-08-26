# PocketShell integration plan and heru event-format alignment

Status: planning document, originally written 2026-08-26 against a spec-only snapshot of this
repo; **refreshed 2026-08-26 against commit `69774f3`**, by which point the Rust implementation
is real and substantial. Every claim below about aplexer's CLI surface was re-verified by
building `./target/release/a` at that commit and reading `src/bin/a.rs` / `src/lib.rs` — not
carried over from the spec. The Part 1 analysis of what the PocketShell Android app does today
was spot-checked against its source on the same date (that repo has not moved) and still holds.

**Two large corrections in this refresh** beyond the aplexer-side update:

1. The original Part 1.2 analyzed the **wrong desktop repository**. The VS Code fork at
   `/home/alexey/git/pocketshell-desktop` (github.com/alexeygrigorev/pocketshell-desktop) is
   **archived and deprecated**. The active desktop companion is
   **pocketshell-electron** (`/home/alexey/git/pocketshell-electron`,
   github.com/alexeygrigorev/pocketshell-electron) — an actual **Electron app** (electron-vite,
   Vue 3 renderer, TypeScript, `ssh2`, xterm.js), *not* a VS Code fork. SPEC.md's and the
   original README's "terminal-first SSH client built as a VS Code fork, not Electron"
   description referred to the dead repo and is wrong for the live one. Section 1.2 is rewritten
   from scratch against pocketshell-electron; every downstream desktop conclusion changed —
   mostly for the better (the panes/splits conflict and the mirrored-backend risk both dissolve).
2. Two concurrent aplexer efforts landed mid-refresh: the **inter-agent messaging channel is now
   implemented** (`a message send/reply/inbox/log/show/ack/gc`), and
   `docs/low-bandwidth-remote-access-design.md` **exists** and is folded into the Phase B
   discussion below. `a watch` (any form) is still **not** implemented.

Sources:

- `docs/SPEC.md` in this repo (sections 8, 9, 15–23, 27); `src/bin/a.rs`, `src/lib.rs`,
  `src/messaging.rs`, `python/aplexer/` (the actual implementation, authoritative where it and
  the spec differ); `docs/low-bandwidth-remote-access-design.md`.
- PocketShell (Android): `/home/alexey/git/pocketshell` — Kotlin app + host-side Python CLI in
  `tools/pocketshell/src/pocketshell/`.
- PocketShell desktop: `/home/alexey/git/pocketshell-electron` — Electron + Vue 3 + Vite +
  `ssh2` + xterm.js (`src/main/**` Node main process, `src/renderer/**` Vue,
  `src/shared/**` common logic). The archived VS Code fork is referenced only historically.
- heru: `https://github.com/alexeygrigorev/heru` (cloned at commit `7278523`, 2026-06-27) —
  `README.md`, `heru/types.py`, `heru/base.py`, `heru/profiles.py`, `tests/contract/`.

---

## Part 1 — Integration plan for pocketshell and pocketshell-electron

### 1.1 What PocketShell (Android) actually does today

Re-verified 2026-08-26 (spot checks: `engines.py` `EngineManifest`/`LaunchSpec`/
`PROVIDER_ENV_UNSET_VARS` at lines 33/132/157; `profiles.py` `PROFILE_ENGINES = ("claude",
"codex")`; `agents.py::launch_agent` ending in `os.execvpe`; desktop `src/agents/types.ts`
`AgentType` enum). One path correction: `SessionTypePickerSheet.kt` lives under
`app/src/main/java/com/pocketshell/app/projects/`, not `.../sessions/`.

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
| Launch construction | Phone builds the string `pocketshell agent <id> --dir '<dir>' [--no-skip-permissions] [--profile '<name>']` (`app/.../projects/SessionTypePickerSheet.kt::buildRegistryAgentCommand`), tmux `send-keys` types it; host side `agents.py::launch_agent` does `build_env()` + `build_argv()` then `execvpe` |
| Agent identity | tmux user options `@ps_agent_kind`, `@ps_agent_profile`, written by `agents.py::record_agent_kind` |
| Agent semantic state | tmux user options `@ps_agent_state`, `@ps_agent_state_updated_at`, written by **agent hooks** installed by `hooks.py` (Claude `Stop`/`Notification` hooks, Codex `notify`, a generated OpenCode JS plugin — each shells `tmux set-option`); read back via batched `tmux list-sessions -F` and mapped in `SessionAgentState.kt` (`Idle / WaitingForInput / Working / Unknown` + staleness grace) |
| Structured host API | `tools/pocketshell/src/pocketshell/daemon.py` — Unix-socket JSON-RPC (`sessions.list`, `agents.kind_for_panes`, `tree.*`, `jobs.*`, `usage.fetch`, ...) with per-method TTL caches |

There is no reference to aplexer anywhere in the repo; `heru` appears only as vestigial naming
(DB columns `heruInstalled` / `heruLastDetectedAt`, historical docs note).

### 1.2 What the PocketShell desktop app (pocketshell-electron) actually does today

Rewritten in this refresh against `/home/alexey/git/pocketshell-electron` — the original
version of this section analyzed the archived VS Code fork and none of its file paths or
structural caveats apply anymore.

The Electron app deliberately **mirrors the Android app's architecture**: an Electron main
process holds `ssh2` connections and shells out to the same remote `pocketshell` host CLI for
almost everything agent- or session-shaped; a Vue 3 renderer hosts xterm.js terminals. Several
modules explicitly document themselves as ports of their Android counterparts (the bootstrap
probe, the `pathAwareCommand` PATH wrapper).

| Concern | Where it lives today |
| --- | --- |
| Host bootstrap | `src/main/helper/bootstrap.ts` — probes `pocketshell`, `tmux`, `uv`/`pipx` and daemon status over one-shot SSH execs; explicit port of Android's `HostBootstrapper`, including the `pathAwareCommand` login-PATH wrapper |
| Host CLI client | `src/main/helper/PocketshellClient.ts` — one-shot `ssh.exec` calls: `pocketshell sessions list --by activity`, `pocketshell sessions create '<name>' -c '<cwd>'` (the helper wraps `tmuxctl create-detached`; `--mem` deliberately not passed), `pocketshell agent --help` (engine capability probe), `pocketshell profiles list --json`, `pocketshell usage --json`, `pocketshell repos ...`, env list/get |
| tmux attach | `src/main/ssh/TmuxClientPool.ts` — **one persistent SSH PTY tmux client per open session tab**, joined via `tmuxctl <name>` (deliberately *not* raw `tmux attach`; the rationale is documented at length in `src/shared/attachCommand.ts`); rendered by xterm.js in `src/renderer/components/TerminalView.vue`. No `-CC` control mode anywhere |
| Engine "registry" | Agent kinds `claude/codex/opencode/grok` in `src/shared/agentLaunch.ts`, which builds `pocketshell agent <kind> ...` launch lines with flag spellings pinned against captured `pocketshell agent <kind> --help` fixtures; **availability is probed at runtime** from `pocketshell agent --help`'s subcommand list, so the host CLI — not the desktop — is already the authority on which engines exist. Per-kind UI metadata (badges, slash-command lists) in `src/shared/agentBadge.ts`, `src/shared/agentCommands.ts` |
| Profiles | Already consumed from the host CLI: `PocketshellClient.listProfiles()` → `pocketshell profiles list --json`, feeding the launch dialog (`src/renderer/components/LaunchSessionDialog.vue`) |
| Agent identity / state | The same `@ps_agent_kind` tmux user option the Android app uses, written host-side by the helper; read back via a `tmux -u list-panes -a` enrichment probe batched with the session listing (`src/main/helper/parsers.ts`). The renderer explicitly cannot set it itself (`src/renderer/pendingAgentLaunch.ts`) |
| Session identity | tmux **session name** (`src/main/projects/sessionName.ts`, `src/shared/sessionNameParts.ts`) |
| Panes/splits | **None as UI.** `list-panes` appears only as a per-server listing probe; a session is one full-screen terminal per tab. The archived fork's windows/panes/`splitWindow` usage — and the product conflict it created with aplexer's no-panes model — does not exist here |

No references to `aplexer` or `heru` anywhere in the source. There is no mirrored-backend
duplication — the archived fork's `src/**` ⇄ `extensions/**` twin-landing caveat is gone.

Consequence for this plan: the Electron app is a *stronger* Phase A citizen than the old
analysis assumed. It already routes engine availability, profile listing, session
create/list, and launch construction through remote `pocketshell` invocations — the exact seam
Phase A swaps out underneath. It inherits an aplexer-backed registry the same day Android does,
with near-zero desktop changes.

### 1.3 What aplexer actually provides today (verified at `69774f3`)

The original version of this document said "the aplexer repo currently contains only spec.md".
That is no longer remotely true. Verified against the built binary and source:

**Implemented and working:**

- `a start` — `--workspace/--tag/--engine/--profile/--cwd/--env KEY=VALUE/--memory/--pids/
  --cpu-quota-us/--history-bytes/--attach/-- <command>`, `--json`. Reclaims a workspace+tag held
  by a finished session; refuses live/broken claims.
- `a list` / `a snapshot` (aliases of the same listing; `--running`, `--json`). JSON is the full
  `SessionRecord` per session enriched with `worker_alive`; human output is a workspace-grouped
  tree. Cheap by design (pid checks only, no per-session socket round-trips).
- `a attach` — with a tmux-style reserved status-bar row (live cgroup memory indicator),
  history-tail replay (32KB default, `--history-bytes` to override), detach on `Ctrl-]` or
  `Ctrl-b d`, clean terminal reset on detach.
- `a send` (`--stdin/--hex/--enter`, workspace+tag or UUID-prefix addressing), `a capture`
  (`--bytes`, `-o`; falls back to persisted history for dead sessions), `a status` (`--json`;
  live cgroup memory/OOM telemetry via worker RPC), `a kill` (kills a broken session's surviving
  workload with `/proc/<pid>/environ` identity verification; **removes** finished sessions'
  state), `a rename`, `a doctor`.
- `a engines` / `a profiles` (`--json`). Built-in engines: **`shell` (default), `claude`,
  `codex`, `gemini`, `grok`** — note: *no `opencode`*, and `gemini` is new relative to both the
  spec's and PocketShell's lists. Profile auto-discovery is a real port of PocketShell's
  `profiles.py` (claude/codex only, `~/.<name>` sibling dirs, marker files + name hints,
  conservative, never reads inside config dirs); discovered profiles carry
  `CLAUDE_CONFIG_DIR`/`CODEX_HOME` env. User TOML config layers over built-ins
  (`[engines.*]`, `[profiles.*]`, `[shortcuts.*]`, `default_engine`, `default_profile`).
- Quick-launch `a -` (create-or-attach in cwd; first word resolved against real engines, then
  shortcuts `cl/co/g/clz/coz/cog` + user-defined, then literal command) and numbered
  quick-attach `a <N> [<M>|<tag>]`; the numeric indexes also work as selectors for
  `attach/status/kill/...`.
- `a whoami` — self-identification from inside a session via the injected
  `APLEXER_SESSION_ID`/`APLEXER_WORKSPACE`/`APLEXER_TAG` env vars; exit-code contract for
  scripts/hooks.
- **Inter-agent messaging** (`a message send/reply/inbox/log/show/ack/gc`,
  `src/messaging.rs`) — landed during this refresh. Per-workspace durable mailbox plus a
  direct pane-injection mode, per `docs/inter-agent-messaging-design.md` (v1 scope: pull-based
  inbox; no push notifications, no cross-host bridging). Session-to-session, not
  client-to-host — additive to this plan rather than on its critical path.
- Python client package (`python/aplexer/`): `list/resolve/start/status/send/capture/kill/
  rename` — subprocess for `start`, direct Unix-socket RPC for the rest. No `engines`/`profiles`
  methods yet.
- Per-session worker processes, cgroup-v2 memory/pids/cpu limits with OOM detection, durable
  session records, destructive isolation integration tests (`tests/oom_isolation.rs`).
- Session identity is **UUIDv4** (`Uuid::new_v4` in `cmd_start`), not the ULID the original
  version of this document assumed.

**Not implemented (also verified — absence of the subcommand in the binary and source):**

- `a watch` in any form. `--jsonl` and the heru envelope (Part 2) remain design-only; SPEC.md
  still lists `a watch --jsonl` under future work, and the low-bandwidth design doc (§7 item 8)
  explicitly declines to accelerate it for bandwidth reasons.
- `a launch-exec` / `a launch-spec` — still absent from both SPEC.md §16 and the code.
- Any `env_unset` / provider-key-stripping concept. `EngineConfig` is `{command, env}` — env is
  additive only.
- A snapshot `generation` counter (Part 2's `metadata.generation` is contingent on building one).

Also landed since the original doc: `docs/low-bandwidth-remote-access-design.md`, which
analyzes PocketShell's mobile/SSH usage pattern directly. Its conclusions relevant here:
payload size of `a snapshot --json` polling is a non-issue (a few KB); poll *cadence* is the
mobile battery cost, so clients should poll adaptively by app state; SSH compression and
`--history-bytes` tuning are zero-aplexer-cost wins PocketShell can take immediately; and its
prioritized aplexer work (dirty-checked status-bar redraws, `--lean`/`--no-status` attach
flags, a `--repaint` SIGWINCH wiggle, an eventual `--framed` attach protocol with resume) is
small, mostly-independent work that should be built alongside — not before — Phase B.

### 1.4 Original blockers, re-evaluated

The original Phase 0 list, item by item:

| Original blocker | Status at `69774f3` |
| --- | --- |
| `a engines --json` | **Partially resolved.** Exists, but the shape is minimal: `[{name, command, available}]`. PocketShell's `EngineManifest` additionally carries `family, harness, label, provider_mark, launch{argv, skip_permissions_argv, env_unset, ...}, usage_provider, enabled`. Either aplexer's shape grows, or (recommended, see 1.5) pocketshell keeps a thin adapter that overlays its UI/metadata fields on aplexer's authoritative `{name, command, available}` core. Vocabulary mismatch: aplexer lacks `opencode` (pocketshell built-in) and adds `gemini` + `shell` (harmless extras). |
| Provider-key `env_unset` forced union | **Not done, and now demonstrably a real gap** — the implemented `Config::resolve` merges env additively and never unsets anything. Launch delegation (A3 below) must not ship without this. |
| `a profiles --json` | **Materially resolved.** Discovery is genuinely ported from `profiles.py` (same markers, hints, conservatism). Shape differences to adapt in the Python shim: aplexer's namespace is flat, keyed by directory stem (`"zlaude"`, `"godex"`) rather than per-engine `Profile.name`; there is no `default` flag (aplexer deliberately emits no profile for an engine's own default dir); the listing includes each profile's `env` map (benign today — values are config-dir paths, not secrets — but pocketshell's "never expose env in listings" rule should be re-asserted before any secret-bearing profile env exists). |
| Launch-resolution command (`a launch-spec --json` / `a launch-exec`) | **Still missing — but it changed category.** It was a spec/design gap; it is now a small implementation task: `Config::resolve` in `src/lib.rs` already computes exactly the needed `ResolvedLaunch {engine, profile, command, cwd, env, limits, history_bytes}` — a `launch-spec` subcommand is a thin JSON-printing wrapper over it, and `launch-exec` an `execvpe` over the same. What `resolve` does **not** yet model: an `env_unset` list (above) and a skip-permissions argv variant (`LaunchSpec.skip_permissions_argv` / the phone's `--no-skip-permissions` flag). Both must be added for A3. |
| Config migration `~/.config/pocketshell/{engines,profiles}.yaml` → aplexer TOML | Not done; but the target shape is no longer speculative (`EngineConfig`/`ProfileConfig`/`ShortcutConfig` in `src/lib.rs`). Small documentation-plus-converter task. |
| Python `aplexer` package (was "nice-to-have") | **Exists** (`python/aplexer/`). Lacks `engines()`/`profiles()` methods; subprocess JSON remains fine for Phase A. |

New capabilities that change the Phase B calculus (they were all "missing aplexer piece" rows in
the original table): the PTY runtime, workers, workspace/tag identity, cgroup/OOM isolation,
`snapshot --json`, `attach`, `send`, `capture` **all exist and work now**. Phase B's remaining
blockers have shrunk to: `a watch --jsonl`, an agent-state ingestion path, and remote/SSH
validation — the desktop panes question that the original doc called Phase B's biggest product
conflict has **dissolved** with the move to pocketshell-electron, which has no split/pane UI at
all (see 1.2). `a whoami` is a genuine new enabler here: an agent hook
running inside an aplexer session can self-identify without parsing env vars, which is exactly
the addressing primitive a future `a state-report` verb (or the messaging design's sender
resolution) needs.

New gaps introduced by the new features: essentially none for Phase A — `a -`, shortcuts, the
status bar, and quick-attach are human-CLI sugar that PocketShell never calls. The one real new
mismatch is the **flat, dir-stem-keyed profile namespace** vs PocketShell's per-engine profile
names; the Python adapter in A1 absorbs it, but if aplexer becomes authoritative the Kotlin
profile pickers eventually display aplexer's ids.

### 1.5 Phased plan, made concrete

The strategic conclusion of the original document is unchanged and re-confirmed: **the
integration seam is the `pocketshell` host CLI, not the two client apps.** Both clients funnel
engine/profile/launch operations through remote `pocketshell <subcommand>` calls; Phase A
touches no tmux, `-CC`, or SSH-transport code. A corollary worth stating now that aplexer is a
real local binary: *the SSH-remoteness tension largely dissolves at this seam*, because the
`pocketshell` host CLI already runs on the same host where aplexer would run — the phone/desktop
transport is unchanged. The tension only returns in Phase B (long-lived `watch` channels over
SSH).

#### Phase 0 — aplexer prerequisites (this repo, Rust)

No longer "build aplexer"; now a short punch list:

| Step | Size | Notes |
| --- | --- | --- |
| 0.1 Add `opencode` built-in engine | trivial | one `config.engines.insert` in `Config::load` (`src/lib.rs`); without it aplexer cannot be authoritative for pocketshell's engine set |
| 0.2 Add `env_unset` to `EngineConfig` + forced-union provider-key list | small–medium | port `PROVIDER_ENV_UNSET_VARS` (~110 names) from `engines.py`; the forced union (custom engines cannot opt out) is the load-bearing property — spec §8.2 under-specifies it |
| 0.3 `a launch-spec [--engine E] [--profile P] [--no-skip-permissions] [--cwd D] --json` | small | wraps `Config::resolve`; prints `{argv, env_set, env_unset, cwd, engine, profile}`; requires 0.2 and a skip-permissions argv notion (new field on `EngineConfig` or per-engine convention) |
| 0.4 `a launch-exec ...` (execvpe variant of 0.3) | small | drop-in replacement target for `agents.py::launch_agent`'s exec step; `a.rs` already imports `CommandExt` |
| 0.5 Engines JSON enrichment decision | decision + small | recommended: keep aplexer's shape lean (`name/command/available`, plus `env_unset` count once 0.2 lands) and let pocketshell overlay UI metadata (`label`, `provider_mark`, `family`) — those are presentation concerns aplexer has no reason to own |
| 0.6 `engines.yaml`/`profiles.yaml` → TOML mapping doc or one-shot converter | small | shapes are near-isomorphic; the profile-namespace flattening (per-engine name → dir-stem id) is the only lossy step |

#### Phase A — aplexer owns engines/profiles/launch; tmux still hosts terminals

Ordered steps, in `/home/alexey/git/pocketshell` (host-side Python only; zero Kotlin changes
while output shapes are preserved):

- **A1 — profile-listing delegation (the smallest first step; see 1.6).** Size: small. Risk:
  minimal. Needs **zero aplexer changes** — `a profiles --json` works today.
- **A2 — engine-listing delegation.** `engines.py`'s manifest construction consults
  `a engines --json` for the authoritative `{name, command, available}` core, overlaying its own
  label/family/provider-mark metadata; `daemon.py` TTL caches sit on top unchanged. Size: medium
  (shape adapter + keeping `~/.config/pocketshell/engines.yaml` overrides working during the
  transition). Do after A1 validates the subprocess-JSON seam. Depends on aplexer 0.1, 0.5.
- **A3 — launch delegation.** `agents.py::launch_agent` becomes a shim that either `exec`s
  `a launch-exec ...` or applies `a launch-spec --json` before its own `execvpe`. Keep writing
  `@ps_agent_kind`/`@ps_agent_profile` tmux options — the Kotlin readers depend on them. Size:
  medium; risk: highest in Phase A (it is the actual launch path; the provider-key safeguard
  must hold). Blocked on aplexer 0.2/0.3/0.4 — **do not start until those land**.
- **A4 — desktop follow-through (much smaller than originally planned).** The original A4 —
  extracting a hardcoded engine enum and adding profiles to a VS Code fork — is obsolete: the
  Electron app already probes engine availability from `pocketshell agent --help` and already
  consumes `pocketshell profiles list --json` (see 1.2), so it inherits A1–A3 automatically.
  What remains is presentation-only: per-kind UI metadata entries in
  `src/shared/agentBadge.ts` / `agentCommands.ts` / `agentLaunch.ts` for any engine that is new
  to the desktop (its flag-spelling fixtures are pinned against captured `--help` output, so a
  new engine needs a captured fixture, as the `grok` comments in `agentLaunch.ts` document).
  Size: small. Not blocking anything.

Value delivered: one authoritative engine/profile/launch registry across Android, desktop, and
the CLI — with the desktop getting it for free through the helper seam; the provider-key
safeguard enforced in one place; the metadata model Phase B needs.

#### Phase B — aplexer PTY runtime hosts selected sessions; both backends supported

The runtime prerequisites (PTY persistence, workspace/tag identity, cgroups/OOM,
`snapshot --json`, `attach`, `send`, `capture`) **now exist** — this phase is no longer blocked
on the runtime, only on the integration-facing pieces listed in 1.7. Plan content unchanged from
the original:

- Android: add an aplexer session source next to the tmux one. `FolderListGateway` merges
  `a snapshot --json` (via SSH exec) into the folder tree; attach becomes an SSH PTY channel
  running `a attach <selector>` instead of the `-CC` client; live updates come from a long-lived
  SSH exec channel running `a watch --jsonl` (once built) — SPEC.md explicitly blesses
  interactive polling of `snapshot` as the interim, so a **polling-based Phase B pilot is now
  technically possible before `watch` exists**, though not recommended before Phase A validates
  the seam. `tmuxctl create-detached --mem` caps are subsumed by aplexer's native cgroups;
  name-derivation/collision code is bypassed — workspace+tag is authoritative.
- Android agent state: the hook-written `@ps_agent_state` mechanism needs an aplexer ingestion
  equivalent (e.g. `a state-report`, with `a whoami` as the hook-side addressing primitive) —
  still undesigned; see Open questions.
- Desktop (pocketshell-electron): structurally the *same shape* as today's tmux path, which is
  the good news of the repo correction. `TmuxClientPool` already holds one persistent SSH PTY
  per session tab running a join command (`tmuxctl <name>`); an aplexer session provider is the
  same pool running `a attach <selector>` instead, rendered by the same xterm.js view.
  `PocketshellClient.listSessions` merges `a snapshot --json` alongside
  `pocketshell sessions list`; the `@ps_agent_kind` list-panes enrichment probe is unnecessary
  for aplexer sessions (engine/profile are declared in the snapshot). No panes conflict — the
  Electron app has no split UI (1.2). The old fork's assistant-tools and mirrored-backend
  concerns are gone with the fork itself.

#### Phase C — aplexer default; tmux path demoted

Unchanged from the original: default new sessions to aplexer; tmux read-only or
migrate-by-restart (agents' `--resume` makes restart acceptable); retire `tmuxctl` invocations,
`@ps_*` options, name-derivation, and `agents_kind.py`/`cgroup_agents.py` inference.
Non-migrating leftovers (`jobs.py` recurring pings, `serve.py`, usage reporting, cards push
feed, transcript parsing) still need homes — spec §27 keeps them out of aplexer scope.

### 1.6 The smallest first step (start here)

**Delegate profile discovery in `tools/pocketshell/src/pocketshell/profiles.py` to
`a profiles --json`, in shadow mode first.** Concretely:

1. In `profiles.py`, add `_aplexer_profiles() -> list[Profile] | None`: if
   `shutil.which("a")` is falsy, return `None`; else run `["a", "profiles", "--json"]`
   (subprocess, ~2s timeout, `None` on any failure) and map each entry of the returned
   `{name: {engine, command, args, env, cwd, history_bytes, limits}}` object to
   `Profile(name=name, engine=entry["engine"], config_dir=Path(entry["env"].get("CLAUDE_CONFIG_DIR") or entry["env"].get("CODEX_HOME")), default=False, env=entry["env"])`,
   skipping entries whose `engine` is not in `PROFILE_ENGINES`.
2. First landing: **shadow mode** — `discover_profiles()` computes its native result as today,
   and (when `a` is present) also calls `_aplexer_profiles()`, logs any divergence
   (missing/extra/differing profiles) through pocketshell's existing logging, and returns the
   native result. Because aplexer's discovery is a port of this very file, divergence is a real
   signal (a port bug, or drift) rather than noise.
3. Second landing (a week of clean shadow logs later): prefer the aplexer result when available,
   behind an env-var kill switch (`POCKETSHELL_APLEXER_PROFILES=0` to force native), native
   discovery as fallback. `pocketshell profiles list`, the daemon's cached readers, the phone's
   profile picker, and the Electron app's launch dialog (which already calls
   `pocketshell profiles list --json`, see 1.2) all inherit it with no further changes.

Why this slice: it needs **zero aplexer-side changes** (verified working today), touches one
Python file plus tests, is read-only (no launch-path risk), degrades to current behavior
whenever `a` is absent or errors, and validates the exact seam every later step depends on —
"aplexer as authoritative registry, consumed over subprocess JSON by the host CLI on the same
host". The SSH question does not arise: the host CLI and aplexer are co-located by construction.

### 1.7 What is still genuinely blocked / not worth starting yet

- **A3 launch delegation** — blocked on aplexer 0.2 (`env_unset` + forced provider-key union),
  0.3/0.4 (`launch-spec`/`launch-exec`), and a skip-permissions argv model. Shipping it without
  0.2 would silently drop PocketShell's provider-key safeguard: a hard no.
- **Phase B live UX** — blocked on `a watch --jsonl`, which (verified) does not exist. Part 2
  remains the design for it. A polling pilot against `a snapshot --json` is possible but
  premature before the Phase A seam is validated in production use.
- **Agent-state ingestion** — no `a state-report`-style push endpoint exists; spec §20 still
  lists only derivation. `a whoami` provides the hook-side identity half; the ingestion verb and
  storage are undesigned. Prerequisite for Phase B reaching Android's current state-badge UX.
- **Inter-agent messaging** — now implemented (`a message ...`), but it is session-to-session
  within a workspace, not client-to-host: nothing in Phases A–C depends on it. Its inbox/pane
  machinery may later be useful as a Phase B notification substrate, but that is speculative —
  do not design client features around it yet.
- **heru "lifecycle" kind (Part 2 Option 2)** — still a heru-repo semver-major decision, not
  actionable from here.
- **Low-bandwidth aplexer work** (`docs/low-bandwidth-remote-access-design.md` §7 items 3–5, 7:
  dirty-checked status bar, `--lean`/`--no-status`, `--repaint`, framed attach) — real, small,
  and worth doing, but it is aplexer-internal polish independent of the Phase A seam; the doc
  itself sequences framed attach "with Phase B", and items 1–2/6 (SSH compression, small
  `--history-bytes`, adaptive poll cadence) are PocketShell-side and can happen any time.

---

## Part 2 — Common event format: adopting heru's `UnifiedEvent`

Status note (2026-08-26 refresh): `a watch` — with or without `--jsonl` — is **still not
implemented** (verified against the binary at `69774f3`); everything below remains the design
the implementation should follow, not a description of shipped behavior. Two corrections made in
this refresh against the now-real implementation: aplexer session ids are **UUIDv4**, not ULIDs;
and aplexer's actual engine set is `shell/claude/codex/gemini/grok` (no `opencode` yet). A third
caveat: the snapshot `generation` counter referenced below does not exist yet either — it must
be built alongside `watch`.

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
sessions: created/activity/state/oom/exited/deleted. These are different layers. Aplexer will
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
  `a snapshot --json` coherently (the generation counter itself is not yet implemented; build it
  with `watch`).
- `engine`: the session's aplexer engine id (today: `claude`, `codex`, `gemini`, `grok`) for
  agent sessions. It is a plain `str` in the model (not a literal), so aplexer ids that heru
  doesn't ship adapters for (`gemini`, `grok`) are wire-legal. For `shell`/process sessions
  there is no agent engine — proposal: `"aplexer"` as the emitting-component value (decision
  flagged below).
- `continuation_id`: **reserved** for the engine-native session id (what `--resume` takes), if
  aplexer ever learns it. The aplexer session UUID is *not* a continuation id — it goes in
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
| `session.created` | `status` | `metadata.event="session.created"`; `metadata`: `session_id`, `workspace`, `tag`, `profile`, `session_kind` (`agent`/`shell`/`process`); `content` = human summary (e.g. `"created pocketshell:review (codex/zodex)"`) | heru has no workspace/tag/profile/session-kind concepts at all — metadata-only |
| `session.activity` | `status` | `metadata.event="session.activity"`; `timestamp` = activity time (replaces sketched `at`) | high-frequency; consider coalescing before emit — heru streams have no rate conventions |
| `agent.state` | `status` | `metadata.event="agent.state"`, `metadata.state` = spec §20 vocab (`starting/running/waiting/idle/exited/oom/error/unknown`); `content` = state string for display | heru has **no** agent-semantic-state field. Its `SubagentStatus` (`created/running/completed/failed/blocked/interrupted`) is a different vocabulary for a different concept (pipeline subagents) — do not conflate; keep aplexer's vocabulary, in `metadata`. Note the implemented runtime's *phase* vocabulary is `starting/running/exiting/exited/failed` (plus derived `broken`) — phases are worker lifecycle, not agent semantic state; keep them distinct when `watch` is built |
| `session.oom` | `error` | `error` = human reason (e.g. `"workload killed: cgroup memory limit"`); `metadata.event="session.oom"`; flatten `ResourceLimitEvent`-shaped scalars into `metadata` (`resource="memory"`, `memory_mb`, `observed_signal`) and/or nest the full shape in `raw` | `ResourceLimitEvent` is heru's normalized shape for exactly this, but it is not a stream event there — reusing its *field names* inside `metadata`/`raw` is alignment, not compliance. The implemented runtime already records `exit.oom_killed` per session, so the data source exists |
| `session.exited` | `status` (normal exit) | `metadata.event="session.exited"`, `metadata.reason` (`exit`/`signal`/`killed`), `metadata.exit_code` | judgment call: abnormal exits could be `kind:"error"` instead; recommend `status` always, reserving `error` for `oom`/internal errors, so the `event`→`kind` mapping stays deterministic |
| `session.deleted` | `status` | `metadata.event="session.deleted"`, `metadata.session_id` | no heru analogue; pure metadata event. Now has a real trigger in the implementation: `a kill` removes finished sessions' state |

Example lines:

```json
{"kind":"status","engine":"codex","sequence":0,"timestamp":"2026-08-26T12:00:00+00:00","content":"created pocketshell:review (codex/zodex)","raw":{"type":"session.created","id":"019d..."},"metadata":{"event":"session.created","session_id":"019d...","workspace":"/home/alexey/git/pocketshell","tag":"review","profile":"zodex","session_kind":"agent","generation":1842}}
{"kind":"status","engine":"codex","sequence":1,"timestamp":"2026-08-26T12:05:11+00:00","content":"waiting","raw":{"type":"agent.state","id":"019d...","state":"waiting"},"metadata":{"event":"agent.state","session_id":"019d...","state":"waiting","generation":1843}}
{"kind":"error","engine":"claude","sequence":2,"timestamp":"2026-08-26T12:09:40+00:00","error":"workload killed: cgroup memory limit","raw":{"type":"session.oom","id":"019e..."},"metadata":{"event":"session.oom","session_id":"019e...","resource":"memory","generation":1844}}
```

### 2.5 What deliberately does not map

- **heru's conversation kinds** (`message`, `tool_call`, `tool_result`, `usage`,
  `continuation`): aplexer emits none. Both clients currently parse agent transcript JSONL
  themselves (Android `shared/core-agents/` parsers; desktop `src/agents/conversation/parsers/`).
  A *future* aplexer feature could tail transcripts and emit true heru conversation events per
  session — that would make the "one common format" vision real end-to-end — but spec §27 lists
  agent conversation storage as a v1 non-goal. Flagged as an open direction, not planned here.
  (The transcript-parsing duplication the original doc cited lives in Android's
  `shared/core-agents/` parsers and the archived VS Code fork; the Electron app currently
  renders the raw terminal only and parses no transcripts.)
- **Transport**: heru = stdout of a finite run; aplexer = long-lived `a watch --jsonl` (over an
  SSH channel for PocketShell). Envelope reuse only; run-lifecycle assumptions (e.g. heru's
  final `continuation` event) do not transfer.
- **Engine vocabularies** overlap but differ: heru ships `codex, claude, copilot, gemini,
  opencode, goz`; aplexer today ships `shell, claude, codex, gemini, grok` (no `opencode` yet —
  Phase 0 step 0.1 adds it). Wire-legal (plain `str`) but documentation/tooling that hard-codes
  heru's list will not know `grok`.
- `role`, `tool_*`, `usage_delta` are meaningless for lifecycle events and are simply omitted
  (which omit-null serialization makes natural).

---

## Open questions / risks

1. **Launch-resolution command** (`a launch-exec` / `a launch-spec --json`): no longer a spec
   design gap — `Config::resolve` computes the needed structure — but it is still unbuilt, and
   the *hard part* moved: modeling `env_unset` (with the forced provider-key union) and a
   skip-permissions argv variant, neither of which the implemented config schema has. A3 is
   blocked on this, and only A3.
2. **Agent-state ingestion:** PocketShell's working mechanism is agent hooks pushing state
   (`tmux set-option @ps_agent_state`). Spec §20 only lists derivation (process/output/logs).
   Aplexer likely needs a push endpoint (e.g. `a state-report waiting`, callable from
   Claude/Codex/OpenCode hooks, resolving its target via the same `APLEXER_SESSION_ID`
   mechanism `a whoami` now uses) to reach parity; whether hook installation itself (today
   `hooks.py`) moves into aplexer's workspace preparation is undecided.
3. **Desktop panes/splits vs aplexer's no-panes model** — *resolved by the repo correction*:
   this was the biggest product conflict when the desktop was the archived VS Code fork, but
   pocketshell-electron has no split/pane UI (one full-screen terminal per session tab), so
   aplexer's no-panes model matches it exactly. Kept here as a record so the conflict is
   re-examined if the Electron app ever grows splits.
4. **heru schema evolution ownership:** Option 1 (status/error + `metadata.event`) needs no heru
   change; a first-class `lifecycle` kind (Option 2) is a heru semver-major break affecting
   litehive. Who decides, and is heru's contract the right long-term home for host-lifecycle
   events at all? Not resolvable from code alone.
5. **Three profile stores in the ecosystem:** `~/.config/pocketshell/profiles.yaml`, aplexer's
   TOML (now real: flat `[profiles.<dir-stem>]` namespace + auto-discovery defaults), and
   `~/.config/heru/profiles.toml` (different shape: `command`/`unset_env_file`/`preflight`). If
   aplexer becomes authoritative, does it also generate/serve heru's `profiles.toml`, or do
   aplexer and heru intentionally keep separate profile namespaces? Additionally, aplexer's
   flat dir-stem keys vs PocketShell's per-engine profile names is a UX-visible rename the
   Kotlin pickers eventually surface.
6. **SSH/remote model:** aplexer is a local per-user runtime with no daemon; PocketShell is
   inherently remote. Phase A is unaffected (host CLI and aplexer are co-located). For Phase B:
   `watch` reconnect/gap semantics, snapshot latency without a daemon (spec §15.1 defers a
   control process pending profiling), and battery cost of a persistent channel on Android are
   unvalidated. `docs/low-bandwidth-remote-access-design.md` (landed) now covers the
   bandwidth/battery half of this: payload size is a non-issue, poll cadence is the cost, and
   `watch`-over-one-idle-channel is the endgame; `watch` reconnect/gap semantics remain the
   open design item.
7. **Session identity migration:** both clients (Android's DB/notification plumbing, the
   Electron app's `sessionName.ts`/`sessionNameParts.ts` and per-tab client pool) key on tmux
   session *names*; aplexer keys on workspace:tag + UUID.
   The mapping/migration for existing sessions and stored references is undesigned. Live agent
   processes cannot be transplanted between tmux and aplexer — Phase C migration implies
   restart-with-`--resume`.
8. **Non-migrating features need homes:** `jobs.py` recurring pings (built on `tmuxctl`), cards
   push feed, `usage`/`quse` — out of aplexer scope by spec §27; their tmux dependence outlives
   Phase C unless separately rehomed.
9. **Provider-key safeguard semantics:** now a verified implementation gap, not just a spec
   under-specification — the shipped `EngineConfig`/`Config::resolve` have no unset concept at
   all. When adding it (Phase 0 step 0.2), make the forced union non-optional by construction so
   a declarative TOML override cannot accidentally disable it.
10. **Desktop helper-version pinning:** pocketshell-electron pins its launch-line construction
    against captured `pocketshell agent <kind> --help` fixtures and probes helper capabilities
    at runtime (it documents living with helper 0.4.44 vs newer). When Phase A changes what the
    helper emits (engine lists, profile JSON), the desktop's fixtures and probes must be
    exercised against the new helper version — cheap, but easy to forget. (Replaces the
    original mirrored-backend risk, which died with the archived VS Code fork.)
11. **Listing hygiene:** `a profiles --json` and `a list --json` include full `env` maps.
    Harmless today (config-dir paths), but PocketShell's rule was "never expose env/secrets in
    listings" — decide whether aplexer adopts that rule before profiles or `--env` values ever
    carry secrets.
