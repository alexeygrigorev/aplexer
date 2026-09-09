# esc-zcodex

Sends Esc to every live aplexer session that is running zcodex, every night
at 03:00 Europe/Berlin (a user crontab entry). Cron has no catch-up: if the
box is down at 03:00, that night's sweep is skipped.

## Which sessions get Esc

`./esc_zcodex --dry-run` shows exactly what would happen. A session counts
as "running zcodex" when either:

- its engine is `zcodex`, or
- a process named `zcodex` sits in the session workload's process tree
  (zcodex started by hand inside a plain shell session).

Exited sessions are never touched (the sweep only looks at live ones).

Caveat: a session parked at zcodex's first-boot trust prompt quits on Esc
(Esc means "No, quit" in that dialog). Sessions in the normal TUI just get
the interrupt, which is the point.

## Manual run

    ./esc_zcodex --dry-run     # list targets, send nothing
    ./esc_zcodex --only <uuid-prefix>   # sweep one session
    ./esc_zcodex               # full sweep, one Esc per target

Output goes to stdout; from cron it is appended to
`~/.local/state/esc-zcodex.log` (see the crontab line).

## Install / uninstall

Add two lines to the crontab (`crontab -e`) — above any `CRON_TZ=` line, so
the entry runs in the system timezone (Europe/Berlin on this box):

    # esc-zcodex: nightly Esc sweep of live zcodex aplexer sessions (03:00 Berlin)
    0 3 * * * /home/alexey/git/aplexer/tools/esc-zcodex/esc_zcodex >> /home/alexey/.local/state/esc-zcodex.log 2>&1

Verify with `crontab -l`; remove those two lines to uninstall.

## Knobs

- `APLEXER_A` env var overrides the `a` binary (default: `a` on PATH, else
  `/home/alexey/.local/bin/a`).
- Exit code is 0 when every send succeeded (or there was nothing to send),
  1 when any `a send` failed, so a failed night is visible in the log.
