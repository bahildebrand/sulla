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
- Raw PTY output of every bead agent is appended to `.sulla/logs/<bead>.log` (AI reviewers to
  `<bead>~review.log`), so transcripts survive crashes, merges and restarts. `l` in the inspect
  popup renders the last 5000 lines.
- Board state lives in beads labels: `sulla:backlog`, `sulla:running`, `sulla:reviewing`, `sulla:review`, `sulla:held`.
- Each board card shows an activity line: time in the current state, the worktree diff stat
  (`+adds -dels Nf`, refreshed every poll), and the last line of agent output.
- Beads in Backlog are never auto scheduled. Pick Backlog or Pending in the new bead popup, `B` moves a bead to Backlog, `s` releases it to Pending and `S` releases the whole Backlog.
  The scheduler only starts Pending beads whose dependencies are closed. Blocked beads show `⊘`.
- Stacked beads: a Pending bead whose only open blocker is in Review (or under AI review) is
  started on a branch forked from the blocker's branch instead of base. The fork point is kept in
  `refs/sulla/stack/<bead>`, so its diff, log and AI review only show its own commits. It still
  cannot be merged until the blocker is closed. When the blocker is merged (or closed outside
  sulla after landing in base) its stacked children are rebased onto base, and their children
  onto the new branches, down the chain. Children with a live agent or AI reviewer are rebased
  once it stops. A child with uncommitted changes or conflicts is left stacked and retried once
  its HEAD or working tree changes, or rebased onto base when it is merged. Discarding a bead, or closing it without merging, unstacks its
  children so they keep its commits in their diff.
- `A` on a blocked bead without a worktree does not launch it unless it can be stacked. It is
  released to Pending and the scheduler starts it once its blockers close.
- Agents are told to run `bd set-state <bead> sulla=review` when done.
- `V` on a bead in Review starts an AI reviewer agent in the same worktree with a fresh session.
  The bead is labeled `sulla:reviewing` and shown in In Progress with `◎` while it runs. The
  reviewer posts line-anchored comments as a JSON bead comment and moves the bead back to
  Review. It shares the implementer's slot when the implementer still holds one, so it can start
  even when every slot is busy. Otherwise it takes its own slot and counts toward `max_agents`.
  Resets of a shared slot wait until both agents are gone. `p` watches it, `X` stops it and
  returns the bead to Review. Reviewing beads without a live reviewer on restart go back to Review.
- When a reviewer moves its bead back to Review, sulla reads the newest bead comment
  (`bd comments <bead> --json`) and parses the review JSON (a code fence around it is fine). Each
  comment is anchored to the matching diff line, exactly as if you had commented on that line.
  Comments that cannot be anchored become file-level comments on their file, or on `COMMIT_MSG`
  when the file is not in the diff. With verdict `changes` and at least one comment, the
  comments are sent to the implementer in the same JSON format as your own `S` feedback and the
  bead goes back to In Progress. With `approve` (or no comments) it stays in Review. Missing or
  unparseable output leaves the bead in Review with a status message.
- The board card and the Review header show `✓ai` (approve) or `✎ai` (changes). In the Review
  view AI comments appear inline in cyan, prefixed `ai:` (or `ai (sent):` once forwarded). All
  of them can be kept, edited with `C`, deleted with `x`, and sent with `S` along with your own.
  Comments whose line is no longer in the diff move to their file header, or to the first file
  when the file is gone. They are cleared when a new AI review starts, when you send feedback,
  or when the bead goes back to running for another reason.
- With `check` set, sulla runs it with `sh -c` in the bead worktree whenever a bead in Review
  has a new branch head or `git status --porcelain`, killing any run on an older state. Only one
  check runs at a time, the rest queue. With `sandbox = "incus"` it runs inside the bead's
  container. If the bead has no container (e.g. after a restart) nothing starts automatically and
  `T` runs it on the host. Output goes to `.sulla/checks/<bead>.log` and the result to
  `<bead>.json`, reused on restart when head and status still match. Cards show `…chk`, `⟳chk`,
  `✓chk` or `✗chk` and the Review header shows the result and duration. `m` refuses to merge
  while checks are failing, queued, running, stale or missing, or the worktree has uncommitted
  changes. `M` force merges anyway. `t` shows the output, `T` reruns.
