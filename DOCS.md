# How sulla works

See [README.md](README.md) for setup, keys and config.

## Bead lifecycle

Board state lives in beads labels: `sulla:backlog`, `sulla:running`, `sulla:reviewing`,
`sulla:review`, `sulla:held`, `sulla:pr`, `sulla:revise`.

- **Backlog** beads are never auto scheduled. The `n` popup creates beads in Backlog or Pending,
  `B` moves a bead to Backlog, `s` releases it to Pending, `S` releases the whole Backlog.
- **Pending** beads are claimed in priority order until `max_agents` agents are in progress. Only
  beads whose dependencies are closed start. Blocked beads show `⊘`. `A` on a blocked bead without
  a worktree only releases it to Pending, unless it can be stacked.
- **Running**: the agent works in `.sulla/worktrees/<bead>` on its own branch (`<branch>` below)
  and is told to run `bd set-state <bead> sulla=review` when done. The branch is `branch_prefix`
  (default `sulla/`) followed by up to three words of the bead title joined by `-`, skipping filler
  words like `the` and `to` and capped at 24 characters (`Fix the login bug` becomes
  `sulla/fix-login-bug`). A name already used by a local or remote branch or another bead gets
  `-2`, `-3` and so on. The name is picked once, when the worktree is first created, and kept in
  `sulla.<bead>.branch` in the git config, so renaming the bead or changing the prefix later does
  not move it. Branches created before naming existed stay `sulla/<bead>`.
- **Review**: you read the diff, comment, run checks, request an AI review, and merge.
- **Merge** rebases the branch onto base and fast-forwards (no merge commits), removes the
  worktree and branch, and closes the bead.
- **Publish** (`P` in review, `workflow = "pr"` only) pushes the branch and opens a GitHub PR
  instead of merging. The bead stays open in **Awaiting PR** (`sulla:pr`, plus `pr:<number>`) with
  its worktree and agent session intact, while the agent is stopped. Beads in `sulla:pr` never
  count toward `max_agents`, are never scheduled and are not resumed on restart.
