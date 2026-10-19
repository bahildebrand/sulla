use super::*;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use portable_pty::CommandBuilder;

use crate::agent::Launch;
use crate::config::ClaudeConfig;
use crate::git;
use crate::sandbox::Prepare;

type Finished = Arc<Mutex<Vec<(usize, String, Outcome)>>>;

struct Gated {
    gate: Arc<Mutex<()>>,
    prepared: Arc<Mutex<Vec<String>>>,
    finished: Finished,
}

impl Sandbox for Gated {
    fn name(&self) -> &str {
        "gated"
    }

    fn prepare(&mut self, _slot: usize, id: &str) -> Prepare {
        let (gate, prepared, id) = (self.gate.clone(), self.prepared.clone(), id.to_string());
        Box::new(move || {
            drop(gate.lock().unwrap());
            prepared.lock().unwrap().push(id);
            Ok(())
        })
    }

    fn wrap(&self, _slot: usize, _launch: &Launch, cmd: CommandBuilder) -> Result<CommandBuilder> {
        Ok(cmd)
    }

    fn finished(&mut self, slot: usize, id: &str, outcome: Outcome) -> Result<()> {
        self.finished
            .lock()
            .unwrap()
            .push((slot, id.to_string(), outcome));
        Ok(())
    }

    fn shutdown(&mut self) {}
}

struct Harness {
    dir: PathBuf,
    app: App,
    rx: Receiver<Event>,
    gate: Arc<Mutex<()>>,
    prepared: Arc<Mutex<Vec<String>>>,
    finished: Finished,
}

impl Harness {
    fn new(name: &str, beads: &[(&str, Option<State>)]) -> Self {
        let dir = std::env::temp_dir().join(format!("sulla-app-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        let cfg = Config {
            max_agents: 2,
            poll_secs: 3600,
            kache: false,
            agent: AgentKind::Claude,
            claude: ClaudeConfig {
                bin: "true".into(),
                ..ClaudeConfig::default()
            },
            ..Config::default()
        };
        let (tx, rx) = channel();
        let mut app = App::new(cfg, dir.clone(), tx).unwrap();
        let gate = Arc::new(Mutex::new(()));
        let prepared = Arc::new(Mutex::new(Vec::new()));
        let finished = Finished::default();
        app.sandbox = Box::new(Gated {
            gate: gate.clone(),
            prepared: prepared.clone(),
            finished: finished.clone(),
        });
        app.all = beads
            .iter()
            .map(|(id, state)| {
                let mut b: Bead = serde_json::from_value(serde_json::json!({
                    "id": id,
                    "title": id,
                    "status": "in_progress",
                }))
                .unwrap();
                b.set_state_label(*state);
                b
            })
            .collect();
        Self {
            dir,
            app,
            rx,
            gate,
            prepared,
            finished,
        }
    }

    fn bead(&self, id: &str) -> Bead {
        self.app.bead(id).cloned().unwrap()
    }

    fn state(&self, id: &str) -> Option<State> {
        self.bead(id).state()
    }

    fn pump(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.app.launching.is_empty()
            || !self.app.starting.is_empty()
            || self.app.bd_pending > 0
        {
            let wait = deadline.saturating_duration_since(Instant::now());
            let ev = self.rx.recv_timeout(wait).expect("launch did not finish");
            if matches!(ev, Event::Finished(_)) {
                self.app.dispatch(ev);
            }
        }
    }

    fn stack(&self, id: &str, parent: &str) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&self.dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        for b in [id, parent] {
            if !self.app.git.has_branch(b) {
                git(&["branch", &git::branch(b)]);
            }
        }
        git(&[
            "update-ref",
            &format!("refs/sulla/stack/{id}"),
            &git::branch(parent),
        ]);
        git(&["config", &format!("sulla.{id}.parent"), parent]);
    }

    fn prepared(&self) -> Vec<String> {
        self.prepared.lock().unwrap().clone()
    }

