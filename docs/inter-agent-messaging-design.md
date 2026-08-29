# Inter-agent messaging: a per-workspace communication channel

Status: implemented (v1 scope -- see `a message --help`); event-stream push notification, `--when-waiting` deferred pane delivery, and cross-host bridging remain design-only, see section 8's open questions
Scope: messaging between aplexer sessions that share a workspace
Related spec sections: 5.1 (no shared PTY owner), 14 (runtime storage), 18 (machine API), 19 (event stream), 26 (security), 32 (recovery)

## 1. Problem

A user routinely runs several agent sessions in the same workspace:

```text
~/git/pocketshell
├── main        claude / default
├── review      codex / zai
└── issue-2294  codex / go
```

Today these sessions are fully isolated — which is exactly what aplexer wants
for *failure* domains, but it also means one agent cannot tell a sibling
"backend is done, the API contract is in api.md" or hand off a task. The only
existing cross-session primitive is `a send`, which injects raw bytes into a
PTY. That is a keystroke channel, not a message channel: it corrupts whatever
the target agent is mid-way through typing or reasoning about, it is invisible
to a session started later, and it leaves no record.

We want a channel that is:

- scoped to a workspace (not global, not pairwise),
- usable by any session in that workspace to reach any other,
- durable enough for handoffs (recipient may not be running yet),
- and — non-negotiably — one that does **not** reintroduce a shared process
  whose death takes multiple sessions' communication (or worse, the sessions
  themselves) down.

## 2. Addressing

### 2.1 Who is the sender?

The sender is a session, identified by its immutable session ID, with its
`(workspace, tag, engine, profile)` recorded as presentation/routing metadata.
The ID is authoritative (spec §3.1: mutable names are never OS/protocol
identity); tag is what humans and sibling agents actually read.

For the sending CLI to know *which* session it is running inside, the workload
environment should carry the session identity, e.g.:

```text
APLEXER_SESSION_ID=019d4d1f-...
APLEXER_WORKSPACE=/home/alexey/git/pocketshell
APLEXER_TAG=main
```

injected by the session worker at workload launch. This is cheap, needs no
lookup, and survives `cd`. `a message send` resolves its sender identity from
these variables; `--from <tag>` (resolved against live sessions in the
workspace) is the fallback for a human driving the CLI from an unrelated
terminal. If neither is available, sends are still allowed with sender
recorded as `{"tag": null, "external": true}` — a human poking at the mailbox
is a legitimate participant.

### 2.2 Who is the recipient?

The workspace is the namespace; the **tag** is the address. This follows
directly from spec §3.2: `(workspace, tag)` is the human-facing session
identity, and it is the only stable name a sibling agent can be told about
("message the `review` session"). Engine/profile are attributes, not
addresses — two Codex sessions in one workspace differ only by tag, so
"send to codex" is ambiguous by construction.

Three recipient forms, in decreasing order of expected use:

1. **Targeted to one tag** — `to: {"tag": "review"}`. The primary form; a
   handoff has a specific recipient.
2. **Broadcast to the workspace** — `to: {"broadcast": true}`. Every session
   in the workspace except the sender is a recipient. Useful for "I'm about to
   rebase main, hold your commits" announcements.
3. **Filtered broadcast by engine** — `to: {"engine": "codex"}`. Kept as
   sugar over broadcast (recipients filter themselves); it exists in the
   envelope so intent is recorded, but it introduces no new routing machinery.

There is deliberately no addressing by session UUID in the CLI surface.
Agents learn about siblings from `a list --workspace . --json`, which shows
tags; making UUIDs the wire address would push an internal identifier into
prompts and transcripts for no gain. The envelope still records the resolved
recipient session ID *when one existed at send time*, for disambiguation
after a tag is reused.

### 2.3 Nonexistent or not-running recipients

Because transport is a durable mailbox (§3), "recipient not running" and
"recipient does not exist yet" are the same state at delivery time: the
message parks until some session with that tag reads it. But at *send* time
they deserve different treatment, because a tag that has never existed is
usually a typo:

