use super::*;

#[test]
fn launch_reserves_slot_and_starts_agent_once_prepared() {
    let mut h = Harness::new("launch", &[("t-1", Some(State::Running)), ("t-2", None)]);
    h.app.ready = vec![h.bead("t-1"), h.bead("t-2")];
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    let called = Instant::now();
    h.app.launch_agent(&bead, false, None, record);
    assert!(called.elapsed() < Duration::from_secs(1));
    assert!(h.app.is_starting("t-1"));
    assert!(h.app.agents.is_empty());
    assert_eq!(h.app.slots.get("t-1"), Some(0));
    assert_eq!(h.app.free_slots(), 1);
    let picks: Vec<String> = h.app.picks(2).into_iter().map(|b| b.id).collect();
    assert_eq!(picks, ["t-2"]);
    assert_eq!(*done.lock().unwrap(), None);
    drop(held);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(true));
    assert!(h.app.agents.contains_key("t-1"));
    assert!(!h.app.is_starting("t-1"));
    assert_eq!(h.app.slots.get("t-1"), Some(0));
    assert!(h.app.git.worktree_path("t-1").join(".git").exists());
    assert_eq!(h.prepared(), ["t-1"]);
}

#[test]
fn kill_while_launching_holds_bead_and_frees_slot() {
    let mut h = Harness::new("kill", &[("t-1", Some(State::Running))]);
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_agent(&bead, false, None, record);
    h.app.kill("t-1");
    assert_eq!(h.state("t-1"), Some(State::Held));
    assert_eq!(h.app.slots.get("t-1"), Some(0));
    drop(held);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(false));
    assert!(h.app.agents.is_empty());
    assert_eq!(h.app.slots.get("t-1"), None);
    assert_eq!(h.app.free_slots(), 2);
    assert_eq!(h.finished(), [(0, "t-1".to_string(), Outcome::Exited)]);
}

#[test]
fn discard_while_launching_waits_for_the_job_then_discards() {
    let mut h = Harness::new("discard", &[("t-1", Some(State::Running))]);
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_agent(&bead, false, None, record);
    h.app.discard("t-1");
    assert!(h.app.status.contains("still launching"), "{}", h.app.status);
    drop(held);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(false));
    assert!(h.app.agents.is_empty());
    assert_eq!(h.app.slots.get("t-1"), None);
    assert!(!h.app.git.worktree_path("t-1").exists());
    assert!(!h.app.git.has_branch("t-1"));
    assert_eq!(h.state("t-1"), Some(State::Held));
}

#[test]
fn feedback_while_launching_reuses_the_launch() {
    let mut h = Harness::new("deliver", &[("t-1", Some(State::Running))]);
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_agent(&bead, false, None, record);
    h.app.deliver("t-1", "also do this");
    assert_eq!(h.app.slots.get("t-1"), Some(0));
    drop(held);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(true));
    assert!(h.app.agents.contains_key("t-1"));
    assert_eq!(h.prepared(), ["t-1"]);
}

#[test]
fn quit_while_launching_waits_for_the_job_and_starts_nothing() {
    let mut h = Harness::new("quit", &[("t-1", Some(State::Running))]);
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_agent(&bead, false, None, record);
    h.app.request_quit();
    assert!(!h.app.quit);
    assert!(matches!(&h.app.mode, Mode::Confirm { msg, .. } if msg.contains("1 agent launch")));
    drop(held);
    h.app.shutdown();
    assert_eq!(h.prepared(), ["t-1"]);
    assert!(h.app.launching.is_empty());
    assert!(h.app.agents.is_empty());
    assert_eq!(*done.lock().unwrap(), None);
}

#[test]
fn bd_worker_runs_jobs_in_order_without_blocking_sender() {
    let (tx, rx) = channel();
    let jobs = spawn_bd_worker(Beads::new(&PathBuf::from(".")), tx);
    let (order_tx, order_rx) = channel();
    let sent = Instant::now();
    for n in 0..3 {
        let order_tx = order_tx.clone();
        let job: BdJob = Box::new(move |_| {
            std::thread::sleep(Duration::from_millis(50));
            order_tx.send(n).unwrap();
            Box::new(|_| {})
        });
        jobs.send(job).unwrap();
    }
    assert!(sent.elapsed() < Duration::from_millis(50));
    for _ in 0..3 {
        let ev = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(ev, Event::Finished(_)));
    }
    assert_eq!(order_rx.try_iter().collect::<Vec<_>>(), [0, 1, 2]);
}