- Merging rebases the branch onto base and fast-forwards. No merge commits. The worktree and
  branch are removed and the bead is closed.
- Each poll runs `git merge-tree` on every open bead's committed branch against base and against
  every other open bead branch. Beads that would conflict show `⚠` on their card. The zoomed card
  and the Review header name what they conflict with, and conflicting files are marked `⚠` in
  the Review file list. Uncommitted worktree changes are not checked.
- Discarding deletes the worktree and branch and holds the bead, so it can be re-run from scratch.
- Killing an agent holds its bead in Pending until released with `s`.
- When an agent's recent output shows a 429, rate limit, or quota error, sulla stops launching
  new agents for 1 minute, doubling on repeat hits up to 30 minutes. Running agents are left
  alone. The status bar shows `RATE LIMITED` with the time left, and `:resume` lifts it early.
- `e` suspends sulla and opens `$VISUAL`, `$EDITOR` or `nvim` in the bead's worktree. Agents keep
  running meanwhile, so edits may conflict with a live agent.
- Agents still marked running or in review when sulla restarts are resumed via their agent
  session, so review beads keep their slots ahead of pending work.
- `c` opens a conversation agent in the main checkout, not tied to any bead. Its first prompt
  explains the sulla states and how to create beads (staged in Backlog, wired with `bd dep add`,
  released to Pending on your go), so you can plan work and have it fill the queue. It does not
  count toward `max_agents`. `X` ends it, `C` starts a fresh one.
- Token usage is read from the agent's session files (`~/.copilot/session-state/<id>/events.jsonl`
  or `~/.claude/projects/*/<id>.jsonl`). Each bead sums its implementer and AI reviewer sessions,
  shown on the zoomed card and in `i` inspect. Copilot reports cost in AI credits (`cr`), Claude
  in USD when its transcript includes it. The status bar shows usage since sulla started.

## Keys

Press `?` in the app for the full list.

| View | Keys |
|---|---|
| Board | `hjkl` `gg` `G` move, `z` zoom focused column (`z`/`Esc` to exit), `/` filter (words match title/id, `label:x`, `p:1`, `p:0-2`; `Esc` clears), `Enter` open, `p` progress, `r` review, `i` inspect bead (`l` there shows the session log), `e` open `$EDITOR` in worktree, `n` new bead, `c`/`C` conversation (resume/fresh), `+`/`-` priority, `X` kill, `D` discard, `s` release, `S` release all backlog, `P` pause/resume scheduling, `R` mark review, `V` ai review, `A` launch/resume, `t`/`T` check output/rerun, `q` quit |
| Progress | `i` insert (type into agent), `Esc Esc` or `Ctrl-]` leave insert, `jk` `Ctrl-u/d` `gg` `G` or mouse wheel scroll (forwarded to the agent when it handles the mouse itself), `e` edit worktree |
| Review | `jk` `Ctrl-d/u` scroll, `]f` `[f` files, `c` comment, `C` edit comment, `x` delete comments, `S` send comments, `m` merge (needs passing checks), `M` force merge, `t`/`T` check output/rerun, `s` send feedback, `V` ai review, `D` discard, `p`/`a` swap to agent view (`i` straight into insert, `r` or `Esc` swaps back with comments kept), `e` edit worktree (diff reloads on return) |
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
on = ["review", "stalled", "waiting"]
cooldown_secs = 60        # min gap between repeat notifications for one bead
notify_send = "notify-send"
tmux_option = "@sulla"    # tmux user option set while beads need attention
```

Logs go to `~/.local/share/sulla/sulla.log` (`SULLA_LOG=debug` for more).

## Notifications

Sulla notifies when a bead moves to Ready for Review, an agent stalls, or an agent rings the
terminal bell (waiting on input). Opening the bead's progress or review view clears its flag.
Notifications are skipped for the bead you are currently viewing.

- `bell` rings the terminal bell in the terminal running sulla.
- `desktop` runs `notify-send -a sulla <summary> <title>`.
- `tmux` sets the `@sulla` user option to `sulla:<n>` while beads need attention, and unsets it
  when none do. Show it with e.g. `set -g status-right '#{@sulla} ...'`.

Waiting detection relies on the agent ringing the bell. For Claude Code, run
`claude config set --global preferredNotifChannel terminal_bell`.

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
