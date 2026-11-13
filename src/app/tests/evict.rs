use super::*;

fn queued_check(h: &mut Harness, id: &str) {
    let key = checks::Key {
        head: String::new(),
        dirty: String::new(),
    };
    let log = h.dir.join(format!("{id}.check.log"));
    h.app
        .checks
        .insert(id.to_string(), Check::queued(key, log, 1, false));
}

#[test]
fn ready_work_evicts_the_longest_parked_agent() {
    let review = Some(State::Review);
    let mut h = Harness::new("evict", &[("r-1", review), ("r-2", review), ("p-1", None)]);
    park(&mut h, &["r-1", "r-2"]);
    h.app.ready = vec![h.bead("p-1")];
    assert_eq!(h.app.free_slots(), 0);
    assert_eq!(h.app.idle(), None);
    h.app.schedule();
    assert!(!h.app.agents.contains_key("r-1"));
    assert!(h.app.agents.contains_key("r-2"));
    assert_eq!(h.app.slots.get("r-1"), None);
    assert_eq!(h.finished(), [(0, "r-1".to_string(), Outcome::Evicted)]);
    assert!(h.app.is_starting("p-1"));
    assert_eq!(h.app.status, "freed slot from parked r-1 for p-1");
    assert_eq!(h.state("r-1"), review);
    assert!(h.app.git.worktree_path("r-1").join(".git").exists());
    h.app.reconcile();
    assert!(!h.app.is_starting("r-1"));
}

#[test]
fn parked_agents_keep_their_slots_without_ready_work() {
    let review = Some(State::Review);
    let mut h = Harness::new("evict-idle", &[("r-1", review), ("r-2", review)]);
    park(&mut h, &["r-1", "r-2"]);
    h.app.schedule();
    assert!(h.app.agents.contains_key("r-1") && h.app.agents.contains_key("r-2"));
    assert!(h.finished().is_empty());
    assert!(h.app.idle().is_some());
}

#[test]
fn parked_agents_with_work_in_flight_are_never_evicted() {
    let review = Some(State::Review);
    let mut h = Harness::new(
        "evict-busy",
        &[("r-1", review), ("r-2", review), ("p-1", None)],
    );
    park(&mut h, &["r-1", "r-2"]);
    h.app.ready = vec![h.bead("p-1")];
    let untouched = |h: &mut Harness| {
        h.app.schedule();
        assert!(h.app.agents.contains_key("r-1") && h.app.agents.contains_key("r-2"));
        assert!(h.finished().is_empty());
        assert!(!h.app.is_starting("p-1"));
        assert!(matches!(h.app.idle(), Some(Idle::SlotsFull { .. })));
    };
    h.app.publishing.insert("r-1".into());
    queued_check(&mut h, "r-2");
    untouched(&mut h);
    assert_eq!(
        h.app.idle().unwrap().describe(),
        format!(
            "all {} slots in use (0 working, parked: r-1, r-2; :max <n> to raise)",
            h.app.cfg.max_agents
        )
    );
    h.app.publishing.clear();
    h.app.republishing.insert("x".into(), "r-1".into());
    h.app.checks.get_mut("r-2").unwrap().status = checks::Status::Running;
    untouched(&mut h);
    h.app.republishing.clear();
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (_, record) = recorder();
    let bead = h.bead("r-1");
    h.app.launch_review(&bead, record);
    untouched(&mut h);
    drop(held);
    h.pump();
    assert_eq!(h.state("r-1"), Some(State::Reviewing));
    untouched(&mut h);
    h.app.checks.clear();
    h.app.schedule();
    assert!(!h.app.agents.contains_key("r-2"));
    assert_eq!(h.finished(), [(1, "r-2".to_string(), Outcome::Evicted)]);
    assert!(h.app.is_starting("p-1"));
}

#[test]
fn review_orphans_resume_only_into_slots_ready_work_does_not_want() {
    let review = Some(State::Review);
    let mut h = Harness::new(
        "evict-restart",
        &[("r-1", review), ("p-1", None), ("p-2", None)],
    );
    h.app.git.ensure_worktree("r-1", "main").unwrap();
    h.app.ready = vec![h.bead("p-1"), h.bead("p-2")];
    h.app.reconcile();
    assert!(!h.app.is_starting("r-1"));
    h.app.ready = vec![h.bead("p-1")];
    h.app.reconcile();
    assert!(h.app.is_starting("r-1"));
    h.pump();
    assert!(h.app.agents.contains_key("r-1"));
}

#[test]
fn feedback_to_an_evicted_bead_makes_room_or_says_why_not() {
    let review = Some(State::Review);
    let mut h = Harness::new(
        "evict-deliver",
        &[("r-1", review), ("r-2", review), ("r-3", review)],
    );
    park(&mut h, &["r-1", "r-2"]);
    h.app.git.ensure_worktree("r-3", "main").unwrap();
    assert!(h.app.deliver("r-3", "fix it"));
    assert_eq!(h.finished(), [(0, "r-1".to_string(), Outcome::Evicted)]);
    assert_eq!(h.state("r-3"), Some(State::Running));
    h.pump();
    assert!(h.app.agents.contains_key("r-3"));
    assert_eq!(h.app.slots.get("r-3"), Some(0));
    h.app
        .edit_local("r-2", |b| b.set_state_label(Some(State::Running)));
    assert!(!h.app.deliver("r-1", "again"));
    assert!(
        h.app.status.starts_with("no free agent slot for r-1"),
        "{}",
        h.app.status
    );
    assert_eq!(h.state("r-1"), review);
    assert!(!h.app.is_starting("r-1"));
    h.app.relaunch("r-1");
    assert!(h.app.status.starts_with("no free agent slot for r-1"));
    assert!(!h.app.is_starting("r-1"));
    h.app.edit_local("r-2", |b| b.set_state_label(review));
    h.app.relaunch("r-1");
    assert!(h.app.is_starting("r-1"));
    assert_eq!(
        h.finished().last(),
        Some(&(1, "r-2".to_string(), Outcome::Evicted))
    );
    h.pump();
    assert_eq!(h.app.slots.get("r-1"), Some(1));
    assert_eq!(h.state("r-1"), review);
}
