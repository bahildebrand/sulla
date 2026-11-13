use super::*;

#[test]
fn release_frees_the_worktree_and_keeps_the_branch_and_refs() {
    let mut h = Harness::new(
        "release",
        &[("p", Some(State::Pr)), ("a", Some(State::Review))],
    );
    h.app.all[0].labels.push("pr:3".into());
    worked(&h, "p", "main");
    h.app.git.ensure_stacked_worktree("a", "p").unwrap();
    let wt = worked(&h, "a", "sulla/p");
    h.app.git.mark_revision("a").unwrap();
    let head = h.app.git.head("a").unwrap();
    let stack = git_in(&h.dir, &["rev-parse", "refs/sulla/stack/a"]);
    h.app.view = View::Progress("a".into());
    h.app.request_release("a".into());
    let Mode::Confirm {
        msg,
        action: Action::Release(id),
    } = std::mem::replace(&mut h.app.mode, Mode::Normal)
    else {
        panic!("no confirm, status: {}", h.app.status);
    };
    assert!(msg.contains("check out sulla/a"), "{msg}");
    h.app.confirm(Action::Release(id));
    assert_eq!(h.app.status, "released a, check out sulla/a to work on it");
    assert_eq!(h.state("a"), Some(State::Released));
    assert!(matches!(h.app.view, View::Board));
    assert!(!wt.exists());
    assert!(h.app.git.has_branch("a"));
    assert_eq!(
        git_in(&h.dir, &["rev-parse", "refs/sulla/released/a"]),
        head
    );
    assert_eq!(git_in(&h.dir, &["rev-parse", "refs/sulla/stack/a"]), stack);
    assert_eq!(h.app.git.stack_parent("a").as_deref(), Some("p"));
    assert!(!git_in(&h.dir, &["rev-parse", "refs/sulla/revise/a"]).is_empty());
    assert_eq!(h.bead("a").column(Utc::now(), 3), Some(Column::Review));
    h.app.release_worktree("a");
    assert_eq!(
        h.app.status,
        "a is released, check out sulla/a to work on it"
    );
    let refusals: [fn(&mut App); 6] = [
        |app: &mut App| app.start_review("a"),
        |app: &mut App| app.request_publish("a".into()),
        |app: &mut App| app.request_merge("a".into(), true),
        |app: &mut App| app.relaunch("a"),
        |app: &mut App| app.mark_review("a"),
        |app: &mut App| app.request_shell("a"),
    ];
    for refuse in refusals {
        h.app.status.clear();
        refuse(&mut h.app);
        assert_eq!(
            h.app.status,
            "a is released, check out sulla/a to work on it"
        );
    }
    assert!(!h.app.deliver("a", "more"));
    assert!(!h.app.is_starting("a"));
    assert_eq!(h.state("a"), Some(State::Released));
    git_in(&h.dir, &["checkout", "-q", "sulla/a"]);
    assert_eq!(git_in(&h.dir, &["rev-parse", "HEAD"]), head);
}

#[test]
fn release_refuses_a_dirty_worktree_or_a_bead_not_in_review() {
    let mut h = Harness::new(
        "release-dirty",
        &[("a", Some(State::Review)), ("b", Some(State::Running))],
    );
    let wt = worked(&h, "a", "main");
    std::fs::write(wt.join("wip.txt"), "wip\n").unwrap();
    h.app.request_release("a".into());
    assert!(matches!(h.app.mode, Mode::Normal));
    assert_eq!(
        h.app.status,
        "a has uncommitted changes in its worktree, commit or discard them before releasing"
    );
    h.app.release_worktree("a");
    assert!(wt.join("wip.txt").exists());
    assert_eq!(h.state("a"), Some(State::Review));
    assert!(git_in(&h.dir, &["for-each-ref", "refs/sulla/released"]).is_empty());
    h.app.request_release("b".into());
    assert!(matches!(h.app.mode, Mode::Normal));
    assert_eq!(
        h.app.status,
        "b is not in Review or Awaiting PR, only those can be released"
    );
}