- If a live or recently-exited session with the tag exists in the workspace:
  send succeeds silently.
- If no session with that tag has ever existed in this workspace (checked
  against session metadata): the CLI errors with the list of known tags,
  unless `--queue` is passed. `--queue` explicitly parks a message for a
  session that will be created later ("start a `security-review` session and
  it will find its instructions waiting") — a genuinely useful handoff
  pattern that must be opt-in, not an accident.
- Broadcasts always succeed; a workspace with no other sessions just has an
  unread broadcast.

Tag rename (spec §3.3) is metadata-only, and so is addressing: messages sent
to the old tag before the rename remain addressed to the old tag. The inbox
filter matches on the recipient session's *current* tag plus its session ID
(recorded at send time when resolvable), so a renamed session keeps receiving
messages that were resolved to its ID, and stops matching its abandoned tag
string. This is an acceptable edge; renames mid-conversation are rare.

## 3. Transport

### 3.1 Candidates weighed

**(a) A per-workspace Unix socket with a broker.** Something must own a
listening socket. Whatever process that is — the first session to send, an
elected worker, a spawned "workspace messaging daemon" — becomes a shared
process whose death breaks messaging for every session in the workspace, and
whose in-memory queues lose messages on crash. That is precisely the shared
failure domain of spec §5.1, rebuilt one floor up. Fixing it requires the
broker to persist to disk and be restartable-with-recovery… at which point
the disk state is the real channel and the broker is an optimization. Spec
§15 already gives the rule: central processes may exist only as rebuildable
optimizations, never as the substrate. Rejected as the *primary* transport.

**(b) Deliver via each recipient's existing per-session worker socket.** The
sender's CLI connects to every recipient worker socket (they already exist,
spec §5.1/§26 framing) and issues a new `message.deliver` RPC; workers hold
inboxes. No new shared process — good. But: the recipient must be running at
send time, which kills the most valuable pattern (park a handoff for a
session started later); broadcast becomes a multi-connect with partial-failure
semantics the sender must report; and workers grow state and responsibilities
that spec §5.2 explicitly wants kept "small and predictable". Durability
would force workers to write message files anyway — so again the files are
the truth and the socket is merely notification. Rejected as primary
transport; retained as the future *notification* path (§3.3).

**(c) A durable per-workspace mailbox on the filesystem.** No process owns
it. The filesystem is already aplexer's shared substrate (session metadata,
history — spec §14), already the recovery source of truth (spec §32), and its
"failure domain" is the disk itself, which every session already depends on.
Sends work when the recipient is down; reads work when the sender is gone;
a corrupted mailbox breaks messaging in one workspace and nothing else — no
session, PTY, or workload is affected. **Chosen.**

### 3.2 Mailbox layout: one file per message

Within (c) there are two shapes: a single append-only JSONL log per
workspace, or a maildir-style directory with one file per message.

