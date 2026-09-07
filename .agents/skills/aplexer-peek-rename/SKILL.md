---
name: aplexer-peek-rename
description: "Peek inside aplexer sessions to see what each one is actually doing (status, screen capture, transcript) and rename cryptic workspace+tag sessions to understandable names with `a rename`. Use when the session list is full of cryptic tags, when you need to identify what's running where before acting, or when asked to inspect, summarize, or relabel sessions."
---

# Aplexer peek-and-rename

You are looking at an aplexer host where sessions are addressed as
`(workspace, tag)` with an immutable internal UUID underneath. Tags drift:
`main-2`, `git-foo`, `2`, `review` stop meaning anything once there are 20
sessions. This skill is the loop for fixing that: **peek inside each
session, figure out what it's doing, rename it to something you can
understand.**

All peeking is read-only. Rename is metadata-only (same ID, same
socket/cgroup/PTY, nothing restarts). Verified against the `a` CLI in this
repo — every command below was exercised against live sessions.

## 1. Inventory first

```bash
a list                          # human view, grouped by workspace
a list --json                   # machine view (same rows + agent/state)
a snapshot --json               # bare newest-first array, same rows
```

Every `--json` row carries (among others): `id`, `workspace`, `tag`,
`engine`, `profile`, `phase`, `state`, `worker_alive`, `agent`
(`claude`/`codex`/`opencode`/`grok`/`null`, detected live from the
workload's process tree — not from config), `cwd`, `command`,
`last_activity_ms`. `agent: null` means no recognizable agent is running in
there right now (including an agent that already exited).

## 2. Addressing a session

Every inspect/rename command takes the session as one `SESSION` argument —
use whichever form is handy:

- full UUID: `a status 324ca01c-e4d5-48e9-b694-1180586b4dd7 --json`
- unambiguous UUID prefix: `a status 324ca01c --json`
- `workspace:tag` selector: `a status ~/git/dtc-website:git-dtc-website`
- bare tag in the current workspace: `cd ~/git/foo && a status review`
- explicit flags: `a status --workspace ~/git/foo --tag review --json`

In scripts, prefer the full UUID from `a list --json` — it survives renames,
tags don't.

## 3. Peek — three layers, cheapest first

**Layer 1 — status (metadata, instant, never touches the PTY):**

```bash
a status <SESSION> --json
```

Tells you workspace/tag/engine/profile, `phase` vs `state` vs
`worker_alive`, `agent`, foreground command, PIDs, `cwd`, original
`command`, last activity. Enough to classify "dead shell" vs "live agent"
without reading any output.

**Layer 2 — screen capture (what's happening RIGHT NOW, the main tool):**

```bash
a capture <SESSION> --screen --plain     # current screen as plain text
a capture <SESSION> --screen             # same, with paintable escapes
a capture <SESSION> --bytes 4000         # raw history tail instead
a capture <SESSION> --json               # byte-exact base64 envelope
```

Prefer `--screen --plain` for "what is this session doing": it renders the
live screen as the agent sees it (prompt, TUI state, last answer) in a few
hundred bytes to a few KB, instead of an arbitrary tail of byte history.
Fall back to `--bytes N` when you want scrollback rather than the screen.
For a dead session, `--screen --plain` falls back to the `screen.txt`
post-mortem captured at exit.

**Layer 3 — transcript (what the agent SAID/DID, when it exists):**

```bash
a transcript <SESSION> --last 5 --json
a transcript <SESSION> --kind message --last 3        # turns only
a transcript <SESSION> --before 12 --last 20 --json   # older page
a transcript <SESSION> --after 31 --follow --json     # live tail
```

Parses the engine's native JSONL into `UnifiedEvent`s with stable `seq`
cursors. Two caveats, both normal:

- A `shell`-engine session with a hand-started agent may have no bindable
  native log → `a: no shell transcript found for session ...`. That's not
  an error in your invocation — use Layer 2 (screen capture) instead.
- `--kind` filters to one of `message`, `tool_call`, `tool_result`,
  `error`, `usage` before `--last`/`--before`/`--after` apply.

Suggested per-session peek order: `status --json` → `capture --screen
--plain` → `transcript --last 5 --json` only if you need the conversation
rather than the current state.

## 4. Rename

```bash
a rename <SESSION> --tag <NEW-TAG> [--workspace <NEW-PATH>] [--json]
```

Rules (enforced by the CLI, so you'll get an error, not a silent bad name):

- Tag: 1–64 bytes, only ASCII letters, digits, `.`, `_`, `-`. No spaces,
  no slashes, no emoji. `seo-slice-1` good; `SEO slice #1` rejected.
- `(workspace, tag)` must be unique among live sessions. On collision the
  CLI names the holder — pick a different tag or rename the holder first.
- Identity is untouched: same UUID, same socket/cgroup/PTY/worker. Clients
  holding the UUID keep working; clients holding the old tag must switch.

Verify after each rename:

```bash
a status <UUID> --json      # same id, new tag
a list                      # new tag shows up in the workspace group
```

Message threads (`a message log`) stay resolved to the session id, so a
renamed session keeps its history.

## 5. The cryptic-tags loop

1. `a list --json` → collect `(id, workspace, tag)` for everything with a
   cryptic tag.
2. For each: `a status <id> --json`, then `a capture <id> --screen
   --plain`. Add `a transcript <id> --last 5 --json` when the screen
   doesn't settle it.
3. Propose the new tags (short, kebab-case, what/why not engine name —
   `seo-slice-1`, `auth-refactor`, `notes-mirror`; don't encode
   engine/profile in the tag, that's already metadata).
4. `a rename <id> --tag <new>` one by one; verify with `a list`.
5. If two sessions deserve the same name, disambiguate with a suffix
   (`review`, `review-2`) rather than reusing a live tag.

## Don't

- Don't `a attach` to peek — it's interactive (takes over your terminal)
  and unnecessary; `capture --screen --plain` shows the same screen.
- Don't `kill` / `forget` / `prune` in a rename pass. Exited sessions keep
  their records for post-mortem capture on purpose; `kill` removes the
  record entirely.
- Don't parse human `a list` tables in scripts — use `a list --json` /
  `a snapshot --json` and the UUID.
- Don't rename to a tag that's already live in that workspace — check
  `a list` first, or let the collision error tell you the holder.