#[test]
fn released_child_is_not_rebased_when_its_parent_merges() {
    let mut h = Harness::new(
        "release-child",
        &[("p", Some(State::Pr)), ("c", Some(State::Released))],
    );
    h.app.cfg.workflow = Workflow::Pr;
    h.app.all[0].labels.push("pr:7".into());
    worked(&h, "p", "main");
    h.app.git.ensure_stacked_worktree("c", "p").unwrap();
    worked(&h, "c", "sulla/p");
    h.app.git.release("c").unwrap();
    let head = h.app.git.head("c").unwrap();
    std::fs::write(h.dir.join("b.txt"), "b\n").unwrap();
    git_in(&h.dir, &["add", "b.txt"]);
    git_in(&h.dir, &["commit", "-qm", "base moves"]);
    h.app
        .on_pr("p", 7, Ok(pr_status(crate::pr::PrState::Merged, &[])));
    assert!(h.app.status.starts_with("PR #7 merged"), "{}", h.app.status);
    assert!(!h.app.status.contains("restack"), "{}", h.app.status);
    assert!(!h.app.git.has_branch("p"));
    assert_eq!(h.app.git.head("c").unwrap(), head);
    assert_eq!(h.app.git.stack_parent("c").as_deref(), Some("p"));
    h.app.edit_local("p", |b| b.status = "closed".into());
    h.app.sweep_stacks();
    assert_eq!(h.app.git.head("c").unwrap(), head);
    assert_eq!(h.app.git.stack_parent("c").as_deref(), Some("p"));
}

#[test]
fn released_pr_merge_closes_while_its_branch_is_checked_out_elsewhere() {
    let mut h = Harness::new("release-merge", &[("a", Some(State::Released))]);
    h.app.cfg.workflow = Workflow::Pr;
    h.app.all[0].labels.push("pr:7".into());
    worked(&h, "a", "main");
    h.app.git.mark_revision("a").unwrap();
    h.app.git.release("a").unwrap();
    let other = h.dir.with_extension("mine");
    let _ = std::fs::remove_dir_all(&other);
    git_in(
        &h.dir,
        &["worktree", "add", "-q", &other.to_string_lossy(), "sulla/a"],
    );
    let kept = h.app.git.checked_out("a").unwrap();
    h.app
        .on_pr("a", 7, Ok(pr_status(crate::pr::PrState::Merged, &[])));
    assert!(h.app.status.starts_with("PR #7 merged"), "{}", h.app.status);
    assert!(h.app.notifier.fired.is_empty());
    assert!(
        h.app.status.ends_with(&format!(
            "; sulla/a is checked out in {}, delete it yourself",
            kept.display()
        )),
        "{}",
        h.app.status
    );
    assert!(h.app.git.has_branch("a"));
    assert!(
        git_in(
            &h.dir,
            &["for-each-ref", "refs/sulla/released", "refs/sulla/revise"]
        )
        .is_empty()
    );
    assert!(h.app.git.merged_diff("a").unwrap().contains("a.txt"));
    git_in(
        &h.dir,
        &["worktree", "remove", "--force", &other.to_string_lossy()],
    );
}

#[test]
fn released_pr_feedback_is_noted_without_queueing_a_revision() {
    let mut h = Harness::new("release-feedback", &[("a", Some(State::Released))]);
    h.app.all[0].labels.push("pr:7".into());
    h.app
        .on_pr("a", 7, Ok(pr_status(crate::pr::PrState::Open, &["c1"])));
    assert_eq!(h.app.status, "PR #7 for a: 1 new comment");
    assert_eq!(h.app.pr_watch.beads["a"].pending.count(), 0);
    assert_eq!(h.app.pr_watch.beads["a"].comments.len(), 1);
    let a = h.bead("a");
    h.app.request_revise(&a);
    assert_eq!(
        h.app.status,
        "a is released, check out sulla/a to work on it"
    );
    assert_eq!(h.state("a"), Some(State::Released));
}

#[test]
fn released_bead_without_a_pr_label_adopts_the_pr_it_finds() {
    let mut h = Harness::new(
        "release-found",
        &[("a", Some(State::Released)), ("b", Some(State::Review))],
    );
    assert_eq!(scheduler::unlinked_releases(&h.app.all), ["a"]);
    assert!(scheduler::watched_prs(&h.app.all).is_empty());
    h.app.pr_found("b", 4);
    assert_eq!(h.bead("b").label("pr"), None);
    h.app.pr_found("a", 5);
    assert_eq!(h.bead("a").label("pr"), Some("5"));
    assert_eq!(h.app.status, "found PR #5 for released a, watching it");
    assert_eq!(h.bead("a").column(Utc::now(), 3), Some(Column::AwaitingPr));
    assert!(scheduler::unlinked_releases(&h.app.all).is_empty());
    assert_eq!(scheduler::watched_prs(&h.app.all), [("a".to_string(), 5)]);
    h.app.pr_found("a", 6);
    assert_eq!(
        h.bead("a")
            .labels
            .iter()
            .filter(|l| l.starts_with("pr:"))
            .count(),
        1
    );
}

fn released_by_hand(h: &mut Harness, id: &str) {
    h.app.git.release(id).unwrap();
    h.app
        .edit_local(id, |b| b.set_state_label(Some(State::Released)));
}

