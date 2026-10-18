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
- Agents still marked running when sulla restarts are resumed via their agent session.

## Keys

Press `?` in the app for the full list.

| View | Keys |
|---|---|
| Board | `hjkl` `gg` `G` move, `Enter` open, `p` progress, `r` review, `n` new bead, `+`/`-` priority, `X` kill, `D` discard, `s` release, `R` mark review, `A` launch/resume, `q` quit |
| Progress | `i` insert (type into agent), `Esc Esc` or `Ctrl-]` leave insert, `jk` `Ctrl-u/d` `gg` `G` scroll |
| Review | `jk` `Ctrl-d/u` scroll, `]f` `[f` files, `m` merge, `s` send feedback, `D` discard |
| Command | `:q` `:q!` `:new <title>` `:max <n>` `:refresh` `:kill <id>` |

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
