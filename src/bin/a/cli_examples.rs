// -- `Examples:` sections appended to each command's --help output. Kept
// as consts so the derive attributes below stay one line each; every
// block leads with copy-pasteable commands and explains in the comment.
pub(crate) const ROOT_EXAMPLES: &str = r#"Examples:
  a                                      sessions at a glance (bare `a` == `a list`)
  a -                                    create-or-attach session "main" right here
  a -review                              create-or-attach session "review" right here
  a - codex review                       create-or-attach codex, tagged "review"
  a new                                  always-create a fresh session here, attached
  a new --engine codex --tag refactor    create with start's full options, attached
  a 2                                    attach to workspace #2 from `a list`
  a 2 review                             attach to "review" in workspace #2
  a open review                          attach by tag in the current workspace
  a send review "cargo test" --enter     type into a session without attaching
  a capture review --screen              peek at what that session shows right now
  a kill review                          hang up a session you are done with
  a keys                                 keys available while attached (Ctrl-b ...)

A session is addressed as a UUID/prefix, `workspace:tag`, or a bare tag in
the current workspace. Every command also takes --json for machine-readable
output, and every subcommand's --help ends with its own examples.
"#;

pub(crate) const START_EXAMPLES: &str = r#"Examples:
  a start                                  new session "main" here, in the background
  a start --attach                         same, but attach as soon as it is up
  a start --engine codex --tag review      pick engine and tag
  a start --workspace ~/git/api --tag dev  session for another directory
  a start --env OPENAI_API_KEY=sk-...      pass env through (repeatable)
  a start -- cargo watch -x check          run a literal command, not an engine
"#;

pub(crate) const NEW_EXAMPLES: &str = r#"Examples:
  a new                              fresh session here, attached (main-2 if taken)
  a new --engine codex --tag review  always creates: a taken tag becomes review-2
  a new --engine shell               a second plain shell alongside `a here`
"#;

pub(crate) const HERE_EXAMPLES: &str = r#"Examples:
  a here                        create-or-attach session "main" in this directory
  a here codex review           create-or-attach codex, tagged "review"
  a here coz                    a configured shortcut: engine+profile in one word
  a here htop                   not a known engine, so runs htop literally
  a -review                     `a here` with the tag named after the dash
"#;

pub(crate) const LIST_EXAMPLES: &str = r#"Examples:
  a list                        live sessions, grouped by workspace (bare `a` too)
  a list --all                  include exited sessions too
  a list --running              only live sessions
  a list --sort activity        busiest workspace first; the choice is remembered
  a list --json                 machine-readable
"#;

pub(crate) const SNAPSHOT_EXAMPLES: &str = r#"Examples:
  a snapshot                    `a list`'s data, always machine-readable
  a snapshot --running          live sessions only
"#;

pub(crate) const ATTACH_EXAMPLES: &str = r#"Examples:
  a attach review                            tag in the current workspace
  a attach myrepo:review                     workspace:tag, works from anywhere
  a attach 7f3c                              UUID prefix
  a attach --workspace ~/git/api --tag main  explicit flags instead
"#;

pub(crate) const SEND_EXAMPLES: &str = r#"Examples:
  a send review "cargo test" --enter  type a command and press Enter for them
  a send myrepo:review "hi"           any session selector works
  a send review --stdin < patch.diff  stream a file's bytes into the session
  a send review --hex 1b5b41          send raw bytes, hex-encoded on the way in
"#;

pub(crate) const CAPTURE_EXAMPLES: &str = r#"Examples:
  a capture review                   recent output, as received bytes
  a capture review --screen --plain  the screen right now, as plain text
  a capture review --bytes 8192      read more (or fewer) history bytes
  a capture review -o tail.log       write to a file instead of stdout
"#;

pub(crate) const STATUS_EXAMPLES: &str = r#"Examples:
  a status review               is it running, exited, since when, and why
  a status myrepo:review        any session selector works
  a status review --json        machine-readable
"#;

pub(crate) const KILL_EXAMPLES: &str = r#"Examples:
  a kill review                       hang it up (SIGHUP, 2s grace, then SIGKILL)
  a kill myrepo:review --signal TERM  graceful TERM-first shutdown
  a kill review --grace-ms 10000      more time to exit cleanly
"#;

pub(crate) const FORGET_EXAMPLES: &str = r#"Examples:
  a forget 7f3c --force         drop dead-session records; processes may live on
"#;