fn commit_elsewhere(h: &Harness, id: &str, file: &str) -> std::path::PathBuf {
    let other = h.dir.with_extension(format!("{id}-mine"));
    let _ = std::fs::remove_dir_all(&other);
    git_in(
        &h.dir,
        &[
            "worktree",
            "add",
            "-q",
            &other.to_string_lossy(),
            &h.app.git.branch(id),
        ],
    );
    std::fs::write(other.join(file), "mine\n").unwrap();
    git_in(&other, &["add", file]);
    git_in(&other, &["commit", "-qm", &format!("user: {file}")]);
    other
}

#[test]
fn reclaim_without_changes_returns_to_the_prior_column() {
    let mut h = Harness::new(
        "reclaim-clean",
        &[("p", Some(State::Pr)), ("r", Some(State::Review))],
    );
    h.app.all[0].labels.push("pr:3".into());
    for id in ["p", "r"] {
        worked(&h, id, "main");
        h.app.request_release(id.into());
        let Mode::Confirm { action, .. } = std::mem::replace(&mut h.app.mode, Mode::Normal) else {
            panic!("no confirm, status: {}", h.app.status);
        };
        h.app.confirm(action);
        assert_eq!(h.state(id), Some(State::Released));
    }
    for (id, state) in [("p", State::Pr), ("r", State::Review)] {
        h.app.request_release(id.into());
        assert!(matches!(h.app.mode, Mode::Normal));
        assert_eq!(h.app.status, format!("reclaimed {id}"));
        assert_eq!(h.state(id), Some(state));
        assert!(h.app.git.worktree_path(id).join(".git").exists());
        assert!(h.app.git.dirty(id).unwrap().is_empty());
        assert!(
            h.app
                .git
                .diff(id, "main")
                .unwrap()
                .contains(&format!("{id}.txt"))
        );
        assert!(h.bead(id).notes.is_empty());
    }
    assert!(
        git_in(
            &h.dir,
            &["for-each-ref", "refs/sulla/released", "refs/sulla/revise"]
        )
        .is_empty()
    );
    assert_eq!(h.bead("p").column(Utc::now(), 3), Some(Column::AwaitingPr));
    assert_eq!(h.bead("r").column(Utc::now(), 3), Some(Column::Review));
}

