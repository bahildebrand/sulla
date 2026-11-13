use super::*;

const WARNING: &str = "closed outside sulla";

fn close(h: &mut Harness, id: &str) {
    h.app.edit_local(id, |b| b.status = "closed".into());
}

fn commit_work(h: &Harness, id: &str) {
    let wt = h.app.git.worktree_path(id);
    std::fs::write(wt.join(format!("{id}.txt")), "x\n").unwrap();
    h.app
        .git
        .commit_pending(id, &format!("{id}: work"))
        .unwrap();
}

#[test]
fn closing_a_running_bead_outside_sulla_frees_its_slot_and_keeps_unmerged_work() {
    let running = Some(State::Running);
    let mut h = Harness::new("closed-outside", &[("a", running), ("b", running)]);
    park(&mut h, &["a", "b"]);
    assert_eq!(h.app.running_count(), 2);
    assert_eq!(h.app.free_slots(), 0);
    commit_work(&h, "a");
    h.app.view = View::Progress("a".into());
    close(&mut h, "a");
    h.app.reconcile();
    assert!(!h.app.agents.contains_key("a"));
    assert!(h.app.agents.contains_key("b"));
    assert_eq!(h.app.slots.get("a"), None);
    assert_eq!(h.app.running_count(), 1);
    assert_eq!(h.app.free_slots(), 1);
    assert_eq!(h.finished(), [(0, "a".to_string(), Outcome::Killed)]);
    assert!(matches!(h.app.view, View::Board));
    assert!(
        h.app
            .status
            .contains("a closed outside sulla, agent stopped; unmerged work kept"),
        "{}",
        h.app.status
    );
    assert!(h.app.git.has_branch("a"));
    assert!(h.app.git.worktree_path("a").join(".git").exists());
    assert!(h.app.git.unmerged_branch("a", "main"));
}

#[test]
fn closing_a_bead_without_unmerged_work_frees_its_slot_quietly() {
    let mut h = Harness::new("closed-empty", &[("a", Some(State::Running))]);
    park(&mut h, &["a"]);
    h.app.status.clear();
    close(&mut h, "a");
    h.app.reconcile();
    assert!(!h.app.agents.contains_key("a"));
    assert_eq!(h.app.running_count(), 0);
    assert!(!h.app.status.contains(WARNING), "{}", h.app.status);
}

#[test]
fn a_bead_merged_by_sulla_does_not_warn_about_unmerged_work() {
    let mut h = Harness::new("closed-merged", &[("a", Some(State::Review))]);
    park(&mut h, &["a"]);
    commit_work(&h, "a");
    h.app.git.land("a", "main", "a: a").unwrap();
    h.app.git.remove("a").unwrap();
    h.app.status.clear();
    close(&mut h, "a");
    h.app.reconcile();
    assert!(!h.app.agents.contains_key("a"));
    assert!(!h.app.status.contains(WARNING), "{}", h.app.status);
}