A single JSONL file needs append atomicity across concurrent writers.
`O_APPEND` single-`write()` lines are atomic in practice for small lines on
local filesystems, but "small" is a footgun (a handoff body with a pasted
diff isn't small), a crashed writer can leave a torn tail line that every
future reader must skip over, and pruning old messages means rewriting the
file under concurrent appenders — a lock, or a compaction dance.

One file per message dissolves all of that using the exact write discipline
aplexer already mandates for metadata (spec §14.1): write to a unique temp
file, fsync, atomically rename into place. Readers never see partial
messages. Any process can prune old messages by unlinking files — idempotent,
no lock, no owner. Naming files by the message's UUIDv7 makes lexical
directory order equal time order, which gives ordering (§4) for free.

Layout, following the spec §14 state directory:

```text
${XDG_STATE_HOME:-~/.local/state}/aplexer/
    messages/
        <workspace-key>/
            workspace.json                  # {"workspace": "/home/alexey/git/pocketshell"}
            msgs/
                019d4d20-....json           # one message per file, named by message id
            cursors/
                <session-id>.json           # per-consumer read/ack cursor
```

`<workspace-key>` is derived from the canonical (realpath'd, per spec §10)
workspace path — the first 128 bits of SHA-256 over its raw Unix path bytes
(paths contain `/` and can exceed filename limits); `workspace.json` inside
the directory makes the mapping reversible for `a doctor` and humans.
Directories are `0700`, files `0600`, matching spec §26.

Consumption state lives in per-consumer **cursor files**, not in the messages
(a broadcast has many readers, so no single reader may delete or mutate a
message to mark it read). A cursor file records the consumer's last-acked
message id plus optional per-id ack exceptions. Cursor read-modify-write is
protected by a per-consumer advisory lock because concurrent CLI invocations
can still act for one session; the cursor itself is committed by atomic rename.

### 3.3 Notification: how a recipient learns there's mail

The mailbox is pull-based and that is the v1 contract: `a message inbox` is a
directory listing plus a cursor comparison — well within the spec §30
"milliseconds" budget, cheap enough for agents to call at every natural
checkpoint and for clients to poll.

Two push layers can be added later without changing the transport:

1. **Event stream.** The session worker (or the `a watch` process itself)
   inotify-watches the workspace's `msgs/` directory and emits
   `{"type":"message.received",...}` on `a watch --jsonl` (spec §19). Clients
   already consuming the event stream get messages pushed with zero new
   sockets. If the watcher dies, nothing is lost — the mailbox still has the
   messages; the watcher restarts and re-scans (spec §32 recovery posture).
2. **Worker nudge (opt-in, see §6).**

Both layers are rebuildable optimizations over durable state — exactly the
role spec §15 permits.

## 4. Delivery semantics

Semantics differ by delivery mode (§6): inbox mode is the durable default;
pane mode trades durability for immediacy.

**Inbox mode:**

- **Durable at-least-once, commit point = the sender's rename(2).** Once
  `a message send` returns success, the message is on disk and every present
  and future matching consumer will see it. There is no delivery
  confirmation and no automatic receipt — "fire-and-durable-forget". If the
  sender needs confirmation, the recipient replies (`reply_to`, §5); that is
  an application-level ack, which is the only kind that means anything when
  the recipient is an autonomous agent that may read a message and ignore it.
- **Recipient liveness is irrelevant at send time** (modulo the typo check in
  §2.3). This is the property that makes handoffs work.
- **At-least-once, not exactly-once.** A consumer that crashes between
  reading and advancing its cursor re-reads. Message IDs make deduplication
  trivial for any consumer that cares; agent consumers are naturally
  idempotent readers ("have I seen id X" is one line of transcript).
- **Ordering: total order per workspace by message id (UUIDv7).** Messages
  from a single sender are strictly ordered (UUIDv7 is monotonic within a
  process). Cross-sender order is wall-clock order to UUIDv7 precision —
  approximate, and fine: this is a mailbox between colleagues, not a
  replicated log. Consumers read in id order; `a message log` shows the
  workspace conversation in one deterministic sequence.
- **Retention.** Messages persist until pruned: default TTL 7 days, plus a
  per-workspace cap (e.g. 1000 messages / 10 MB) as backstop. Pruning is
  opportunistic — any `a message` invocation may unlink expired files — and
  ownerless: unlinking an old file is safe from any process. Acked-by-all
  status is *not* required for pruning (a session created 8 days later simply
  misses old traffic; `--queue` messages get a longer TTL or `ttl` field).
  No daemon needed.

**Pane mode (§6.2):**

- **Synchronous at-most-once.** The commit point is the target worker
  accepting the PTY write over its socket; success means the bytes reached
  the PTY master, nothing more. Failure (no such session, worker dead, socket
  unreachable) is reported immediately to the sender; nothing is queued
  unless `--or-inbox` converts the failure into an inbox send.
- **Requires a live target worker.** By construction — the PTY only exists
  while its worker does.
- **Also recorded in the mailbox** (with `"delivery": "pane"`, pre-acked for
  the recipient) so the workspace log remains a complete account of
  inter-agent traffic in both modes. The mailbox write happens after
  successful injection; a mailbox write failure after a successful injection
  is reported as a warning, not a delivery failure.
- **Ordering.** Pane messages order with the target's other terminal input by
  arrival at the PTY, and with mailbox traffic by their message id like
  everything else.

## 5. Message format

One JSON object per file. Versioned, like every machine-visible aplexer
schema (spec §12/§18):

```json
{
  "schema_version": 1,
  "id": "019d4d20-8a31-7c02-b1f0-3d9e42aa61c7",
  "workspace": "/home/alexey/git/pocketshell",
  "created_at": 1787738302,
  "from": {
    "session_id": "019d4d1f-9f52-7f21-94ce-7cc175f4ab8d",
    "tag": "main",
    "engine": "claude",
    "profile": "default"
  },
  "to": { "tag": "review", "session_id": "019d4d1f-aaaa-..." },
  "kind": "note",
  "reply_to": null,
  "body": "Backend is done. API contract is in api.md — please review the error-shape section.",
  "data": null
}
```

Field notes:

- `id` — UUIDv7; doubles as filename and ordering key.
- `to` — exactly one of `{"tag": ...}` (with `session_id` when resolvable at
  send time), `{"broadcast": true}`, `{"engine": ...}`.
- `kind` — open enum for extensibility; initial values `note` (default,
  human/agent prose), `handoff` (carries a task; conventionally expects a
  reply), `reply`, `system` (emitted by aplexer itself, reserved). Unknown
  kinds must be preserved and displayed, never dropped.
- `body` — UTF-8 text, the payload agents actually read. Size-capped
  (e.g. 64 KB) so the mailbox stays a mailbox, not a file transfer system —
  the workspace's own files are the right place for large artifacts; messages
  point at paths.
- `data` — optional structured payload for machine consumers; opaque to
  aplexer.
- `reply_to` — message id this responds to; gives threads without a thread
  object.
- `delivery` — `"inbox"` (default) or `"pane"` (§6.2); records how the
  message was delivered so `a message log` can distinguish parked mail from
  input that was injected into a terminal.

**Relation to the event-format effort (heru).** A sibling design effort is
evaluating whether aplexer's event stream should adopt a common event format
from <https://github.com/alexeygrigorev/heru>. This envelope must stay
*reconcilable* with whatever that lands on: concretely, a message must be
representable as an event (`{"type": "message.received", "message": {...}}`
or a flattened equivalent), so the envelope keeps aplexer's existing
conventions — `schema_version`, snake_case fields, integer epoch timestamps,
stable IDs — and avoids inventing a second timestamp or identity scheme. If
the heru adoption lands first and prescribes envelope fields (e.g. an event
`type`/`source`/`time` triple), this schema should be re-skinned to match
before implementation; nothing in the transport or semantics above depends on
the exact field names. Do not block messaging on that decision.

## 6. Delivery UX: two first-class modes

Aplexer already has two natural delivery surfaces: durable out-of-band state
(files, events) and the PTY itself (`a send` writes raw bytes into a session's
terminal today, spec §16.5). Rather than pick one, the channel offers **both
as first-class modes**, because they serve genuinely different intents:

- **Inbox mode** — "here is information; act on it when you next look."
- **Pane mode** — "read this *now*: it becomes terminal input in your session
  immediately."

### 6.1 Inbox mode (default)

The default for `a message send` is the durable mailbox of §3, surfaced three
ways:

1. **Inbox pull (v1).** `a message inbox` lists unread messages for the
   calling session; `--json` for agents. Agents adopt the checkpoint habit
   (encoded in the companion skill): check the inbox when starting work,
   after completing a task, and when otherwise going idle. This matches how
   coding agents actually operate — they act at turn boundaries, and a
   message discovered mid-task would be deferred to a boundary anyway.
   Harness-level hooks (e.g. a Claude Code hook running
   `a message inbox --new --json` each turn) can tighten the loop without any
   aplexer changes.
2. **Event push (later).** `message.received` events on `a watch --jsonl`
   (§3.3) for agents/clients already consuming the stream, and for PocketShell
   to show a per-session unread badge.

Inbox mode never touches the recipient's PTY. It is the right default because
it works when the recipient is down, it does not interleave with whatever the
target is typing or generating, and it leaves the target agent's transcript
exactly as its harness framed it.

### 6.2 Pane mode (direct-to-PTY, explicit)

`a message send --pane --to <tag> "text"` delivers the message as **literal
terminal input** into the target session's PTY — the same write-bytes-to-PTY
mechanism `a send` uses today, reached through the target's per-session worker
socket, but wrapped in messaging semantics rather than being a bare keystroke
primitive:

- **Addressed like any message** (workspace + tag, §2), with the same sender
  identity resolution — not a raw selector the sender must construct.
- **Framed for an agent recipient.** The injected bytes are a single
  fixed-shape line, e.g.
  `[aplexer message from main] backend done, see api.md` followed by a
  carriage return so an agent sitting at its prompt receives it as a submitted
  instruction. `--raw` suppresses the frame and trailing return for the rare
  case where exact bytes matter (at which point the sender is really doing
  `a send` with better addressing).
- **Recorded.** A pane-delivered message is *also* appended to the workspace
  mailbox with `"delivery": "pane"`, so the workspace conversation log (§7,
  `a message log`) stays complete and a later reader can see what was pushed
  into whom. The recipient's cursor treats it as already-acked — it was
  delivered by definition.
- **Requires a live target.** Pane delivery fails immediately if the target
  worker is not running or its socket is unreachable; there is no queueing in
  pane mode. `--or-inbox` degrades to inbox mode on failure instead of
  erroring, for senders who want "interrupt if you can, park it if you can't".
- **Targeted only.** No pane broadcast. Injecting input into every session in
  a workspace at once is a footgun with no motivating use case; `--all` and
  `--pane` are mutually exclusive.

`a send` itself remains what it is — the low-level byte primitive with no
envelope, no record, no framing. Pane-mode messaging is a layer above it, and
should share its implementation path (worker socket RPC that writes to the
PTY master).

### 6.3 When a sender picks which

The rule of thumb the CLI help and the companion skill should teach:

| Situation | Mode |
|---|---|
| Handoff, FYI, "when you get to it", any broadcast | inbox |
| Recipient not running yet / might be down | inbox (or `--queue`) |
| Recipient is an agent **waiting at its prompt** and you want it to act now | pane |
| Steering a sibling mid-run ("stop, the API changed") | pane, accepting that mid-generation input lands wherever the target's UI puts it |
| You need confirmation something was seen | inbox + ask for a `reply`; pane delivery proves injection, not comprehension |

The honest trade-off, stated in both docs: pane mode buys immediacy at the
cost of transcript pollution and interleaving risk. Injected input is
indistinguishable from the user typing, so the target agent will treat it as
an instruction from its operator — powerful for cooperation, and exactly why
it must be explicit (`--pane`), targeted, framed with a visible
`[aplexer message from <tag>]` prefix, and logged in the mailbox. Within one
user's workspace this is an acceptable trust model (every participant already
runs as the same user and could `a send` anyway); the frame prefix exists so
the receiving agent and any human reading the transcript can tell
sibling-agent input from operator input.

A semantic-state refinement (later, spec §20): `--pane --when-waiting` could
hold delivery until the target agent reaches `waiting` state, converting the
worst interleaving case ("injected mid-generation") into the good case
("arrives at the prompt"), with a timeout falling back per `--or-inbox`.

## 7. CLI surface (sketch, not a spec)

```bash
# send (workspace defaults to $APLEXER_WORKSPACE, else cwd)
a message send --to review "backend done, see api.md"
a message send --to security-review --queue --kind handoff \
    --data '{"files":["api.md"]}' "audit the new auth endpoints"
a message send --all "rebasing main in 5 minutes, hold pushes"
a message send --to-engine codex "codex folks: profile zai is rate-limited"
a message reply <message-id> "reviewed; two comments in the message body"

# send, pane mode: inject as terminal input into one live sibling session
a message send --pane --to review "stop: the API contract changed, re-read api.md"
a message send --pane --or-inbox --to review "ping me when the review is done"
a message send --pane --when-waiting --to review "please continue"   # later; spec §20

# receive
a message inbox                      # unread messages for the calling session
a message inbox --new --json         # machine form; empty array when none
a message inbox --watch              # follow (poll/inotify) until interrupted
a message log [--workspace .]        # full workspace conversation, id order
a message show <message-id> --json

# consume
a message ack <message-id>...        # advance/record ack for calling session
a message ack --all

# hygiene / plumbing
a message gc [--workspace .]         # prune per TTL/caps (also runs opportunistically)
```

Sender/consumer identity resolution order: `--from <tag>` flag →
`APLEXER_SESSION_ID` env → error (for `inbox`/`ack`, which need a consumer
identity; `log` and `send` degrade gracefully per §2.1).

## 8. Open questions

1. **Session self-identity env vars.** Exact names and injection point for
   `APLEXER_SESSION_ID` / `APLEXER_WORKSPACE` / `APLEXER_TAG`; behavior when a
   workload re-execs or a user opens a nested shell.
2. **heru envelope alignment.** Final field naming once the event-format
   decision lands (§5) — re-skin before implementation, not after.
3. **The nudge.** Is opt-in PTY nudging (§6.3) worth the prompt-injection and
   transcript-pollution risk even in its restricted form, or should mid-task
   attention be left entirely to harness hooks and the event stream?
4. **Cursor semantics for broadcasts to future sessions.** Does a session
   created after a broadcast see it (current answer: yes, within TTL)? Should
   `--queue` messages carry their own extended TTL, and how long?
5. **Read vs ack.** Should `inbox` implicitly advance the cursor (read =
   acked), or is the explicit `ack` step worth the extra agent action? Leaning
   explicit-ack so a crashed agent re-surfaces unhandled messages, but that
   depends on how annoying double-delivery proves in practice.
6. **Tag reuse.** After `review` exits and a new `review` session starts, does
   the new one inherit the old one's unacked targeted messages? (Current
   design: tag-addressed messages match the tag, so yes — arguably correct
   for handoffs, arguably confusing. Session-id resolution at send time
   mitigates but doesn't decide this.)
7. **Cross-host.** Messaging is per-host by construction (state dir). Whether
   PocketShell should bridge workspace mailboxes across SSH hosts is out of
   scope here but should not be foreclosed by the schema (it isn't:
   workspace path + message id travel fine).
8. **Workspace-key collisions and symlinks.** Keying by realpath'd workspace
   inherits spec §10's symlink decision; confirm the canonicalization used
   for mailboxes is byte-identical to the one used for session metadata, or
   siblings will straddle two mailboxes.
9. **Quotas and abuse.** Per-sender rate/size caps beyond the global body cap
   — probably unnecessary for a per-user local tool, but a runaway agent in a
   send loop should hit *some* wall before the disk does (the workspace cap
   in §4 may suffice).
10. **`a doctor` integration.** Mailbox health (orphaned workspace keys, torn
    temp files, cursor files for dead sessions) belongs in doctor's checks.
11. **Pane-mode framing details.** Exact prefix format for injected messages
    (`[aplexer message from <tag>]`), whether the trailing byte is `\r` or
    `\n` per engine (agents differ in what submits a prompt), whether
    multi-line bodies are allowed in pane mode or rejected in favor of
    "pointer into the inbox", and whether bracketed-paste framing should be
    used when the target terminal has it enabled.
12. **`--when-waiting` delivery.** Whether deferred pane delivery (§6.3)
    should live in the sender CLI (poll target state, then inject) or in the
    target's worker (accept-and-hold RPC) — the latter adds held state to the
    worker, which spec §5.2 resists.