- **Revise** (`sulla:revise`): an Awaiting PR bead with new PR feedback is picked like any other
  pending bead (see [Revising from PR feedback](#revising-from-pr-feedback)). A bead with a
  `pr:<number>` label stays in the Awaiting PR column while it is queued, revising or revised.
- **Discard** (`D`) deletes the worktree and branch and holds the bead so it can rerun from
  scratch. **Retry** (`F`) does the same but asks why the attempt failed, appends that to the
  bead's notes (which the agent prompt includes) and returns it to Pending.
- **Delete** (`dd` on the board) does what Discard does, then permanently deletes the bead with
  `bd delete --force`. Dependents are orphaned rather than deleted.
- **Kill** (`X`) holds the bead in Pending until released with `s`.
- On restart, running agents are resumed via their agent session. Review beads are resumed only
  into slots that no pending, stackable or revise work wants, so a restart never puts parked
  review agents ahead of ready work.

### Workflow

`workflow = "local"` (the default) merges finished branches straight into the base branch as
described above. `workflow = "pr"` opts a repo into landing work through GitHub pull requests via
the `gh` CLI, configured by the `[pr]` table:

- `tool` (default `"gh"`): `"gh"` publishes with `gh`, `"graphite"` publishes with the Graphite `gt`
  CLI (see [Publishing](#publishing)). Watching and revising still use `gh`, so both must be
  installed, and the repo must have run `gt init`.
- `remote` (default `"origin"`): remote that sulla branches are pushed to.
- `draft` (default `false`): open pull requests as drafts.
- `poll_secs` (default `120`): how often open pull requests are polled.
- `backup_sessions` (default `true`): back up agent sessions so agents can resume on review
  comments (see [Session backups](#session-backups)).
- `auto_revise` (default `true`): queue a revision when a PR gets new comments or failing checks.
  When `false`, sulla only notifies and `U` on the board queues it.
- `reply` (default `false`): when a revised bead is published again, comment on the PR with the
  pushed commit and what it addressed.

### Stacked beads

A Pending bead whose only open blocker is in Review (or under AI review) starts on a branch forked
from the blocker's branch. With `workflow = "pr"` it also stacks on a blocker that is Awaiting PR,
so a chain of dependent beads becomes a chain of stacked PRs. The fork point is kept in
`refs/sulla/stack/<bead>`, so its diff, log and AI review show only its own commits. It can be
published while its blocker is Awaiting PR (the PR targets the blocker's branch), or together
with blockers still in Review (see [Publishing](#publishing)), but it cannot merge locally until
the blocker is closed.

When the blocker merges (or is closed after landing in base), its children are rebased onto base,
and their children onto the new branches, down the chain. Children with a live agent or reviewer
are rebased once it stops. A child with uncommitted changes or conflicts stays stacked and is
retried when its HEAD or working tree changes, or rebased when it merges. Discarding a blocker, or
closing it without merging, unstacks its children so they keep its commits in their diff.

## Agents

- Each agent runs `copilot --yolo` or `claude --dangerously-skip-permissions` in a PTY with
  `RUSTC_WRAPPER=kache` (when `kache = true`).
- If Copilot rejects a prompt sulla sent (at launch or as feedback) with "Managed account policy
  is still loading", sulla resubmits it a few seconds later.
- Labels override config per bead: `agent:copilot`/`agent:claude`, `model:<name>`. The AI
  reviewer uses the same agent and model.
- `preset:<name>` adds approach guidance to the prompt. Built-ins: `bugfix`, `feature`,
  `refactor`, `tests`, `docs`. `[presets]` in config overrides or adds more. The `n` popup sets
  agent and preset labels.
- `system_prompt` in config holds your own instructions, appended to every prompt sulla starts
  an agent with: bead tasks, AI reviews, rebuilt PR sessions and `c` conversations. Set it in
  `.sulla/config.toml` for repo-specific rules, or globally for rules that apply everywhere.
  Edits apply after restarting sulla, and agents keep the prompt they were started with.
- `c` opens a conversation agent in the main checkout, not tied to a bead and not counted toward
  `max_agents`. It is primed to plan work and create beads (staged in Backlog, wired with
  `bd dep add`, released on your go). `X` ends it, `C` starts fresh.
- `e` suspends sulla and opens `$VISUAL`, `$EDITOR` or `nvim` in the worktree. Agents keep
  running, so edits may conflict.
- `E` suspends sulla and drops into a shell for the bead. With `sandbox = "incus"` it runs
  `bash` in the bead's container (as your user, in the worktree, after `[incus] init`), otherwise `$SHELL`
  in the worktree on the host. A bead with no container left gets a host shell. Exit the shell to
  return, the review diff reloads like after `e`.

### Budgets

`[budget]` caps each run in minutes and/or tokens (input + output). Labels `budget:minutes=45` or
`budget:tokens=500k` override it (`k`/`M` suffixes, `0` = unlimited). Counters restart each time
the bead enters running. Over budget, sulla kills the agent and holds the bead: the card shows a
red `⏸` with the reason, a `budget` notification fires, and `s` releases it for a fresh run. The
zoomed card and `i` show e.g. `budget 12/30m 150.0k/500.0k tok`.

### Rate limits

When recent agent output shows a 429, rate limit or quota error, sulla stops launching agents for
1 minute, doubling on repeats up to 30. Running agents are left alone. The status bar shows
`RATE LIMITED` and `:resume` lifts it early.

### Tokens and logs

- Token usage comes from session files (`~/.copilot/session-state/<id>/events.jsonl` or
  `~/.claude/projects/*/<id>.jsonl`), summed over a bead's implementer and reviewer sessions.
  Copilot reports AI credits (`cr`), Claude USD when available. The status bar shows usage since
  startup.
- Raw PTY output goes to `.sulla/logs/<bead>.log` (reviewers to `<bead>~review.log`) and survives
  crashes, merges and restarts. `l` in the inspect popup shows the last 5000 lines.

## Review

A bead in Review keeps its agent and slot (its container with `sandbox = "incus"`) so feedback,
checks and AI reviews reuse a warm session. These parked agents do not count toward `max_agents`,
and they give up their slot on demand: when ready work cannot start because every free slot is
held by a parked agent, sulla stops the agent parked longest (by time in Review) for each slot
needed and the status line shows `freed slot from parked <bead> for <bead>`. The slot gets the
same light reset as after a kill. The bead stays in Review with its worktree, branch and agent
session, and its agent is resumed into a free slot once no ready work wants one. A bead is never
evicted while its checks are queued or running, an AI review is launching or running, a publish
is in flight, or you are typing into its agent.

Feedback (`S`, forwarded AI comments) and `A` on an evicted bead resume its session, evicting
another parked agent if no slot is free. When every slot holds a working agent, the status line
says so and the bead stays in Review.

### AI review

`V` starts a reviewer agent in the bead's worktree with a fresh session. The bead is labeled
`sulla:reviewing` and shown in In Progress with `◎`. The reviewer shares the implementer's slot if
it still holds one, otherwise it takes its own. `p` watches it, `X` stops it and returns the bead to
Review. On restart, reviewing beads without a live reviewer go back to Review.

When done, the reviewer posts a JSON bead comment and moves the bead to Review. Sulla parses the
newest comment (code fences are fine) and anchors each comment to its diff line, falling back to
the file header, or `COMMIT_MSG` if the file is not in the diff.

- Verdict `changes` with comments: comments go to the implementer like your own `S` feedback and
  the bead returns to In Progress.
- `approve` or no comments: the bead stays in Review.
- Missing or unparseable output: stays in Review with a status message.

Cards and the Review header show `✓ai` or `✎ai`. AI comments appear inline in cyan as `ai:` (or
`ai (sent):`) and can be edited (`C`), deleted (`x`) or sent (`S`) with yours. They are cleared on
a new AI review, when you send feedback, or when the bead goes back to running.

### Publishing

With `workflow = "pr"`, `P` publishes the reviewed bead instead of `m`. Pending changes are
committed and rejected hunks dropped as on merge, then `<branch>` is pushed to the `[pr]`
remote with `--set-upstream` and `gh pr create --head <branch> --base <base>` opens a PR (a
draft when `draft = true`). The PR takes its title from the subject of the branch's first commit
and its body from that commit's body, so a commit written to the repo's PR conventions becomes the
PR description. When that commit's body is empty, the bead description is used as the body
(followed by `Bead: <bead>`). When the first commit is the one sulla made for uncommitted changes
(`<bead>: <title>`), the bead title and description are used instead. If an open PR
already exists for the branch it is reused. A bead stacked on a parent that has an open PR (in
Awaiting PR, including while that PR is being revised) targets the parent's branch instead of base. The bead gets `pr:<number>`, moves to Awaiting PR and
its agent and reviewer stop, leaving the worktree and session for later rounds. Publishing again
force pushes with `--force-with-lease`. Cards show the PR number as `#<number>`. With
`workflow = "local"`, `P` only shows a status message.

Publishing a stacked bead whose stack parent is still in Review publishes the stack below it too.
sulla walks up the stack parents while each is open, in Review, has a branch and is not already
being published, stopping at the first parent that is Awaiting PR (or at base). If no open blocker
of any bead on that chain is left besides its own stack parent, the prompt names every branch
(`Submit sulla/a, sulla/b with Graphite as a stack onto <base>?`) and they are published one after
another from the bottom, each exactly as if `P` had been pressed on it once the one below was
Awaiting PR: the bottom one targets the Awaiting PR parent's branch or base, every other one
targets the branch below it. Rejected hunks in each bead's review are dropped. If one fails, the
beads above it are not published and stay in Review. A stack parent without a PR in any other
state (running, under AI review, held) still blocks the publish, and the status line names it.

With `[pr] tool = "graphite"`, publishing uses the Graphite `gt` CLI instead of `git push` and
`gh pr create`, so stacked beads show up as a Graphite stack. After committing and dropping
rejected hunks, a stacked bead whose parent branch was rewritten since it was stacked (the parent
was amended in review or rebased before a revision) is first restacked onto the parent's current
tip, with either tool. sulla then runs `gt track <branch> --parent <parent> --no-interactive` in the bead's
worktree, where the parent is the stack parent's branch or base as above. Re-tracking only updates
Graphite's metadata and never rebases. It then runs
`gt submit --no-interactive --no-edit --no-ai --no-stack` (plus `--draft` when `draft = true`),
which pushes the branch and its downstack and opens or updates their PRs, never touching branches
stacked above it. The PR is looked up with `gh pr list --head <branch>`, and when it was just
created its title and body are set with `gh pr edit` the same way as with `gh`. The rest (labels, Awaiting PR,
watching, revising and merge detection) is the same as with `gh`. A stacked bead's parent must
already be tracked by Graphite, which it is when the parent was also published with Graphite.
Only sulla runs `gt`; agents never push or submit.

A Graphite publish that fails because of a bad rebase is repaired automatically:

- When `gt track` rejects the parent (`<parent> is not in the history of <branch>`) or `gt track`
  or `gt submit` asks for a restack, sulla rebases the branch itself (onto the stack parent's
  current tip when stacked, else base), then runs `gt track` and `gt submit` once more.
- When that rebase, or the restack onto a rewritten parent before it, hits conflicts, the rebase is
  aborted and the bead goes back to its agent (resumed if it stopped) with a prompt naming the
  exact `git rebase` to run and resolve. The status line says
  `publishing <bead> hit conflicts with <onto>, its agent is resolving them and sulla will publish again`.
  Once the agent moves the bead back to Review, sulla publishes it again, together with the
  beads that were stacked above it in the original publish. That retry is not repaired again: a
  second conflict is reported in the status line and the beads stay in Review.

sulla rebases stacks itself, so with `tool = "graphite"` it keeps Graphite's metadata in step:

- Whenever a branch is restacked (a parent merged locally, its PR merged, or a closed parent was
  swept) or unstacked (its parent closed without merging, or was discarded or retried), sulla
  re-tracks it with `gt track` onto its new parent: the stack parent's branch while that parent is
  open and Awaiting PR, else base. Branches still waiting to restack are re-tracked too. `gt` never
  rebases anything.
- A restacked branch that is Awaiting PR, whose rebase succeeded and whose worktree is clean, is
  then re-submitted with `gt submit` from its worktree so its PR base and commits update.
- When a bead is closed or discarded and its branch removed, sulla first runs
  `gt untrack <branch> --force`, ignoring errors when it was not tracked. Graphite untracks its
  children too, and they are re-tracked as above once restacked.
- When a revised Awaiting PR bead is published again, its Awaiting PR children are restacked onto
  the new parent tip, then re-tracked and re-submitted.

Failures show in the status line as `could not update Graphite for <bead>` and successful
re-submits as `resubmitted <bead> with Graphite`. With `tool = "gh"` none of this runs.

### Watching PRs

With `workflow = "pr"`, a background thread polls every Awaiting PR bead's PR every
`[pr] poll_secs` (default 120) with `gh pr view <n> --json state,comments,reviews,statusCheckRollup`
plus `gh api repos/{owner}/{repo}/pulls/<n>/comments` for inline review comments.

- **Merged**: the bead is closed with reason `merged via PR #<n>`, its agent and reviewer stop,
  and its worktree and branch are removed (the diff stays viewable in Done). Beads stacked on it
  are restacked onto `<remote>/<base>` after a fetch (local base if the fetch fails).
- **Closed without merging**: a `pr` notification fires and a status message is shown. The bead
  stays in Awaiting PR.
- **New comments, reviews or failing checks**: new items are appended to the bead's notes (which
  the agent prompt includes) as `PR #<n> review activity:` and `PR #<n> checks failing: <names>`,
  and a `pr` notification fires. Reviews with an empty body count only when they approve or
  request changes.

Seen comment and review ids, the last checks result, the closed flag and unaddressed feedback are
kept per bead in `.sulla/pr-watch.json`, so restarts do not refire. Cards show `✉<n>` for the
number of PR comments seen, `↻<n>` for unaddressed feedback, `⟳ci`, `✓ci` or `✗ci` for the checks
result and `closed` when the PR was closed. Publishing triggers an immediate poll.

### Revising from PR feedback

New comments and reviews (approvals excluded) and newly failing checks on an open PR are queued
as unaddressed feedback. With `[pr] auto_revise = true` (the default) the bead moves to
`sulla:revise`, otherwise only the notification fires and `U` on the board
queues it. Revise beads are scheduled like pending work, so they wait for a free slot under
`max_agents`, ordered by priority, and evict parked Review agents (including `revised` beads
waiting for `P`) like pending work does. Through the whole revision the card stays in Awaiting PR
and shows `revise queued`, `revising` (`Enter` opens the agent), `ai reviewing` when an AI review
runs on it, and `revised, Enter to review` once it is waiting on you: `Enter` opens the review and
`P` there publishes it (`P` on the board pauses scheduling instead).

When a Revise bead is picked, sulla:

1. Fetches `<remote>/<base>` and rebases `<branch>` onto it in the worktree (a bead stacked on
   a parent that is still open is restacked onto the parent instead). On conflicts the rebase is
   aborted and the prompt tells the agent to run it and resolve the conflicts first.
2. Records the rebased HEAD in `refs/sulla/revise/<bead>`. Until the next publish, the Review diff,
   log, rejected hunks and the card's line counts cover only the changes made since then. While
   the revision has no commits or uncommitted changes yet they show the whole branch instead.
3. Resumes the bead's agent session with a prompt listing each comment (author, `file:line`,
   body) and each failing check with the last lines of `gh run view --log-failed` when available.
   The agent is told to address every comment, commit, run the build and tests, run
   `bd set-state <bead> sulla=review --reason "addressed PR feedback"` and stop, and not to push or
   comment on the PR.

The queued comment ids are then marked addressed so they never retrigger. When the bead shows
`revised, Enter to review`, read the incremental diff and publish again with `P` in the review (if
its agent was evicted meanwhile, publishing works the same): the branch is force pushed with
`--force-with-lease` to the existing PR and the bead returns to Awaiting PR. With `reply = true`
sulla comments on the PR with the pushed commit and what it addressed. That comment carries a
hidden `<!-- sulla -->` marker so the watcher ignores it. Graphite's stack comment, which `gt submit`
posts under your account, is ignored the same way by its `<!-- Current dependencies on/for this PR: -->`
marker.

Apart from that opt-in reply, sulla only reads from GitHub. Agent and AI reviewer prompts tell
agents never to post comments, reviews or replies to GitHub or change PRs through `gh`. PR
feedback reaches them only through the bead notes and the revise prompt.

### Session backups

Awaiting PR beads can wait days or weeks, and agent session files live outside the repo
(`~/.copilot/session-state/<session>/` or `~/.claude/projects/*/<session>.jsonl`, honoring
`COPILOT_HOME` and `CLAUDE_CONFIG_DIR`), where they may be pruned. With `workflow = "pr"` and
`[pr] backup_sessions = true`, every publish copies the implementer's session files to
`.sulla/sessions/<bead>/` (git-ignored with the rest of `.sulla/`), replacing the previous backup.
The backup is removed when the bead closes.

Whenever a bead's agent is resumed with `workflow = "pr"` (revising, sending feedback, `A`, or a
restart):

- If the live session files exist, the session resumes as usual.
- If they are missing but a backup exists, the backup is copied back to its original location and
  the session resumes.
- If neither exists, sulla starts a fresh session with the original bead prompt, the PR URL, the
  `git log base..HEAD` summary of the branch and the feedback being sent, and the status bar says
  the session was rebuilt.

With `sandbox = "incus"`, agents run with the host `HOME` and the default `[incus] mounts` share
`~/.copilot` and `~/.claude` with the host, so sessions are backed up and restored on the host. If
`[incus] env` points `COPILOT_HOME` or `CLAUDE_CONFIG_DIR` elsewhere, that path is used instead.
When the session directory is not under any mount, sulla cannot see it from the host and skips
backup, restore and rebuild for that agent. With `workflow = "local"` none of this runs.

### Rejecting hunks

`n` rejects the hunk under the cursor, `y` accepts it, and the cursor moves on like `git add -p`.
On a file header, or with `N`/`Y`, the whole file is toggled. Rejected hunks are dimmed and files
are marked `±` (partly) or `✗` (fully). On merge, pending changes are committed, rejected hunks are
reverse-applied (binary files restored from the fork point) and committed as
`Drop changes rejected in review of <bead>`, then the merge runs. If a rejected hunk no longer
applies, the merge stops and `R` reloads the diff, keeping unchanged rejections. Checks do not
rerun on the trimmed result.

### Checks

With `check` set, sulla runs it via `sh -c` in the worktree whenever a Review bead's branch head or
`git status --porcelain` changes, killing stale runs. One check runs at a time, the rest queue.
With `sandbox = "incus"` it runs in the bead's container; without one (e.g. after restart or once
its agent was evicted) nothing starts automatically and `T` runs it on the host.

Output goes to `.sulla/checks/<bead>.log`, the result to `<bead>.json` (reused on restart if head
and status match). Cards show `…chk`, `⟳chk`, `✓chk` or `✗chk`. `m` refuses to merge unless
checks pass on the current state and the worktree is clean. `M` forces it. `t` shows output, `T`
reruns.

### Conflicts

Each poll runs `git merge-tree` on every open bead's committed branch against base and every other
open branch. Conflicting beads show `⚠` on their card, and the zoomed card, Review header and file
list say with what. Uncommitted changes are not checked.

## Beads database

Beads keeps its data in Dolt, either embedded in each `bd` process (bd's default) or in a Dolt SQL
server (`bd init --server`). sulla works with both, and `.beads/metadata.json` (`dolt_mode`) tells
it which one a repo uses.

- **Embedded.** Every read and write is a `bd` subprocess. Each one takes 0.25 to 0.35s and the
  database file lock, so sulla's polling and the agents' `bd` calls queue behind each other.
- **Server.** sulla starts the Dolt server with `bd dolt start` if it is not running, then starts
  `bd serve` on a fixed per-repo port (20000 to 29999, derived from the repo path) and reads
  through its loopback HTTP API instead of spawning `bd`. If a `bd serve` for the same `.beads`
  already listens on that port, sulla reuses it. No config is needed. `bd serve` output goes to
  `.sulla/bd-serve.log`.

Over HTTP sulla reads the board (`bd list`, `bd ready` and the blocked beads), bead details for
`i` and `H`, comments and events, and writes claims, releases, label and note updates, priority,
close, delete and new beads. A board refresh makes no `bd` subprocess call.

- Blocked beads come from `GET /v0/beads/blocked` when the build advertises `blocked.list`.
  bd 1.3.1 does not, so sulla asks `GET /v0/beads/dependencies/blocking` about every open bead
  instead and keeps the ones with an open blocker. That covers `blocks` edges, which is what the
  board shows. Unlike `bd blocked` it does not mark a child blocked because its parent is, and it
  does not count `waits-for` or `conditional-blocks` edges. Scheduling is unaffected: what
  starts comes from `bd ready`, which is served over HTTP with bd's own rules.
- New beads are created over HTTP as `task`s with the git `user.email` as owner, like `bd create`.
  The API does not let a client set `created_by`, so beads sulla creates in server mode have none.
- `bd set-state` has no HTTP equivalent and stays a CLI call, as does anything a `bd serve` build
  does not list in `GET /v0/beads/context`. In server mode the CLI talks to the same server, so
  those calls take about 0.07s.

HTTP writes do not run bd's mutation hooks. When `.beads/hooks/on_create`, `on_update` or
`on_close` exists and is executable, sulla sends its writes through the CLI so the hooks keep
firing. Agents always use the CLI.

If `bd serve` cannot start, refuses connections, or answers 503 (its database is unavailable),
sulla runs that call through the CLI instead. A `bd serve` that sulla started is then restarted,
at most every 30 seconds. One known cause of a lasting 503: `bd serve` reaches Dolt through a
`bd db-proxy-child` (pid in `.beads/dolt/proxy.pid`) that outlives it and stays pinned to the port
Dolt had when it started, so after `bd dolt stop` and `bd dolt start` move Dolt to a new port
every request fails, and a new `bd serve` refuses to start while that proxy runs. When a start
fails that way, sulla stops the proxy (only after checking it is a `db-proxy-child` for this
repo's `.beads/dolt`) and starts `bd serve` again. A `bd serve` sulla did not start is left alone,
and the health panel names the proxy to stop.

On exit sulla stops the `bd serve` it started (one it reused keeps running) and leaves the Dolt
server running, since agents in other terminals and other tools may still use it. Stop it with
`bd dolt stop`.

sulla sets `DOLT_DISABLE_EVENT_FLUSH=1` for every `bd` it runs in server mode (including
`bd dolt start` and `bd serve`), so a Dolt server it starts does not send Dolt's anonymous usage
events. A server started elsewhere keeps its own setting.

`sulla init` runs `bd init --server` when the repo has no beads database yet and `dolt` is on
`PATH` (otherwise it offers an embedded one). It never migrates a database. For an embedded repo
it prints these steps, which keep the full Dolt history. Stop sulla and every agent first:

```sh
bd backup init ../<prefix>-beads-backup && bd backup sync
mv .beads .beads.embedded
bd init --server -p <prefix>
bd backup restore --force ../<prefix>-beads-backup
bd list --all
```

Then carry over settings from `.beads.embedded/config.yaml` and delete `.beads.embedded` once
everything is there.

## Board

- The board refreshes every `poll_secs` (default 2). Git state (diff stats, conflicts, stacks) is
  reread every time, but with embedded Dolt the `bd list`, `bd ready` and `bd blocked` calls only
  run when the database manifest under `.beads/embeddeddolt` has changed since the last read,
  after sulla's own writes, and at least once a minute. Each `bd` call takes the embedded
  database lock, so polling it constantly slowed down every other `bd` command. A Dolt SQL server
  is read every time, over `bd serve` (see [Beads database](#beads-database)).
- Cards show time in state, the worktree diff stat (`+adds -dels Nf`) and the last line of agent
  output.
- The card meta line shows dependencies by id: `stacked on <parent>` for a stacked bead (that
  parent is not repeated under `blocked by`), `blocked by <ids>` for other unmet blockers,
  `blocks <ids>` for open beads waiting on it, and `stacked: <ids>` (or `<n> stacked` past two)
  for beads stacked on it.
- The selected card's chain is highlighted in every column: its transitive blockers and stack
  parents get a yellow `▲` and title, and beads transitively blocked by or stacked on it get a
  magenta `▼` and title. Dependency cycles are walked once.
- `L` toggles a chain filter that shows only the selected bead, its ancestors and its descendants
  across columns. The status bar shows `CHAIN <id>` and column counts read `shown/total` while it
  is on. `L` again (or `Esc`) restores the normal board. It combines with the `/` filter.
- `!` (or `:health`) opens a health panel: probes `bd`, the agent CLI, `kache` and `incus`
  (including `incus query /1.0`), plus `gh` (including `gh auth status`) when `workflow = "pr"`.
  In a server-mode repo it also checks the `dolt` binary, whether the Dolt server answers on its
  port, and whether `bd serve` is ready: it asks `GET /v0/beads/ready?limit=1`, since `/healthz`
  stays green while the database is unreachable. It says whether sulla started or reused it, or
  why sulla fell back to the CLI. It shows the last bd poll error, and explains why nothing is
  starting (paused, rate limited, slots full, failed starts, all blocked, backlog only, or nothing
  open). When the slots are full and some are held by beads without a working agent (parked Review
  beads, including revised PR beads, or beads with checks or a publish in flight), it names the
  first three of them, for example
  `all 3 slots in use (1 working, parked: a-1, a-2; :max <n> to raise)`. `r` rechecks.
