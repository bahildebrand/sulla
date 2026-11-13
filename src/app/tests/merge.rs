use super::*;

#[test]
fn merge_closes_the_bead_when_only_cleanup_fails() {
    let mut h = Harness::new("merge-cleanup", &[("a", Some(State::Review))]);
    let closes = h.app.beads.record_closes();
    let wt = h.app.git.ensure_worktree("a", "main").unwrap();
    std::fs::write(wt.join("a2.txt"), "x\n").unwrap();
    h.app.git.commit_pending("a", "a: work").unwrap();
    git_in(&h.dir, &["worktree", "lock", &wt.to_string_lossy()]);
    h.app.land_merge("a");
    h.pump();
    assert_eq!(*closes.lock().unwrap(), vec!["a".to_string()]);
    let path = wt.display().to_string();
    assert!(
        h.app
            .status
            .starts_with("merged a into main; cleanup failed: "),
        "{}",
        h.app.status
    );
    assert!(
        h.app.status.contains(&format!(
            "Remove {path} by hand, then run `git worktree prune` and `git branch -D sulla/a`"
        )),
        "{}",
        h.app.status
    );
    assert!(h.dir.join("a2.txt").exists());
    assert!(wt.join(".git").exists());
    assert!(h.app.git.was_merged("a", "main"));
    assert!(matches!(h.app.view, View::Board));
    git_in(&h.dir, &["worktree", "unlock", &path]);
}

#[test]
fn merge_runs_in_the_background_and_refuses_a_second_merge() {
    let mut h = Harness::new("merge-background", &[("a", Some(State::Review))]);
    let closes = h.app.beads.record_closes();
    h.app.beads.assume_unblocked();
    let wt = h.app.git.ensure_worktree("a", "main").unwrap();
    std::fs::write(wt.join("a2.txt"), "x\n").unwrap();
    h.app.git.commit_pending("a", "a: work").unwrap();
    h.app.view = View::Progress("a".into());
    h.app.merge("a");
    assert_eq!(h.app.status, "merging a...");
    assert!(h.app.merging.contains("a"));
    h.app.request_merge("a".into(), false);
    assert!(matches!(h.app.mode, Mode::Normal));
    assert_eq!(h.app.status, "a is already merging, wait for it to finish");
    h.app.confirm_discard("a".into());
    assert!(matches!(h.app.mode, Mode::Normal));
    h.pump();
    assert!(h.app.merging.is_empty());
    assert_eq!(h.app.status, "merged a into main");
    assert_eq!(*closes.lock().unwrap(), vec!["a".to_string()]);
    assert!(h.dir.join("a2.txt").exists());
    assert!(!wt.exists());
    assert!(matches!(h.app.view, View::Board));
}

#[test]
fn merge_conflicts_offer_to_resolve_once_the_background_land_fails() {
    let mut h = Harness::new("merge-conflict", &[("a", Some(State::Review))]);
    let closes = h.app.beads.record_closes();
    h.app.beads.assume_unblocked();
    let wt = h.app.git.ensure_worktree("a", "main").unwrap();
    std::fs::write(wt.join("a.txt"), "theirs\n").unwrap();
    h.app.git.commit_pending("a", "a: work").unwrap();
    std::fs::write(h.dir.join("a.txt"), "ours\n").unwrap();
    git_in(&h.dir, &["commit", "-qam", "base moves"]);
    h.app.merge("a");
    h.pump();
    assert!(h.app.merging.is_empty());
    assert_eq!(h.app.status, "merge conflicts in a");
    assert!(matches!(
        h.app.mode,
        Mode::Confirm {
            action: Action::ResolveConflicts(ref id),
            ..
        } if id == "a"
    ));
    assert!(closes.lock().unwrap().is_empty());
    assert!(wt.join(".git").exists());
}

#[test]
fn merging_a_parent_restacks_its_child_in_the_background() {
    let mut h = Harness::new(
        "merge-restack",
        &[("p", Some(State::Review)), ("c", Some(State::Review))],
    );
    h.app.beads.record_closes();
    h.app.beads.assume_unblocked();
    let wt = h.app.git.ensure_worktree("p", "main").unwrap();
    std::fs::write(wt.join("p.txt"), "x\n").unwrap();
    h.app.git.commit_pending("p", "p: work").unwrap();
    let cwt = h.app.git.ensure_stacked_worktree("c", "p").unwrap();
    std::fs::write(cwt.join("c.txt"), "x\n").unwrap();
    h.app.git.commit_pending("c", "c: work").unwrap();
    h.app.merge("p");
    h.pump();
    assert_eq!(h.app.status, "merged p into main, restacked c");
    assert_eq!(h.app.git.stack_parent("c"), None);
}

#[test]
fn stack_sweep_restacks_children_of_a_closed_parent_off_the_ui_thread() {
    let mut h = Harness::new(
        "sweep-background",
        &[("p", Some(State::Review)), ("c", Some(State::Review))],
    );
    let wt = h.app.git.ensure_worktree("p", "main").unwrap();
    std::fs::write(wt.join("p.txt"), "x\n").unwrap();
    h.app.git.commit_pending("p", "p: work").unwrap();
    let cwt = h.app.git.ensure_stacked_worktree("c", "p").unwrap();
    std::fs::write(cwt.join("c.txt"), "x\n").unwrap();
    h.app.git.commit_pending("c", "c: work").unwrap();
    git_in(&h.dir, &["merge", "-q", "--ff-only", "sulla/p"]);
    h.app.edit_local("p", |b| b.status = "closed".into());
    h.app.sweep_stacks();
    assert!(h.app.sweeping);
    assert_eq!(h.app.git_pending, 1);
    h.app.sweep_stacks();
    assert_eq!(h.app.git_pending, 1);
    h.pump();
    assert!(!h.app.sweeping);
    assert_eq!(h.app.status, "restacked c");
    assert_eq!(h.app.git.stack_parent("c"), None);
}
