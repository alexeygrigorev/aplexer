# PocketShell config migration: `engines.yaml` / `profiles.yaml` -> aplexer TOML

Phase 0 item 0.6 of `docs/pocketshell-integration-plan.md`. This is a
mapping reference, not a converter -- the two shapes are near-isomorphic
enough that hand-editing `~/.config/aplexer/config.toml` from an existing
`~/.config/pocketshell/{engines,profiles}.yaml` is a small, mechanical job.
See that plan doc's section 1.4 (`a profiles --json` row) and section 1.6
for the one genuinely lossy step (the profile-namespace flattening) --
it is not re-derived here, only cross-referenced.

Source shapes below are read directly from
`tools/pocketshell/src/pocketshell/engines.py` (`_manifest_from_mapping`,
`_launch_from_mapping`, `PROVIDER_ENV_UNSET_VARS`) and
`tools/pocketshell/src/pocketshell/profiles.py` (`load_config_profiles`),
both in the `pocketshell` repo (read-only reference for this doc; not
modified by aplexer's Phase 0 work).

## `engines.yaml` -> `[engines.*]`

PocketShell (`~/.config/pocketshell/engines.yaml`):

```yaml
engines:
  - id: myengine
    harness: myharness          # aplexer has no separate harness/id split
    label: "My Engine"          # presentation-only, aplexer doesn't model it
    family: myfamily            # presentation-only, aplexer doesn't model it
    provider_mark: "Provider"   # presentation-only, aplexer doesn't model it
    usage_provider: null        # presentation-only, aplexer doesn't model it
    enabled: true                # aplexer has no enabled/disabled flag
    launch:
      argv: [myharness, --flag]
      skip_permissions_argv: [--yolo]
      env:
        set:
          SOME_VAR: value
        unset: [EXTRA_VAR_TO_STRIP]
      profile_env: MY_HOME
```

aplexer (`~/.config/aplexer/config.toml`):

```toml
[engines.myengine]
command = ["myharness", "--flag"]
env = { SOME_VAR = "value" }
env_unset = ["EXTRA_VAR_TO_STRIP"]
skip_permissions_argv = ["--yolo"]
```

Field-by-field:

| `engines.yaml` (`EngineManifest`/`LaunchSpec`) | aplexer `[engines.<id>]` (`EngineConfig`) | Notes |
| --- | --- | --- |
| `id` | the TOML table key (`[engines.<id>]`) | same role, different syntax |
| `harness` | first element of `command` | aplexer has no separate `harness` vs `id`; `command[0]` is both the lookup name and the argv program |
| `launch.argv` | `command` | full argv, program first |
| `launch.skip_permissions_argv` | `skip_permissions_argv` | direct port (added aplexer-side in Phase 0 item 0.3) |
| `launch.env.set` | `env` | direct port |
| `launch.env.unset` | `env_unset` | direct port, **but** see below -- aplexer forces a union with a hardcoded list regardless of this field |
| `launch.profile_env` | *(none)* | aplexer has no `profile_env` concept on `EngineConfig` itself; a profile's own `env` map sets the config-dir var directly (e.g. `CLAUDE_CONFIG_DIR = "..."`), see profiles below |
| `family`, `label`, `provider_mark`, `usage_provider` | *(none)* | presentation-only fields the plan doc (0.5) deliberately keeps out of aplexer's scope; a pocketshell-side overlay is expected to supply these when consuming `a engines --json` |
| `enabled` | *(none)* | aplexer has no enable/disable; omit the engine entirely from `config.toml` to the same effect (it just won't exist in `a engines --json`) |
| `available` / `unavailable_reason` | *(none, computed)* | aplexer always computes `available` live via `command_exists`; there is no override |

**`env_unset` is NOT purely additive-vs-replacing the same way on both
sides.** PocketShell's `LaunchSpec.__post_init__` already unions any
config-supplied `env_unset` with its own hardcoded
`PROVIDER_ENV_UNSET_VARS` (`_ordered_env_unset_union`); aplexer's
`Config::resolve` does the exact same thing with its own ported copy of
that same list (`PROVIDER_ENV_UNSET_VARS` in `src/lib.rs`). So an
`engines.yaml` entry's `launch.env.unset` list and an aplexer
`[engines.*]` entry's `env_unset` list both mean the same thing: "vars to
strip *in addition to* the built-in provider-key list" -- neither side
lets you shrink that built-in list. No migration adjustment needed here
beyond copying the list verbatim.

## `profiles.yaml` -> `[profiles.*]`

PocketShell (`~/.config/pocketshell/profiles.yaml`):

```yaml
profiles:
  - name: "Claude (Z.AI)"
    engine: claude
    config_dir: ~/.zlaude
    env:
      SOME_EXTRA_VAR: value
```

aplexer (`~/.config/aplexer/config.toml`):

```toml
[profiles.zlaude]
engine = "claude"
env = { CLAUDE_CONFIG_DIR = "/home/you/.zlaude", SOME_EXTRA_VAR = "value" }
```

Field-by-field:

| `profiles.yaml` entry (`Profile`) | aplexer `[profiles.<id>]` (`ProfileConfig`) | Notes |
| --- | --- | --- |
| `name` | the TOML table key (`[profiles.<id>]`) | **lossy** -- see below, this is the one real migration decision |
| `engine` | `engine` | direct port |
| `config_dir` | folded into `env` (e.g. `CLAUDE_CONFIG_DIR = "<config_dir>"` for claude, `CODEX_HOME = "<config_dir>"` for codex) | aplexer has no dedicated `config_dir` field -- a profile's `env` map is where the config-dir env var goes, matching what `Config::resolve` actually does with a profile's `env` (merges it into the launch env) |
| `default` | *(none)* | aplexer deliberately emits no profile entry for an engine's own default dir (see `discover_profiles`'s doc comment in `src/lib.rs`) -- the engine's built-in `command` already resolves there with no override needed, so there is nothing to migrate for a `default: true` entry; only non-default (sibling) profiles need a `[profiles.*]` entry |
| `env` | `env` | direct port, merged with the `config_dir` mapping above |
| *(none)* | `command`, `args`, `cwd`, `history_bytes`, `limits` | aplexer-only extras with no `profiles.yaml` equivalent; leave unset unless you want profile-specific overrides beyond what pocketshell's `Profile` models |

### The one lossy step: profile naming

PocketShell's `Profile.name` is scoped **per engine** (two engines can
each have a profile literally named `"Work"` without collision).
aplexer's `Config.profiles` is a single **flat** namespace shared by every
engine, keyed by the config-dir's own directory stem minus its leading dot
(e.g. `~/.zlaude` -> `"zlaude"`) -- this is deliberate and already
documented in `docs/pocketshell-integration-plan.md` sections 1.4 (the
`a profiles --json` row) and 1.6; see those for the full rationale
(directory-stem keys are collision-free by construction, unlike two
engines both wanting a profile called `"zai"`). When migrating a
`profiles.yaml` entry:

- Pick the aplexer profile id from the directory stem, not `Profile.name`
  (e.g. `config_dir: ~/.zlaude` -> `[profiles.zlaude]`, regardless of what
  the pocketshell entry's `name` field says).
- If two `profiles.yaml` entries (necessarily for different engines, since
  `config_dir` is per-engine) would map to the *same* directory stem, that
  cannot happen in practice -- two different top-level `~/.<name>` dirs can
  never share a name, so the collision the flat namespace is designed to
  avoid does not actually arise from a real `profiles.yaml`.
- The human-readable `Profile.name` (e.g. `"Claude (Z.AI)"`) has no
  aplexer equivalent to migrate into -- `a profiles --json` reports only
  the flat id and `engine`/`env`/etc, no display name. A consumer that
  wants a friendly label (e.g. a future pocketshell adapter) needs to keep
  its own id-to-label map rather than expecting aplexer to carry one.

## `shortcuts` (aplexer-only, no `profiles.yaml`/`engines.yaml` source)

`[shortcuts.<id>]` (`ShortcutConfig`, e.g. `a - cl` / `a - clz`) has no
pocketshell equivalent to migrate from -- it is purely an aplexer human-CLI
convenience (`docs/pocketshell-integration-plan.md` 1.4 notes PocketShell
never calls this path). Nothing to do here for a migration.