pub(crate) const PRUNE_EXAMPLES: &str = r#"Examples:
  a prune                       remove dead, unreclaimable sessions + their history
  a prune --json                machine-readable report of what went away
"#;

pub(crate) const RENAME_EXAMPLES: &str = r#"Examples:
  a rename --tag docs              rename the session you are inside (APLEXER_SESSION_ID)
  a rename main --tag docs         retag "main" as "docs" in the current workspace
  a rename myrepo:main --tag docs  any session selector works
"#;

pub(crate) const ENGINES_EXAMPLES: &str = r#"Examples:
  a engines                     ids `a start --engine` accepts
  a engines --json
"#;

pub(crate) const PROFILES_EXAMPLES: &str = r#"Examples:
  a profiles                    engine variants, e.g. another account via CODEX_HOME
  a profiles --json
"#;

pub(crate) const DOCTOR_EXAMPLES: &str = r#"Examples:
  a doctor                      check config, engines, cgroups, state dir
  a doctor --json
"#;

pub(crate) const INIT_EXAMPLES: &str = r#"Examples:
  a init                        install agent-state hooks for every engine found
  a init --check                installed already? exit code 0 says yes
  a init --engine codex         just codex
  a init --uninstall            remove aplexer's hooks again
"#;

pub(crate) const WHOAMI_EXAMPLES: &str = r#"Examples:
  a whoami                      identity of the session containing this shell
  a whoami --json
"#;

pub(crate) const STATE_REPORT_EXAMPLES: &str = r#"Examples:
  a state-report waiting        pushed by hooks from `a init`; not typed by hand
"#;

pub(crate) const MESSAGE_EXAMPLES: &str = r#"Examples:
  a message send --to review "done, see api.md"  note to one sibling, by tag
  a message send --all "standup in 5"            every sibling in the workspace
  a message inbox                                what is unread for this session
  a message log                                  the whole workspace conversation
"#;

pub(crate) const MESSAGE_SEND_EXAMPLES: &str = r#"Examples:
  a message send --to review "done, see api.md"     one session, by tag
  a message send --all "standup in 5"               every sibling in this workspace
  a message send --to-engine codex "status?"        every codex sibling
  a message send --to review --pane "git push"      inject as terminal input instead
  a message send --to watcher --queue "anyone up?"  target need not exist yet
  a message send --to review "ctx" --kind handoff --data '{"branch":"feat"}'
"#;

pub(crate) const MESSAGE_REPLY_EXAMPLES: &str = r#"Examples:
  a message reply <id> "on it"            the id comes from `a message inbox`
  a message reply <id> --pane "approved"  deliver into the sender's terminal
"#;

pub(crate) const MESSAGE_INBOX_EXAMPLES: &str = r#"Examples:
  a message inbox               unread messages addressed to this session
  a message inbox --from main   read as a different consumer tag
"#;

pub(crate) const MESSAGE_LOG_EXAMPLES: &str = r#"Examples:
  a message log                 the workspace conversation, oldest first
  a message log --workspace ~/git/api
"#;

pub(crate) const MESSAGE_SHOW_EXAMPLES: &str = r#"Examples:
  a message show <message-id>   one message envelope, full detail
"#;

pub(crate) const MESSAGE_ACK_EXAMPLES: &str = r#"Examples:
  a message ack <id>            mark one message read
  a message ack --all           clear the whole inbox
"#;

pub(crate) const MESSAGE_GC_EXAMPLES: &str = r#"Examples:
  a message gc                  drop expired/over-cap messages from the mailbox
"#;

pub(crate) const WATCH_EXAMPLES: &str = r#"Examples:
  a watch --jsonl                        lifecycle events for every agent session
  a watch --jsonl --workspace ~/git/api  scope to one workspace
  a watch --jsonl --all                  include plain shell sessions too
"#;

pub(crate) const TRANSCRIPT_EXAMPLES: &str = r#"Examples:
  a transcript review --last 50              last 50 conversation events
  a transcript review --follow               print new events as the agent writes them
  a transcript review --kind tool_call       only one event kind
  a transcript review --before 12 --last 20  page backward through history
"#;

pub(crate) const COMPLETIONS_EXAMPLES: &str = r#"Examples:
  source <(a completions bash)          try it in the current shell
  a completions zsh > "${fpath[1]}/_a"  then restart the shell to compinit
  a completions fish > ~/.config/fish/completions/a.fish
"#;

pub(crate) const HOTKEYS_EXAMPLES: &str = r#"Examples:
  a hotkeys                     the Ctrl-b key reference for attached mode
"#;