    fn finished(&self) -> Vec<(usize, String, Outcome)> {
        self.finished.lock().unwrap().clone()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.app.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Done = Arc<Mutex<Option<bool>>>;

fn recorder() -> (Done, impl FnOnce(&mut App, Result<()>) + Send + 'static) {
    let done = Done::default();
    let d = done.clone();
    (done, move |_: &mut App, r: Result<()>| {
        *d.lock().unwrap() = Some(r.is_ok());
    })
}

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
fn review_launch_marks_reviewing_when_it_starts() {
    let mut h = Harness::new("review", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_review(&bead, record);
    let rid = agent::review_id("t-1");
    assert!(h.app.is_starting(&rid));
    assert_eq!(h.app.agent_key("t-1"), rid);
    assert_eq!(h.state("t-1"), Some(State::Review));
    drop(held);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(true));
    assert!(h.app.agents.contains_key(&rid));
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
}

#[test]
fn review_launch_is_dropped_when_bead_leaves_review() {
    let mut h = Harness::new("review-left", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_review(&bead, record);
    h.app
        .edit_local("t-1", |b| b.set_state_label(Some(State::Running)));
    drop(held);
    h.pump();
    let rid = agent::review_id("t-1");
    assert_eq!(*done.lock().unwrap(), Some(false));
    assert!(!h.app.agents.contains_key(&rid));
    assert_eq!(h.app.slots.get(&rid), None);
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

fn chain(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn publish_chain_takes_unpublished_review_parents() {
    let review = Some(State::Review);
    let h = Harness::new("chain", &[("a", review), ("b", review), ("c", review)]);
    h.stack("b", "a");
    h.stack("c", "b");
    assert_eq!(h.app.publish_chain("c"), chain(&["a", "b", "c"]));
    assert_eq!(h.app.publish_chain("a"), chain(&["a"]));
}

#[test]
fn publish_chain_stops_at_awaiting_or_unreviewed_parent() {
    let h = Harness::new(
        "chain-stop",
        &[
            ("a", Some(State::Pr)),
            ("b", Some(State::Review)),
            ("c", Some(State::Review)),
            ("x", Some(State::Running)),
            ("y", Some(State::Review)),
        ],
    );
    h.stack("b", "a");
    h.stack("c", "b");
    h.stack("y", "x");
    assert_eq!(h.app.publish_chain("c"), chain(&["b", "c"]));
    assert_eq!(h.app.pr_base("b"), "sulla/a");
    assert_eq!(h.app.publish_chain("y"), chain(&["y"]));
}

#[test]
fn confirm_publish_offers_the_whole_stack() {
    let review = Some(State::Review);
    let mut h = Harness::new("chain-confirm", &[("a", review), ("b", review)]);
    h.stack("b", "a");
    h.app.view = View::Progress("b".into());
    let blockers = Ok(vec![vec![], vec!["a".to_string()]]);
    h.app
        .confirm_publish("b".into(), chain(&["a", "b"]), blockers);
    let Mode::Confirm {
        msg,
        action: Action::Publish(c),
    } = &h.app.mode
    else {
        panic!("no confirm, status: {}", h.app.status);
    };
    assert_eq!(c, &chain(&["a", "b"]));
    assert!(msg.contains("sulla/a, sulla/b"), "{msg}");
}

#[test]
fn confirm_publish_names_blockers_below_in_the_stack() {
    let review = Some(State::Review);
    let mut h = Harness::new("chain-blocked", &[("a", review), ("b", review)]);
    h.stack("b", "a");
    h.app.view = View::Progress("b".into());
    let blockers = Ok(vec![vec!["q".to_string()], vec!["a".to_string()]]);
    h.app
        .confirm_publish("b".into(), chain(&["a", "b"]), blockers);
    assert!(matches!(h.app.mode, Mode::Normal));
    assert_eq!(
        h.app.status,
        "cannot publish b: a below it is blocked by open q"
    );
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    assert_eq!(h.app.status, "cannot publish b: blocked by open a");
}

#[test]
fn failed_parent_publish_skips_the_rest_of_the_stack() {
    let review = Some(State::Review);
    let mut h = Harness::new("chain-fail", &[("a", review), ("b", review)]);
    h.stack("b", "a");
    h.app.publish(chain(&["a", "b"]));
    assert!(h.app.publishing.contains("a") && h.app.publishing.contains("b"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !h.app.publishing.is_empty() {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("publish did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
        }
    }
    assert!(
        h.app.status.ends_with(", did not publish b"),
        "{}",
        h.app.status
    );
    assert_eq!(h.state("a"), review);
    assert_eq!(h.state("b"), review);
}

#[test]
fn open_pr_needs_a_valid_pr_label() {
    let mut h = Harness::new("open-pr", &[("a", Some(State::Pr)), ("b", Some(State::Pr))]);
    h.app.all[1].labels.push("pr:x".into());
    h.app.open_pr("a");
    assert_eq!(h.app.status, "a has no PR");
    h.app.open_pr("b");
    assert_eq!(h.app.status, "b has an invalid PR label pr:x");
}
