# Sulla

Keyboard-first TUI for running coding agents off a [beads](https://github.com/steveyegge/beads)
queue. See [DESIGN.md](DESIGN.md) for the why.

## Running

```sh
cargo run -- -C /path/to/repo        # repo must have `bd init` run in it
cargo run -- --max-agents 5 --base main
```

Requires `git`, `bd`, and either `copilot` or `claude` (Claude Code) on `PATH`.

## How it works

Ready beads are claimed in priority order, up to `max_agents` at once. Each agent works in its own
worktree on `sulla/<bead>`, then moves the bead to Review, where you (or an AI reviewer) comment,
run checks and merge. See [DOCS.md](DOCS.md) for the full lifecycle, stacking, budgets, AI review
and checks.

## Keys

Press `?` in the app for the full list.

| View | Keys |
|---|---|
| Board | `hjkl` `gg` `G` move, `z` zoom focused column (`z`/`Esc` to exit), `/` filter (words match title/id, `label:x`, `p:1`, `p:0-2`; `Esc` clears), `Enter` open, `p` progress, `r` review, `i` inspect bead (`l` there shows the session log, `h` the timeline), `H` bead timeline (time per state, review rounds, ai verdicts), `e` open `$EDITOR` in worktree, `n` new bead, `c`/`C` conversation (resume/fresh), `+`/`-` priority, `X` kill, `D` discard, `F` retry with note, `s` release, `S` release all backlog, `P` pause/resume scheduling, `R` mark review, `V` ai review, `A` launch/resume, `t`/`T` check output/rerun, `q` quit |
| Progress | `i` insert (type into agent), `Esc Esc` or `Ctrl-]` leave insert, `jk` `Ctrl-u/d` `gg` `G` or mouse wheel scroll (forwarded to the agent when it handles the mouse itself), `e` edit worktree |
| Review | `jk` `Ctrl-d/u` scroll, `]f` `[f` files, `c` comment, `C` edit comment, `x` delete comments, `S` send comments, `y`/`n` accept/reject hunk, `Y`/`N` accept/reject file, `m` merge (needs passing checks), `M` force merge, `P` publish as GitHub PR (`workflow = "pr"`), `t`/`T` check output/rerun, `s` send feedback, `V` ai review, `D` discard, `F` retry with note, `p`/`a` swap to agent view (`i` straight into insert, `r` or `Esc` swaps back with comments kept), `e` edit worktree (diff reloads on return) |
| Command | `:q` `:q!` `:new <title>` `:max <n>` `:pause` `:resume` `:refresh` `:kill <id>` `:chat [new]` |

## Config

`~/.config/sulla/config.toml`, all optional. A repo can override it with
`<repo>/.sulla/config.toml` (untracked, `.sulla/` is in `.git/info/exclude`).
The two files are merged key by key: nested tables like `[claude]` merge
recursively, arrays and scalars in the repo file replace the global ones.
CLI flags (`--max-agents`, `--base`) win over both.

```toml
max_agents = 3
done_retention_days = 3
poll_secs = 2
base_branch = "main"
kache = true
check = "cargo test && cargo clippy" # pre-merge check, unset by default
agent = "copilot" # or "claude"
sandbox = "worktree" # or "incus"
workflow = "local"   # or "pr" to land work through GitHub pull requests

[copilot]
bin = "copilot"
model = "claude-opus-4.8"
extra_args = []

[claude]
bin = "claude"
model = "opus"
extra_args = []

# Incus system container sandbox, used when sandbox = "incus"
[incus]
bin = "incus"
golden = "sulla-golden"   # container that slot containers are copied from
snapshot = "clean"        # snapshot on each slot container used for hard resets
prefix = "sulla"          # slot containers are named <prefix>-<repo-slug>-<slot>
storage_pool = "default"
memory = "8GiB"           # limits.memory per slot container
# cpus = 4                # limits.cpu per slot container, unset by default
nofile = 262144           # limits.kernel.nofile
reset_every = 10          # hard reset a slot after this many beads, 0 = never
reset_volumes = true      # light reset also prunes Docker volumes
mounts = ["~/.claude", "~/.copilot"] # host paths shared at the same path
env = {}                  # extra env passed to agents in containers

[notify]
methods = ["bell"]        # any of "bell", "desktop" (notify-send), "tmux"
on = ["review", "stalled", "waiting", "budget"]
cooldown_secs = 60        # min gap between repeat notifications for one bead
notify_send = "notify-send"
tmux_option = "@sulla"    # tmux user option set while beads need attention

[stall]
minutes = 5               # no screen progress for this long marks it stalled, 0 = never
prompts = ["Do you want to", "No, and tell"] # screen text meaning a permission/input prompt

[budget]
minutes = 0               # max minutes per agent run, 0 = unlimited
tokens = 0                # max input + output tokens per agent run, 0 = unlimited

# GitHub pull request settings, used when workflow = "pr"
[pr]
remote = "origin"         # remote that sulla branches are pushed to
draft = false             # open pull requests as drafts
poll_secs = 120           # how often to poll open pull requests
backup_sessions = true    # back up agent sessions so agents can resume on review comments

[presets]                 # prompt guidance picked per bead with a preset:<name> label
perf = "This is a performance change. Measure before and after and report the numbers."
```

Logs go to `~/.local/share/sulla/sulla.log` (`SULLA_LOG=debug` for more).

## Notifications

Sulla notifies when a bead moves to Ready for Review, an agent stalls, an agent goes over its
budget, or an agent rings the terminal bell (waiting on input). Opening the bead's progress or review view clears its flag.
Notifications are skipped for the bead you are currently viewing.

- `bell` rings the terminal bell in the terminal running sulla.
- `desktop` runs `notify-send -a sulla <summary> <title>`.
- `tmux` sets the `@sulla` user option to `sulla:<n>` while beads need attention, and unsets it
  when none do. Show it with e.g. `set -g status-right '#{@sulla} ...'`.

Running agents (and AI reviewers) are watched for trouble. A card shows `?` when the agent is
waiting on input: it rang the bell, or one of the `[stall] prompts` strings is in the bottom rows
of its screen, and it has been quiet for a few seconds. A card shows `⧗` when the agent's screen
text has not meaningfully changed for `[stall] minutes`. Spinners, elapsed timers and counters are
ignored, so an agent stuck spinning on a hung request is still caught. A card shows a bold `✗` when
the bead is in progress but its agent exited (for over 30 seconds) or failed to start. These fire
notifications (`waiting` for the first, `stalled` for the others), and the flag clears once the
agent makes progress again.

Bell-based waiting detection relies on the agent ringing the bell. For Claude Code, run
`claude config set --global preferredNotifChannel terminal_bell`.

## Sandboxes

`sandbox` picks where bead agents run:

- `"worktree"` (default): agents run on the host in their worktree.
- `"incus"`: each agent slot gets its own long-lived Incus system container with its own
  network namespace and Docker daemon, so several agents can run the same `docker compose` stack
  (Postgres, ClickHouse, a web app on the same ports) without conflicts.

Set it globally, or per repo in `<repo>/.sulla/config.toml` (untracked, merged over the global
file key by key):

```toml
sandbox = "incus"
kache = false

[incus]
golden = "sulla-golden"     # instance slot containers are copied from
memory = "8GiB"             # limits.memory per slot
cpus = 4                    # limits.cpu per slot, unset means no limit
nofile = 262144             # limits.kernel.nofile, ClickHouse needs a high limit
reset_every = 10            # hard reset a slot after this many beads, 0 = never
reset_volumes = true        # light reset also prunes Docker volumes
mounts = ["~/.claude", "~/.copilot"]
```

Use `kache = false` in incus repos. kache is usually not installed in the container, and agents
would otherwise get `RUSTC_WRAPPER=kache` and fail to build.

### Prerequisites

1. Incus installed and initialised with `incus admin init`. Use a btrfs storage pool, or ZFS 2.2+,
   so Docker inside the containers can use overlay2.
2. Delegate your uid and gid to root so containers can map them 1:1 (`raw.idmap`). With
   `id -u` and `id -g` both `1000`:

   ```sh
   echo 'root:1000:1' | sudo tee -a /etc/subuid
   echo 'root:1000:1' | sudo tee -a /etc/subgid
   sudo systemctl restart incus
   ```

3. Build the golden container:

   ```sh
   scripts/incus-golden.sh
   ```

   It launches `images:ubuntu/24.04` as `sulla-golden` with nesting enabled, installs Docker and
   the compose plugin, git, build tools, Node, `bd` (same version as on the host), the copilot
   and claude CLIs and rustup, creates a user matching your host user, uid, gid and home path,
   checks `docker run hello-world`, and stops it. It is safe to re-run to refresh the image.
   Overrides: `NAME=...`, `IMAGE=...`, `INSTALL_RUST=0`. The script exits with the exact
   subuid/subgid lines to add if step 2 is missing.

### How slots work

- Slot containers are named `<prefix>-<repo-slug>-<slot>`, copied from the golden container on
  first use and reused across beads. The repo is bind-mounted at the same absolute path, along
  with each `mounts` entry, so worktrees, `.beads` and agent logins work unchanged. Agents run
  the real `bd` inside the container against the shared `.beads`.
- Each slot keeps a persistent Docker volume at `/var/lib/docker`, so pulled images and build
  cache survive resets.
- Light reset after every merged, discarded or killed bead: remove all Docker containers and
  networks (and volumes when `reset_volumes = true`) and kill the agent user's processes.
- Hard reset when a slot has run `reset_every` beads, when a bead is discarded, or when the light
  reset fails: the container is restored to its `clean` snapshot.

### Reaching a slot's services

`incus list` shows each slot container and its IP. Ports published by `docker compose` inside a
slot are reachable from the host at that IP, or by name via Incus DNS as `<container>.incus`
(for example `http://sulla-myrepo-1a2b3c-0.incus:3000`) when your host resolver forwards the
`incus` domain to the Incus bridge.
