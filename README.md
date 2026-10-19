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

- Ready beads are claimed in priority order until `max_agents` agents are in progress.
- Each agent runs `copilot --yolo` (or `claude --dangerously-skip-permissions` with `agent = "claude"`) in a PTY inside `.sulla/worktrees/<bead>` on branch
  `sulla/<bead>`, with `RUSTC_WRAPPER=kache`.
- Board state lives in beads labels: `sulla:backlog`, `sulla:running`, `sulla:review`, `sulla:held`.
- Beads in Backlog are never auto scheduled. Pick Backlog or Pending in the new bead popup, `B` moves a bead to Backlog, `s` releases it to Pending and `S` releases the whole Backlog.
  The scheduler only starts Pending beads whose dependencies are closed. Blocked beads show `⊘`.
- `A` on a blocked bead without a worktree does not launch it. It is released to Pending and
  the scheduler starts it once its blockers close.
- Agents are told to run `bd set-state <bead> sulla=review` when done.
- Merging rebases the branch onto base and fast-forwards. No merge commits. The worktree and
  branch are removed and the bead is closed.
- Discarding deletes the worktree and branch and holds the bead, so it can be re-run from scratch.
- Killing an agent holds its bead in Pending until released with `s`.
- `e` suspends sulla and opens `$VISUAL`, `$EDITOR` or `nvim` in the bead's worktree. Agents keep
  running meanwhile, so edits may conflict with a live agent.
- Agents still marked running when sulla restarts are resumed via their agent session.
- `c` opens a conversation agent in the main checkout, not tied to any bead. Its first prompt
  explains the sulla states and how to create beads (staged in Backlog, wired with `bd dep add`,
  released to Pending on your go), so you can plan work and have it fill the queue. It does not
  count toward `max_agents`. `X` ends it, `C` starts a fresh one.

## Keys

Press `?` in the app for the full list.

| View | Keys |
|---|---|
| Board | `hjkl` `gg` `G` move, `z` zoom focused column (`z`/`Esc` to exit), `Enter` open, `p` progress, `r` review, `i` inspect bead, `e` open `$EDITOR` in worktree, `n` new bead, `c`/`C` conversation (resume/fresh), `+`/`-` priority, `X` kill, `D` discard, `s` release, `S` release all backlog, `R` mark review, `A` launch/resume, `q` quit |
| Progress | `i` insert (type into agent), `Esc Esc` or `Ctrl-]` leave insert, `jk` `Ctrl-u/d` `gg` `G` or mouse wheel scroll (forwarded to the agent when it handles the mouse itself), `e` edit worktree |
| Review | `jk` `Ctrl-d/u` scroll, `]f` `[f` files, `m` merge, `s` send feedback, `D` discard, `p`/`a` swap to agent view (`i` straight into insert, `r` or `Esc` swaps back with comments kept), `e` edit worktree (diff reloads on return) |
| Command | `:q` `:q!` `:new <title>` `:max <n>` `:refresh` `:kill <id>` `:chat [new]` |

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
agent = "copilot" # or "claude"
sandbox = "worktree" # or "incus"

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
```

Logs go to `~/.local/share/sulla/sulla.log` (`SULLA_LOG=debug` for more).

## Sandboxes

`sandbox` picks where bead agents run:

- `"worktree"` (default): agents run on the host in their worktree, as described above.
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
