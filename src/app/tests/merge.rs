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