#[test]
fn setup_runs_once_per_worktree_and_reports_failure() {
    let mut h = Harness::new("setup", &[("t-1", Some(State::Running))]);
    h.app.cfg.hooks.setup = Some("echo $SULLA_BEAD >> ../setup-runs; exit 4".into());
    let runs = h.app.git.worktree_path("setup-runs");
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_agent(&bead, false, None, record);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(true));
    assert!(h.app.agents.contains_key("t-1"));
    assert!(
        h.app.status.contains("t-1: setup failed"),
        "{}",
        h.app.status
    );
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "t-1\n");
    let log = std::fs::read_to_string(h.app.agent_log("t-1")).unwrap();
    assert!(log.contains("setup failed"), "{log}");
    assert!(!h.app.git.needs_setup("t-1"));

    h.app.take_agent("t-1");
    let (done, record) = recorder();
    h.app.launch_agent(&bead, true, None, record);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(true));
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "t-1\n");

    h.app.command("setup t-1");
    assert!(h.app.status.contains("running setup"), "{}", h.app.status);
    let deadline = Instant::now() + Duration::from_secs(20);
    while h.app.setups.contains("t-1") {
        let ev =
            h.rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("setup did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
        }
    }
    assert!(
        h.app.status.contains("t-1: setup failed"),
        "{}",
        h.app.status
    );
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "t-1\nt-1\n");
}

#[test]
fn fresh_launches_never_reuse_a_session() {
    let mut h = Harness::new("fresh-sessions", &[("t-1", Some(State::Running))]);
    let bead = h.bead("t-1");
    for _ in 0..2 {
        let (done, record) = recorder();
        h.app.launch_agent(&bead, false, None, record);
        h.pump();
        assert_eq!(*done.lock().unwrap(), Some(true));
        h.app.take_agent("t-1");
    }
    let s = h.sessions("t-1");
    assert_eq!(s.len(), 2);
    assert_ne!(s[0], s[1]);
}

fn launch_and_park(h: &mut Harness, id: &str, resume: bool) {
    let bead = h.bead(id);
    let (done, record) = recorder();
    h.app.launch_agent(&bead, resume, None, record);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(true));
    h.app.take_agent(id);
}

fn touch_claude_session(h: &Harness, session: &str) {
    let dir = h.session_root().join("projects").join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{session}.jsonl")), "{}\n").unwrap();
}

#[test]
fn resume_uses_the_latest_recorded_session_after_a_restart() {
    let mut h = Harness::new("resume-session", &[("t-1", Some(State::Running))]);
    launch_and_park(&mut h, "t-1", false);
    launch_and_park(&mut h, "t-1", false);
    let started = h.sessions("t-1");
    assert_eq!(h.app.bead_sessions["t-1"], started);
    h.app.bead_sessions.clear();
    h.app.bead_sessions = h.app.git.sessions();
    assert_eq!(h.app.bead_sessions["t-1"], started);
    touch_claude_session(&h, &started[1]);
    launch_and_park(&mut h, "t-1", true);
    assert_eq!(h.sessions("t-1")[2], started[1]);
    assert_eq!(h.app.bead_sessions["t-1"], started);
}

#[test]
fn lost_session_is_rebuilt_under_the_local_workflow() {
    let mut h = Harness::new("lost-local", &[("t-1", Some(State::Running))]);
    assert_eq!(h.app.cfg.workflow, Workflow::Local);
    launch_and_park(&mut h, "t-1", false);
    launch_and_park(&mut h, "t-1", true);
    let s = h.sessions("t-1");
    assert_eq!(s.len(), 2);
    assert_ne!(s[0], s[1]);
    assert_eq!(h.app.bead_sessions["t-1"], s);
    let note = h.app.note.clone().unwrap_or_default();
    assert!(note.contains("agent session was lost"), "{note}");
}

#[test]
fn delete_forgets_recorded_sessions() {
    let mut h = Harness::new("delete-sessions", &[("t-1", Some(State::Running))]);
    launch_and_park(&mut h, "t-1", false);
    assert!(h.app.git.sessions().contains_key("t-1"));
    h.app.confirm(Action::Delete("t-1".into()));
    assert!(!h.app.bead_sessions.contains_key("t-1"));
    assert!(!h.app.git.sessions().contains_key("t-1"));
}
