# Sulla

Keyboard-first TUI for running coding agents off a [beads](https://github.com/steveyegge/beads)
queue. See [DESIGN.md](DESIGN.md) for the why.

## Running

```sh
cargo run -- init                    # write .sulla/config.toml for the current repo
cargo run
cargo run -- -C /path/to/repo        # -C points sulla at another repo, like git -C
cargo run -- --max-agents 5 --base main
cargo run -- setup <bead>            # rerun [hooks] setup in a bead's worktree
```

The repo must be a git repo. If it has no beads database yet, `sulla init` creates one, a Dolt
SQL server by default (`bd init --server`), and for an existing embedded database it prints how to
migrate without losing data (it never migrates on its own). It adds `.sulla/` to
`.git/info/exclude`, then asks for the agent, model, max agents, base branch, workflow, sandbox, kache, check command and worktree setup command.
Press enter to keep the current value. Answers that match the global config are not pinned in
the repo file, other keys already in `.sulla/config.toml` are kept.

Requires `git`, `bd`, and either `copilot` or `claude` (Claude Code) on `PATH`, plus `dolt` for a
server-mode beads database. In server mode sulla starts the Dolt server and `bd serve` and reads
beads over HTTP instead of spawning `bd` for every call, see
[DOCS.md](DOCS.md#beads-database).

## How it works

Ready beads are claimed in priority order, up to `max_agents` at once. Each agent works in its own
worktree on `<branch_prefix><bead>` (`sulla/<bead>` by default), then moves the bead to Review, where you (or an AI reviewer) comment,
run checks and merge. See [DOCS.md](DOCS.md) for the full lifecycle, stacking, budgets, AI review
and checks.

## Keys

Press `?` in the app for the full list.

| View | Keys |
|---|---|
| Board | `hjkl` `gg` `G` move, `z` zoom focused column (`z`/`Esc` to exit), `/` filter (words match title/id, `label:x`, `p:1`, `p:0-2`; `Esc` clears), `Enter` open, `p` progress, `r` review, `i` inspect bead (`l` there shows the session log, `h` the timeline), `H` bead timeline (time per state, review rounds, ai verdicts), `e` open `$EDITOR` in worktree, `E` shell in the container or worktree, `n` new bead, `c`/`C` conversation (resume/fresh), `+`/`-` priority, `X` kill, `D` discard, `dd` delete bead, `F` retry with note, `s` release, `S` release all backlog, `P` pause/resume scheduling, `R` mark review, `V` ai review, `A` launch/resume, `t`/`T` check output/rerun, `U` revise from PR feedback (also in the review), `w` release the worktree of a Review or Awaiting PR bead so you can work on its branch (`sulla:released`, also in the review), `w` again reclaims it once the branch is not checked out elsewhere, `q` quit |
| Progress | `i` insert (type into agent), `Esc Esc` or `Ctrl-]` leave insert, `jk` `Ctrl-u/d` `gg` `G` or mouse wheel scroll (forwarded to the agent when it handles the mouse itself), `e` edit worktree, `E` shell |
| Review | `jk` `Ctrl-d/u` scroll (counts like `5j` work), `]f` `[f` files, `c` comment (`10c` covers ten lines from the cursor), `C` edit comment, `x` delete comments, `S` send comments, `y`/`n` accept/reject hunk, `Y`/`N` accept/reject file, `m` merge (needs passing checks), `M` force merge, `P` publish as GitHub PR, with its unpublished stack parents in Review (`workflow = "pr"`), `w` release the worktree to work on the branch yourself (`w` again reclaims it), `t`/`T` check output/rerun, `s` send feedback, `V` ai review, `D` discard, `F` retry with note, `p`/`a` swap to agent view (`i` straight into insert, `r` or `Esc` swaps back with comments kept), `e` edit worktree, `E` shell (diff reloads on return from either) |
| Command | `:q` `:q!` `:new <title>` `:max <n>` `:pause` `:resume` `:refresh` `:kill <id>` `:setup [id]` `:chat [new]` `:browse [bead\|slot]` |

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
branch_prefix = "sulla/" # bead branches are this plus the bead id
kache = true
check = "cargo test && cargo clippy" # pre-merge check, unset by default
agent = "copilot" # or "claude"
sandbox = "worktree" # or "incus"
workflow = "local"   # or "pr" to land work through GitHub pull requests
system_prompt = """
Use pnpm, not npm. Run `make lint` before committing.
"""

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

# Host browser access to slot apps, see "Browsing a slot's app"
[incus.browse]
ports = []                # container ports forwarded to the slot's host loopback IP
hosts = []                # host patterns the slot's browser resolves to that IP
# url = "http://app.example.test:8000" # page `sulla browse` opens, {ip} is substituted

[notify]
methods = ["bell"]        # any of "bell", "desktop" (notify-send), "tmux"
on = ["review", "stalled", "waiting", "budget", "pr"]
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
reply = false             # comment on the PR with what a revision addressed when republishing

[hooks]
setup = "npm ci"          # run once in each new worktree before its agent starts, unset by default

[presets]                 # prompt guidance picked per bead with a preset:<name> label
perf = "This is a performance change. Measure before and after and report the numbers."
```

Logs go to `~/.local/share/sulla/sulla.log` (`SULLA_LOG=debug` for more).

## Notifications

Sulla notifies when a bead moves to Ready for Review, an agent stalls, an agent goes over its
budget, an agent rings the terminal bell (waiting on input), or a watched PR gets new comments,
failing checks or is closed without merging (`pr`). Opening the bead's progress or review view clears its flag.
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
- Before every bead sulla makes sure the container has a user with your uid whose primary group
  has your gid, creating or fixing the user and group if needed. Sessions run as that user
  through `setpriv --init-groups`, so they get its supplementary groups, such as `docker` from
  the golden script, without `sg` or `sudo`.
- With a server-mode beads database, each slot gets an Incus proxy device (`beads-dolt`) that
  listens on the container's `127.0.0.1:<dolt port>` and forwards to the host's Dolt server on
  `127.0.0.1`, so `bd` in the container uses the port in `.beads/dolt-server.port` unchanged.
  The server stays bound to the host's loopback, so it is not reachable on the Incus bridge or
  beyond. sulla also sets `BEADS_DOLT_AUTO_START=0` in the container, so `bd` there never starts
  a second server over the shared data directory. Before every bead sulla starts the host Dolt
  server if it is down (and fails the slot if it cannot), then checks the device and updates it
  if the server's port changed.
- Each slot keeps a persistent Docker volume at `/var/lib/docker`, so pulled images and build
  cache survive resets.
- Light reset after every merged, discarded or killed bead: remove all Docker containers and
  networks (and volumes when `reset_volumes = true`) and kill the agent user's processes.
- Hard reset when a slot has run `reset_every` beads, when a bead is discarded, or when the light
  reset fails: the container is restored to its `clean` snapshot.
- Before every bead sulla reconciles the slot's disk devices against `mounts`, so editing the list
  takes effect on the next bead without recreating the container: missing paths are mounted,
  and the `mount<N>` devices sulla owns are removed when their path was dropped or their source
  no longer matches. Other disk devices are left alone, and one already mounted at a listed path
  counts as that mount. A `mounts` entry that does not exist on the host is skipped with a
  warning in the status line. Mounts use the container's `raw.idmap`, so files keep your uid and
  gid.

### Shell init

`[incus] init` is a bash snippet run in the worktree before every command sulla execs in a slot
(agents, AI reviewers and checks), for setup a static `env` cannot express, like activating a
conda environment or putting a worktree-relative directory on `PATH`. The command is then started
with `exec` from that shell, so exported variables carry over. If the snippet fails, the command
still starts. Before every bead sulla also runs the snippet once in the repo and, if a command
in it fails outside a condition such as `|| true` or `if`, shows the first error in the status
line.

```toml
[incus]
mounts = ["~/.claude", "~/.copilot", "~/micromamba", "~/.local/bin/micromamba"]
init = '''
eval "$("$HOME/.local/bin/micromamba" shell hook -s bash)" && micromamba activate myenv
export PATH="$PWD/.venv/bin:$PATH"
'''
```

### Reaching a slot's services

`incus list` shows each slot container and its IP. Ports published by `docker compose` inside a
slot are reachable from the host at that IP, or by name via Incus DNS as `<container>.incus`
(for example `http://sulla-myrepo-1a2b3c-0.incus:3000`) when your host resolver forwards the
`incus` domain to the Incus bridge.

### Browsing a slot's app

Apps that hardcode hostnames and ports (in CORS lists, dev proxies, Vite config) cannot be moved
to other host ports, so sulla gives each slot its own loopback IP instead and keeps the ports:

```toml
[incus.browse]
ports = [8000, 8001, 8002]
hosts = ["*.memfault.test"]
url = "http://app.memfault.test:8000"
```

- Slot `N` gets `127.0.100.<N+1>` (slot 0 is `127.0.100.1`). Before every bead sulla adds an
  Incus proxy device `browse-<port>` per port that listens on that IP on the host and connects
  to `127.0.0.1:<port>` in the container, and removes `browse-*` devices for ports no longer
  listed. Linux routes all of `127.0.0.0/8` to loopback, so nothing needs configuring on the
  host, and anything already on `127.0.0.1:<port>` (a host dev stack) is untouched. A device that
  cannot be added (say the address is taken) is shown as a slot warning and the bead still starts.
- `sulla browse <slot|bead>` (or `:browse [bead|slot]` in the TUI, defaulting to the selected
  bead) starts Chrome with its own profile in `.sulla/browser/slot-<N>`,
  `--host-resolver-rules="MAP <host> 127.0.100.<N+1>"` for each `hosts` entry, and opens `url`
  (default `http://<ip>:<first port>`). The separate profile is what makes Chrome honour the
  resolver rules while your normal browser is open, and keeps each slot's cookies apart. The
  binary is `$SULLA_BROWSER`, else the first of `google-chrome`, `chromium`, `chromium-browser`
  on `PATH`.
- A bead is resolved to its slot through `.sulla/slots.json`, which the running sulla keeps up to
  date. `sulla status` prints each slot with its container, loopback IP, the beads it holds and
  its URL, and the board shows `host <ip>` next to the container of a bead with a slot.
- Two repos running incus sulla at once would want the same IPs, so only one of them gets the
  browse devices.
