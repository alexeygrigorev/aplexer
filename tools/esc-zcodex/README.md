# esc-zcodex

Sends Esc to every live aplexer session that is running zcodex, every night
at 03:00 Europe/Berlin (a systemd user timer, so it still fires after a
reboot that skipped the slot, thanks to `Persistent=true`).

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

Output goes to stdout; under systemd it lands in the journal:

    journalctl --user -u esc-zcodex.service

## Install / uninstall

    mkdir -p ~/.config/systemd/user
    cp units/*.service units/*.timer ~/.config/systemd/user/
    systemctl --user daemon-reload
    systemctl --user enable --now esc-zcodex.timer

    systemctl --user list-timers esc-zcodex.timer   # verify next fire time

Uninstall with `systemctl --user disable --now esc-zcodex.timer` and remove
the two unit files.

## Knobs

- `APLEXER_A` env var overrides the `a` binary (default: `a` on PATH, else
  `/home/alexey/.local/bin/a`).
- Exit code is 0 when every send succeeded (or there was nothing to send),
  1 when any `a send` failed, so a failed night is visible in the journal.
