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
- **Running**: the agent works in `.sulla/worktrees/<bead>` on branch `sulla/<bead>` and is told
  to run `bd set-state <bead> sulla=review` when done.
- **Review**: you read the diff, comment, run checks, request an AI review, and merge.
- **Merge** rebases the branch onto base and fast-forwards (no merge commits), removes the
  worktree and branch, and closes the bead.
- **Publish** (`P` in review, `workflow = "pr"` only) pushes the branch and opens a GitHub PR
  instead of merging. The bead stays open in **Awaiting PR** (`sulla:pr`, plus `pr:<number>`) with
  its worktree and agent session intact, while the agent is stopped. Awaiting PR beads never count
  toward `max_agents`, are never scheduled and are not resumed on restart.
- **Revise** (`sulla:revise`): an Awaiting PR bead with new PR feedback waits in Pending and is
  picked like any other pending bead (see [Revising from PR feedback](#revising-from-pr-feedback)).
- **Discard** (`D`) deletes the worktree and branch and holds the bead so it can rerun from
  scratch. **Retry** (`F`) does the same but asks why the attempt failed, appends that to the
  bead's notes (which the agent prompt includes) and returns it to Pending.
- **Delete** (`dd` on the board) does what Discard does, then permanently deletes the bead with
  `bd delete --force`. Dependents are orphaned rather than deleted.
- **Kill** (`X`) holds the bead in Pending until released with `s`.
- On restart, agents still running or in review are resumed via their agent session, so review
  beads keep their slots ahead of pending work.

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
published while its blocker is Awaiting PR (the PR targets the blocker's branch), but it cannot
merge locally until the blocker is closed.

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
- `c` opens a conversation agent in the main checkout, not tied to a bead and not counted toward
  `max_agents`. It is primed to plan work and create beads (staged in Backlog, wired with
  `bd dep add`, released on your go). `X` ends it, `C` starts fresh.
- `e` suspends sulla and opens `$VISUAL`, `$EDITOR` or `nvim` in the worktree. Agents keep
  running, so edits may conflict.

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
committed and rejected hunks dropped as on merge, then `sulla/<bead>` is pushed to the `[pr]`
remote with `--set-upstream` and `gh pr create --head sulla/<bead> --base <base>` opens a PR (a
draft when `draft = true`) titled after the bead with its description as the body. If an open PR
already exists for the branch it is reused. A bead stacked on a parent that is itself Awaiting PR
targets the parent's branch instead of base. The bead gets `pr:<number>`, moves to Awaiting PR and
its agent and reviewer stop, leaving the worktree and session for later rounds. Publishing again
force pushes with `--force-with-lease`. Cards show the PR number as `#<number>`. With
`workflow = "local"`, `P` only shows a status message.

With `[pr] tool = "graphite"`, publishing uses the Graphite `gt` CLI instead of `git push` and
`gh pr create`, so stacked beads show up as a Graphite stack. After committing and dropping
rejected hunks, sulla runs `gt track sulla/<bead> --parent <parent> --no-interactive` in the bead's
worktree, where the parent is the stack parent's branch or base as above. Re-tracking only updates
Graphite's metadata and never rebases. It then runs
`gt submit --no-interactive --no-edit --no-ai --no-stack` (plus `--draft` when `draft = true`),
which pushes the branch and its downstack and opens or updates their PRs, never touching branches
stacked above it. The PR is looked up with `gh pr list --head sulla/<bead>`, and when it was just
created its title and body are set from the bead with `gh pr edit`. The rest (labels, Awaiting PR,
watching, revising and merge detection) is the same as with `gh`. A stacked bead's parent must
already be tracked by Graphite, which it is when the parent was also published with Graphite.
Only sulla runs `gt`; agents never push or submit.

sulla rebases stacks itself, so with `tool = "graphite"` it keeps Graphite's metadata in step:

- Whenever a branch is restacked (a parent merged locally, its PR merged, or a closed parent was
  swept) or unstacked (its parent closed without merging, or was discarded or retried), sulla
  re-tracks it with `gt track` onto its new parent: the stack parent's branch while that parent is
  open and Awaiting PR, else base. Branches still waiting to restack are re-tracked too. `gt` never
  rebases anything.
- A restacked branch that is Awaiting PR, whose rebase succeeded and whose worktree is clean, is
  then re-submitted with `gt submit` from its worktree so its PR base and commits update.
- When a bead is closed or discarded and its branch removed, sulla first runs
  `gt untrack sulla/<bead> --force`, ignoring errors when it was not tracked. Graphite untracks its
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
`sulla:revise` and shows in Pending, otherwise only the notification fires and `U` on the board
queues it. Revise beads are scheduled like pending work, so they wait for a free slot under
`max_agents`, ordered by priority.

When a Revise bead is picked, sulla:

1. Fetches `<remote>/<base>` and rebases `sulla/<bead>` onto it in the worktree (a bead stacked on
   a parent that is still open is restacked onto the parent instead). On conflicts the rebase is
   aborted and the prompt tells the agent to run it and resolve the conflicts first.
2. Records the rebased HEAD in `refs/sulla/revise/<bead>`. Until the next publish, the Review diff,
   log and rejected hunks cover only the commits made since then.
3. Resumes the bead's agent session with a prompt listing each comment (author, `file:line`,
   body) and each failing check with the last lines of `gh run view --log-failed` when available.
   The agent is told to address every comment, commit, run the build and tests, run
   `bd set-state <bead> sulla=review --reason "addressed PR feedback"` and stop, and not to push or
   comment on the PR.

The queued comment ids are then marked addressed so they never retrigger. When the bead is back in
Review, read the incremental diff and publish again with `P`: the branch is force pushed with
`--force-with-lease` to the existing PR and the bead returns to Awaiting PR. With `reply = true`
sulla comments on the PR with the pushed commit and what it addressed. That comment carries a
hidden `<!-- sulla -->` marker so the watcher ignores it.

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
With `sandbox = "incus"` it runs in the bead's container; without one (e.g. after restart) nothing
starts automatically and `T` runs it on the host.

Output goes to `.sulla/checks/<bead>.log`, the result to `<bead>.json` (reused on restart if head
and status match). Cards show `…chk`, `⟳chk`, `✓chk` or `✗chk`. `m` refuses to merge unless
checks pass on the current state and the worktree is clean. `M` forces it. `t` shows output, `T`
reruns.

### Conflicts

Each poll runs `git merge-tree` on every open bead's committed branch against base and every other
open branch. Conflicting beads show `⚠` on their card, and the zoomed card, Review header and file
list say with what. Uncommitted changes are not checked.

## Board

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
  (including `incus query /1.0`), plus `gh` (including `gh auth status`) when `workflow = "pr"`,
  shows the last bd poll error, and explains why nothing is
  starting (paused, rate limited, slots full, failed starts, all blocked, backlog only, or nothing
  open). `r` rechecks.
