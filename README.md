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
- Beads in Backlog are never auto scheduled. Pick Backlog or Pending in the new bead popup, `B` moves a bead to Backlog and `s` releases it to Pending.
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
| Board | `hjkl` `gg` `G` move, `Enter` open, `p` progress, `r` review, `i` inspect bead, `e` open `$EDITOR` in worktree, `n` new bead, `c`/`C` conversation (resume/fresh), `+`/`-` priority, `X` kill, `D` discard, `s` release, `R` mark review, `A` launch/resume, `q` quit |
| Progress | `i` insert (type into agent), `Esc Esc` or `Ctrl-]` leave insert, `jk` `Ctrl-u/d` `gg` `G` or mouse wheel scroll (forwarded to the agent when it handles the mouse itself), `e` edit worktree |
| Review | `jk` `Ctrl-d/u` scroll, `]f` `[f` files, `m` merge, `s` send feedback, `D` discard, `p`/`a` swap to agent view (`i` straight into insert, `r` or `Esc` swaps back with comments kept), `e` edit worktree (diff reloads on return) |
| Command | `:q` `:q!` `:new <title>` `:max <n>` `:refresh` `:kill <id>` `:chat [new]` |

## Config

`~/.config/sulla/config.toml`, all optional:

```toml
max_agents = 3
done_retention_days = 3
poll_secs = 2
base_branch = "main"
kache = true
agent = "copilot" # or "claude"

[copilot]
bin = "copilot"
model = "claude-opus-4.8"
extra_args = []

[claude]
bin = "claude"
model = "opus"
extra_args = []
```

Logs go to `~/.local/share/sulla/sulla.log` (`SULLA_LOG=debug` for more).