#[test]
fn reclaim_scopes_the_review_to_the_users_commits() {
    let mut h = Harness::new("reclaim-changed", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    worked(&h, "a", "main");
    released_by_hand(&mut h, "a");
    let released = git_in(&h.dir, &["rev-parse", "refs/sulla/released/a"]);
    let other = commit_elsewhere(&h, "a", "mine.txt");
    let kept = h.app.git.checked_out("a").unwrap();
    h.app.open_review("a".into());
    h.app.request_release("a".into());
    assert_eq!(
        h.app.status,
        format!(
            "sulla/a is checked out in {}, switch off it first",
            kept.display()
        )
    );
    assert_eq!(h.state("a"), Some(State::Released));
    assert!(!h.app.git.worktree_path("a").exists());
    assert_eq!(
        git_in(&h.dir, &["rev-parse", "refs/sulla/released/a"]),
        released
    );
    assert!(git_in(&h.dir, &["for-each-ref", "refs/sulla/revise"]).is_empty());

    git_in(
        &h.dir,
        &["worktree", "remove", "--force", &other.to_string_lossy()],
    );
    h.app.request_release("a".into());
    assert_eq!(h.app.status, "reclaimed a, 1 new commit");
    assert_eq!(h.state("a"), Some(State::Pr));
    assert_eq!(
        git_in(&h.dir, &["rev-parse", "refs/sulla/revise/a"]),
        released
    );
    assert!(git_in(&h.dir, &["for-each-ref", "refs/sulla/released"]).is_empty());
    let diff = h.app.git.diff("a", "main").unwrap();
    assert!(diff.contains("mine.txt"), "{diff}");
    assert!(!diff.contains("a.txt"), "{diff}");
    let stat = h.app.git.diff_stat("a", "main").unwrap().unwrap();
    assert_eq!((stat.files, stat.adds, stat.dels), (1, 1, 0));
    let log = h.app.git.log("a", "main").unwrap();
    assert!(log.contains("user: mine.txt"), "{log}");
    assert!(!log.contains("a: work"), "{log}");
    let View::Review(d) = &h.app.view else {
        panic!("review closed");
    };
    assert!(d.files.iter().any(|f| f.name == "mine.txt"));
    assert!(!d.files.iter().any(|f| f.name == "a.txt"));
    let short = git_in(&h.dir, &["rev-parse", "--short", "sulla/a"]);
    assert_eq!(
        h.bead("a").notes,
        format!("Changes made by the user while released:\n{short} user: mine.txt")
    );
}

#[test]
fn reclaim_refuses_a_deleted_branch() {
    let mut h = Harness::new("reclaim-gone", &[("a", Some(State::Review))]);
    worked(&h, "a", "main");
    released_by_hand(&mut h, "a");
    git_in(&h.dir, &["branch", "-D", "sulla/a"]);
    h.app.request_release("a".into());
    assert_eq!(h.app.status, "sulla/a no longer exists, cannot reclaim a");
    assert_eq!(h.state("a"), Some(State::Released));
    assert!(!h.app.git.worktree_path("a").exists());
}

#[test]
fn reclaim_rederives_the_fork_point_after_a_user_restack() {
    for amend in [false, true] {
        let mut h = Harness::new(
            &format!("reclaim-restack-{amend}"),
            &[("p", Some(State::Pr)), ("c", Some(State::Review))],
        );
        let pwt = worked(&h, "p", "main");
        h.app.git.ensure_stacked_worktree("c", "p").unwrap();
        worked(&h, "c", "sulla/p");
        released_by_hand(&mut h, "c");
        let old_fork = git_in(&h.dir, &["rev-parse", "refs/sulla/stack/c"]);
        std::fs::write(pwt.join("p.txt"), "p2\n").unwrap();
        let commit = if amend {
            vec!["commit", "-q", "-a", "--amend", "--no-edit"]
        } else {
            vec!["commit", "-q", "-a", "-m", "p: revised"]
        };
        git_in(&pwt, &commit);
        let tip = h.app.git.head("p").unwrap();
        let other = h.dir.with_extension("c-mine");
        let _ = std::fs::remove_dir_all(&other);
        git_in(
            &h.dir,
            &["worktree", "add", "-q", &other.to_string_lossy(), "sulla/c"],
        );
        git_in(&other, &["rebase", "-q", "--onto", "sulla/p", &old_fork]);
        git_in(
            &h.dir,
            &["worktree", "remove", "--force", &other.to_string_lossy()],
        );
        h.app.request_release("c".into());
        assert_eq!(h.app.status, "reclaimed c", "amend: {amend}");
        assert_eq!(h.state("c"), Some(State::Review));
        assert_eq!(h.app.git.stack_parent("c").as_deref(), Some("p"));
        assert_eq!(git_in(&h.dir, &["rev-parse", "refs/sulla/stack/c"]), tip);
        let diff = h.app.git.diff("c", "main").unwrap();
        assert!(diff.contains("c.txt"), "{diff}");
        assert!(!diff.contains("p.txt"), "{diff}");
        assert!(h.bead("c").notes.is_empty());
    }
}

#[test]
fn reclaim_restacks_onto_base_when_the_parent_merged() {
    let mut h = Harness::new(
        "reclaim-merged-parent",
        &[("p", Some(State::Pr)), ("c", Some(State::Released))],
    );
    h.app.cfg.workflow = Workflow::Pr;
    h.app.all[0].labels.push("pr:7".into());
    worked(&h, "p", "main");
    h.app.git.ensure_stacked_worktree("c", "p").unwrap();
    worked(&h, "c", "sulla/p");
    h.app.git.release("c").unwrap();
    git_in(&h.dir, &["merge", "-q", "--ff-only", "sulla/p"]);
    h.app
        .on_pr("p", 7, Ok(pr_status(crate::pr::PrState::Merged, &[])));
    h.app.edit_local("p", |b| b.status = "closed".into());
    assert!(!h.app.git.has_branch("p"));
    h.app.request_release("c".into());
    assert_eq!(h.app.status, "reclaimed c, restacked c");
    assert_eq!(h.state("c"), Some(State::Review));
    assert_eq!(h.app.git.stack_parent("c"), None);
    let diff = h.app.git.diff("c", "main").unwrap();
    assert!(diff.contains("c.txt"), "{diff}");
    assert!(!diff.contains("p.txt"), "{diff}");
}

#[test]
fn reclaim_unstacks_from_a_parent_closed_without_merging() {
    let mut h = Harness::new(
        "reclaim-closed-parent",
        &[("p", Some(State::Review)), ("c", Some(State::Released))],
    );
    worked(&h, "p", "main");
    h.app.git.ensure_stacked_worktree("c", "p").unwrap();
    worked(&h, "c", "sulla/p");
    h.app.git.release("c").unwrap();
    h.app.edit_local("p", |b| b.status = "closed".into());
    h.app.request_release("c".into());
    assert_eq!(h.app.status, "reclaimed c");
    assert_eq!(h.app.git.stack_parent("c"), None);
    assert!(h.app.git.worktree_path("c").join(".git").exists());
}
