use super::monitor::WAIT_CONFIRM;
use super::*;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use portable_pty::CommandBuilder;

use crate::agent::Launch;
use crate::checks;
use crate::config::{ClaudeConfig, PrTool};
use crate::git;
use crate::sandbox::Prepare;
use crate::scheduler::Idle;

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
            Ok(Vec::new())
        })
    }

    fn wrap(
        &self,
        _slot: usize,
        _kind: AgentKind,
        _launch: &Launch,
        cmd: CommandBuilder,
    ) -> Result<CommandBuilder> {
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

    fn loopback(&self, slot: usize) -> Option<String> {
        browse::loopback(slot)
    }
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
            notify: crate::config::NotifyConfig {
                methods: Vec::new(),
                ..crate::config::NotifyConfig::default()
            },
            ..Config::default()
        };
        let (tx, rx) = channel();
        let layers = crate::config::effective::Layers::load_from(
            None,
            &Config::repo_path(&dir),
            toml::Table::new(),
        )
        .unwrap();
        let mut app = App::new(cfg, layers, dir.clone(), tx).unwrap();
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
                git(&["branch", &self.app.git.branch(b)]);
            }
        }
        git(&[
            "update-ref",
            &format!("refs/sulla/stack/{id}"),
            &self.app.git.branch(parent),
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
fn confirm_publish_stacks_onto_a_parent_revising_its_pr() {
    let mut h = Harness::new(
        "chain-revising",
        &[
            ("a", Some(State::Revise)),
            ("b", Some(State::Review)),
            ("c", Some(State::Review)),
        ],
    );
    h.app.all[0].labels.push("pr:7".into());
    h.stack("b", "a");
    h.stack("c", "b");
    h.app.view = View::Progress("c".into());
    assert_eq!(h.app.publish_chain("c"), chain(&["b", "c"]));
    assert_eq!(h.app.pr_base("b"), "sulla/a");
    let blockers = Ok(vec![vec!["a".to_string()], vec!["b".to_string()]]);
    h.app
        .confirm_publish("c".into(), chain(&["b", "c"]), blockers);
    let Mode::Confirm {
        msg,
        action: Action::Publish(c),
    } = &h.app.mode
    else {
        panic!("no confirm, status: {}", h.app.status);
    };
    assert_eq!(c, &chain(&["b", "c"]));
    assert!(msg.contains("onto sulla/a"), "{msg}");
}

fn commit_on(dir: &Path, parent: &str, msg: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["commit-tree", "HEAD^{tree}", "-p", parent, "-m", msg])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "commit-tree {parent}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn confirmed_publish(h: &Harness) -> (String, Vec<String>) {
    let Mode::Confirm {
        msg,
        action: Action::Publish(c),
    } = &h.app.mode
    else {
        panic!("no confirm, status: {}", h.app.status);
    };
    (msg.clone(), c.clone())
}

#[test]
fn confirm_publish_restacks_onto_a_pr_blocker_that_lost_its_stack_parent() {
    let mut h = Harness::new(
        "adopt-stale",
        &[("a", Some(State::Pr)), ("b", Some(State::Review))],
    );
    h.app.all[0].labels.push("pr:7".into());
    let old = commit_on(&h.dir, "main", "a v1");
    let child = commit_on(&h.dir, &old, "b");
    let amended = commit_on(&h.dir, "main", "a v2");
    git_in(&h.dir, &["branch", "sulla/a", &amended]);
    git_in(&h.dir, &["branch", "sulla/b", &child]);
    git_in(&h.dir, &["update-ref", "refs/sulla/stack/b", &old]);
    assert_eq!(h.app.git.stack_parent("b"), None);
    h.app.view = View::Progress("b".into());
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    let (msg, c) = confirmed_publish(&h);
    assert_eq!(c, chain(&["b"]));
    assert!(msg.contains("against sulla/a"), "{msg}");
    assert_eq!(h.app.git.stack_parent("b").as_deref(), Some("a"));
    assert_eq!(git_in(&h.dir, &["rev-parse", "refs/sulla/stack/b"]), old);
}

#[test]
fn confirm_publish_stacks_onto_a_pr_blocker_from_the_fork_point() {
    let mut h = Harness::new(
        "adopt-fresh",
        &[("a", Some(State::Revise)), ("b", Some(State::Review))],
    );
    h.app.all[0].labels.push("pr:7".into());
    let a = commit_on(&h.dir, "main", "a");
    let b = commit_on(&h.dir, "main", "b");
    git_in(&h.dir, &["branch", "sulla/a", &a]);
    git_in(&h.dir, &["branch", "sulla/b", &b]);
    h.app.view = View::Progress("b".into());
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    let (msg, _) = confirmed_publish(&h);
    assert!(msg.contains("against sulla/a"), "{msg}");
    assert_eq!(h.app.git.stack_parent("b").as_deref(), Some("a"));
    let main = git_in(&h.dir, &["rev-parse", "main"]);
    assert_eq!(git_in(&h.dir, &["rev-parse", "refs/sulla/stack/b"]), main);
}

#[test]
fn confirm_publish_keeps_blocking_on_a_blocker_without_a_pr() {
    let mut h = Harness::new(
        "adopt-none",
        &[("a", Some(State::Running)), ("b", Some(State::Review))],
    );
    let a = commit_on(&h.dir, "main", "a");
    git_in(&h.dir, &["branch", "sulla/a", &a]);
    git_in(&h.dir, &["branch", "sulla/b", "main"]);
    h.app.view = View::Progress("b".into());
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    assert!(matches!(h.app.mode, Mode::Normal));
    assert_eq!(h.app.status, "cannot publish b: blocked by open a");
    assert_eq!(h.app.git.stack_parent("b"), None);
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
fn publish_conflict_hands_off_to_the_agent_then_republishes() {
    let review = Some(State::Review);
    let mut h = Harness::new("publish-conflict", &[("a", review), ("b", review)]);
    h.app.cfg.workflow = Workflow::Pr;
    h.app.cfg.pr.tool = PrTool::Graphite;
    h.stack("b", "a");
    h.app.git.ensure_worktree("b", "sulla/a").unwrap();
    let conflict = git::RebaseConflict {
        base: "sulla/a".into(),
        detail: "could not apply".into(),
    };
    h.app.publishing.insert("a".into());
    h.app
        .pushed("a".into(), Err(conflict.into()), Some("b".into()));
    h.pump();
    assert!(!h.app.publishing.contains("a"));
    assert_eq!(h.app.republishing.get("a").map(String::as_str), Some("b"));
    assert_eq!(h.state("a"), Some(State::Running));
    h.app.republish_resolved(&[]);
    assert!(h.app.publishing.is_empty());
    h.app.edit_local("a", |b| b.set_state_label(review));
    h.app.republish_resolved(&["a".to_string()]);
    assert!(h.app.republishing.is_empty());
    assert!(h.app.publishing.contains("a") && h.app.publishing.contains("b"));
}

#[test]
fn publish_conflict_is_reported_without_a_republish_target() {
    let review = Some(State::Review);
    let mut h = Harness::new("publish-conflict-off", &[("a", review)]);
    let conflict = git::RebaseConflict {
        base: "main".into(),
        detail: "could not apply".into(),
    };
    h.app.publishing.insert("a".into());
    h.app.pushed("a".into(), Err(conflict.into()), None);
    assert!(h.app.republishing.is_empty());
    assert_eq!(h.state("a"), review);
    assert!(
        h.app.status.contains("rebase onto main failed"),
        "{}",
        h.app.status
    );
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

#[test]
fn pr_feedback_notifies_without_queueing_a_revision() {
    let mut h = Harness::new("pr-feedback", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let comment = crate::pr::PrComment {
        id: "c1".into(),
        author: "rev".into(),
        body: "rename this".into(),
        path: None,
        line: None,
        created_at: None,
        approval: false,
        bot: false,
    };
    let status = crate::pr::PrStatus {
        state: crate::pr::PrState::Open,
        comments: vec![comment],
        checks: crate::pr::Checks::None,
        head: None,
        viewer: None,
    };
    h.app.on_pr("a", 7, Ok(status));
    assert_eq!(h.state("a"), Some(State::Pr));
    assert_eq!(h.app.status, "PR #7 for a: 1 new comment, U to revise");
    assert_eq!(h.app.pr_watch.beads["a"].pending.count(), 1);
}

fn park(h: &mut Harness, ids: &[&str]) {
    for (age, id) in ids.iter().enumerate() {
        let bead = h.bead(id);
        let (done, record) = recorder();
        h.app.launch_agent(&bead, false, None, record);
        h.pump();
        assert_eq!(*done.lock().unwrap(), Some(true));
        let at = Utc::now() - chrono::Duration::minutes(10 - age as i64);
        h.app.edit_local(id, |b| b.state_changed_at = Some(at));
    }
}

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

fn pr_status(state: crate::pr::PrState, comments: &[&str]) -> crate::pr::PrStatus {
    crate::pr::PrStatus {
        state,
        comments: comments
            .iter()
            .map(|id| crate::pr::PrComment {
                id: id.to_string(),
                author: "rev".into(),
                body: "rename this".into(),
                path: None,
                line: None,
                created_at: None,
                approval: false,
                bot: false,
            })
            .collect(),
        checks: crate::pr::Checks::None,
        head: None,
        viewer: None,
    }
}

fn worked(h: &Harness, id: &str, onto: &str) -> std::path::PathBuf {
    let wt = h.app.git.ensure_worktree(id, onto).unwrap();
    std::fs::write(wt.join(format!("{id}.txt")), "x\n").unwrap();
    h.app
        .git
        .commit_pending(id, &format!("{id}: work"))
        .unwrap();
    wt
}

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
fn new_bead_form_steps_through_acceptance_before_priority() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut h = Harness::new("new-bead-acceptance", &[]);
    h.app.mode = Mode::NewBead(NewBeadForm::new(Vec::new()));
    let key = |h: &mut Harness, code: KeyCode| {
        h.app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    };
    key(&mut h, KeyCode::Tab);
    key(&mut h, KeyCode::Char('d'));
    key(&mut h, KeyCode::Enter);
    key(&mut h, KeyCode::Char('a'));
    h.app.on_paste("b\r\nc");
    key(&mut h, KeyCode::Enter);
    key(&mut h, KeyCode::Char('0'));
    let Mode::NewBead(form) = &h.app.mode else {
        panic!("form closed");
    };
    assert_eq!(form.description, "d");
    assert_eq!(form.acceptance, "ab\nc");
    assert_eq!(form.priority, 0);
    assert_eq!(form.field, 3);
}

#[test]
fn config_command_opens_the_viewer_with_runtime_changes() {
    use crate::config::effective::Source;
    use crate::ui::config::Row;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut h = Harness::new("config-view", &[]);
    let key = |h: &mut Harness, c: char| {
        h.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    };
    let source = |h: &Harness, path: &str| {
        let Mode::Config(v) = &h.app.mode else {
            panic!("config viewer not open");
        };
        v.sections
            .iter()
            .flat_map(|s| &s.items)
            .find(|i| i.path.join(".") == path)
            .map(|i| (i.source, i.value.clone()))
            .unwrap()
    };
    h.app.command("max 5");
    h.app.command("config");
    assert_eq!(
        source(&h, "max_agents"),
        (Source::Runtime, Some(toml::Value::Integer(5)))
    );
    assert_eq!(
        source(&h, "poll_secs"),
        (Source::Runtime, Some(toml::Value::Integer(3600)))
    );
    assert_eq!(source(&h, "branch_prefix").0, Source::Default);
    key(&mut h, '/');
    for c in "max_ag".chars() {
        key(&mut h, c);
    }
    h.app
        .on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    key(&mut h, 'y');
    assert_eq!(h.app.status, "max_agents = 5");
    let Mode::Config(v) = &h.app.mode else {
        panic!("closed");
    };
    assert!(!v.typing);
    assert_eq!(v.filter, "max_ag");
    assert!(matches!(v.current(), Some(Row::Item(..))));
    h.app
        .on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let Mode::Config(v) = &h.app.mode else {
        panic!("Esc closed before clearing the filter");
    };
    assert!(v.filter.is_empty());
    key(&mut h, 'z');
    key(&mut h, 'M');
    let Mode::Config(v) = &h.app.mode else {
        panic!("closed");
    };
    assert!(v.rows().iter().all(|r| matches!(r, Row::Header(_))));
    key(&mut h, 'q');
    assert!(matches!(h.app.mode, Mode::Normal));
}

#[test]
fn every_view_renders_at_any_terminal_size() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut h = Harness::new(
        "render",
        &[
            ("r-1", Some(State::Backlog)),
            ("r-2", None),
            ("r-3", Some(State::Running)),
            ("r-4", Some(State::Review)),
            ("r-5", Some(State::Held)),
        ],
    );
    for b in &mut h.app.all {
        b.description = "a fairly long description line that will need to be cut".into();
    }
    let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n ctx\n";
    let mut d = crate::ui::review::Diff::parse("r-4", diff);
    d.move_cursor(5);
    d.add_comment("please rename", 1);
    let views = [View::Board, View::Progress("r-3".into()), View::Review(d)];
    let modes = || {
        vec![
            Mode::Normal,
            Mode::Insert,
            Mode::Help { scroll: 3 },
            Mode::Health,
            Mode::Config(Box::new(
                crate::ui::config::ConfigView::new(&h.app.layers, &h.app.cfg, &h.app.reload)
                    .unwrap(),
            )),
            Mode::NewBead(NewBeadForm::new(vec!["rust".into()])),
            Mode::Inspect {
                id: "r-1".into(),
                text: "TITLE\n\nDESCRIPTION\n  body\n".into(),
                scroll: 0,
            },
            Mode::Confirm {
                action: Action::Discard("r-1".into()),
                msg: "discard r-1? y/n".into(),
            },
            Mode::Input {
                kind: InputKind::Command,
                buf: "max 3".into(),
            },
            Mode::Input {
                kind: InputKind::Comment(2),
                buf: "a comment".into(),
            },
        ]
    };
    for view in views {
        h.app.view = view;
        for zoom in [false, true] {
            h.app.zoom = zoom;
            for mode in modes() {
                h.app.mode = mode;
                for (w, ht) in [(200, 50), (80, 24), (24, 8), (6, 3), (1, 1)] {
                    let mut t = Terminal::new(TestBackend::new(w, ht)).unwrap();
                    t.draw(|f| crate::ui::draw(f, &h.app)).unwrap();
                }
            }
        }
    }
}

#[test]
fn auto_review_starts_a_reviewer_when_the_implementer_finishes() {
    let mut h = Harness::new("auto-review", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let rid = agent::review_id("t-1");
    h.app.ready_for_review("t-1", true);
    assert!(!h.app.is_starting(&rid));
    h.app.cfg.ai_review.auto = true;
    h.app.ready_for_review("t-1", false);
    assert!(!h.app.is_starting(&rid));
    h.app.ready_for_review("t-1", true);
    assert!(h.app.is_starting(&rid));
    assert_eq!(
        h.app.status,
        "starting auto ai review of t-1 (round 1/3)..."
    );
    h.pump();
    assert!(h.app.agents.contains_key(&rid));
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
    assert_eq!(h.app.auto_rounds.get("t-1"), Some(&1));
}

#[test]
fn auto_review_stops_after_max_rounds_and_resets() {
    let mut h = Harness::new("auto-review-max", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    h.app.cfg.ai_review.auto = true;
    h.app.cfg.ai_review.max_rounds = 2;
    h.app.auto_rounds.insert("t-1".into(), 2);
    let rid = agent::review_id("t-1");
    h.app.ready_for_review("t-1", true);
    assert!(!h.app.is_starting(&rid));
    assert!(!h.app.auto_rounds.contains_key("t-1"));
    assert_eq!(
        h.app.status,
        "auto ai review of t-1: stopped after 2 round(s), left in review"
    );
    h.app.ready_for_review("t-1", true);
    assert!(h.app.is_starting(&rid));
    h.pump();
    assert_eq!(h.app.auto_rounds.get("t-1"), Some(&1));
}

#[test]
fn auto_review_skips_released_beads_and_counts_nothing_when_it_cannot_start() {
    let mut h = Harness::new(
        "auto-review-skip",
        &[("a", Some(State::Released)), ("b", Some(State::Review))],
    );
    h.app.cfg.ai_review.auto = true;
    h.app.ready_for_review("a", true);
    assert!(!h.app.is_starting(&agent::review_id("a")));
    assert!(!h.app.auto_rounds.contains_key("a"));
    h.app.ready_for_review("b", true);
    h.pump();
    assert!(!h.app.agents.contains_key(&agent::review_id("b")));
    assert!(!h.app.auto_rounds.contains_key("b"));
    assert_eq!(h.state("b"), Some(State::Review));
}

fn launched_args(app: &App) -> Vec<String> {
    let launch = crate::agent::Launch {
        id: "x-1".into(),
        session: "s-1".into(),
        bead: true,
        worktree: PathBuf::from("/w"),
        resume: false,
        prompt: None,
        model: None,
    };
    app.backend(AgentKind::Claude)
        .command(&launch)
        .get_argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn write_repo_config(h: &Harness, body: &str) {
    let path = Config::repo_path(&h.dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn reload_applies_live_keys_and_holds_restart_keys() {
    use crate::config::SandboxKind;
    let mut h = Harness::new("reload-live", &[]);
    assert!(!launched_args(&h.app).contains(&"opus".to_string()));
    write_repo_config(
        &h,
        "max_agents = 4\nsandbox = \"incus\"\n[claude]\nmodel = \"opus\"\n[incus]\nmemory = \"4GiB\"\n[pr]\npoll_secs = 30",
    );
    h.app.command("reload");
    assert_eq!(
        h.app.status,
        "config reloaded: 3 changed (claude.model, max_agents 2→4, pr.poll_secs 120→30) · restart needed for incus.memory, sandbox"
    );
    assert_eq!(h.app.cfg.max_agents, 4);
    assert_eq!(h.app.cfg.claude.model.as_deref(), Some("opus"));
    assert_eq!(h.app.cfg.claude.bin, "true");
    assert_eq!(h.app.cfg.poll_secs, 3600);
    assert_eq!(h.app.cfg.sandbox, SandboxKind::Worktree);
    assert_eq!(h.app.cfg.incus.memory, "8GiB");
    assert_eq!(h.app.pr_every.get(), Duration::from_secs(30));
    assert_eq!(h.app.poll_every.get(), Duration::from_secs(3600));
    let args = launched_args(&h.app);
    assert_eq!(args[0], "true");
    assert!(
        args.windows(2).any(|w| w == ["--model", "opus"]),
        "{args:?}"
    );
    assert_eq!(h.app.reload.restart.len(), 2);

    h.app.command("config");
    let Mode::Config(v) = &h.app.mode else {
        panic!("config viewer not open");
    };
    let memory = v
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.dotted() == "incus.memory")
        .unwrap();
    assert_eq!(memory.value, Some(toml::Value::String("8GiB".into())));
    assert_eq!(
        memory.restart,
        Some(Some(toml::Value::String("4GiB".into())))
    );
    let max = v
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.dotted() == "max_agents")
        .unwrap();
    assert!(v.flashing(max));

    write_repo_config(
        &h,
        "max_agents = 4\npoll_secs = 7\n[claude]\nmodel = \"opus\"",
    );
    h.app.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('r'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        h.app.status,
        "config reloaded: 2 changed (poll_secs 3600→7, pr.poll_secs 30→120)"
    );
    assert!(h.app.reload.restart.is_empty());
    assert_eq!(h.app.poll_every.get(), Duration::from_secs(7));
    let Mode::Config(v) = &h.app.mode else {
        panic!("r closed the viewer");
    };
    assert_eq!(v.flash.as_deref(), Some(h.app.status.as_str()));

    h.app.command("reload");
    assert_eq!(h.app.status, "config reloaded: no changes");
}

#[test]
fn reload_keeps_runtime_overrides_whose_file_value_did_not_change() {
    let mut h = Harness::new("reload-runtime", &[]);
    h.app.command("max 6");
    write_repo_config(&h, "[stall]\nminutes = 9");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 6);
    assert_eq!(h.app.cfg.stall.minutes, 9);
    write_repo_config(&h, "max_agents = 1\n[stall]\nminutes = 9");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 1);
}

#[test]
fn invalid_config_leaves_the_running_config_untouched() {
    let mut h = Harness::new("reload-invalid", &[]);
    write_repo_config(&h, "max_agents = 4\n");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 4);
    write_repo_config(&h, "max_agents = 5\n[claude\n");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 4);
    assert!(
        h.app.status.starts_with(crate::config::reload::FAILED),
        "{}",
        h.app.status
    );
    assert!(
        h.app.status.contains("config.toml line 2"),
        "{}",
        h.app.status
    );
    assert!(h.app.reload.error.is_some());
    assert_eq!(h.app.layers.config().unwrap().max_agents, 4);
    write_repo_config(&h, "max_agents = 5\n");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 5);
    assert!(h.app.reload.error.is_none());
}

#[test]
fn editing_the_config_file_reloads_automatically() {
    let mut h = Harness::new("reload-auto", &[]);
    h.app.handle(Event::Tick);
    write_repo_config(&h, "max_agents = 7\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.app.cfg.max_agents != 7 && Instant::now() < deadline {
        h.app.handle(Event::Tick);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.app.cfg.max_agents, 7);
    assert_eq!(h.app.status, "config reloaded: 1 changed (max_agents 2→7)");
}

#[test]
fn browse_runs_the_app_lifecycle_off_the_ui_thread() {
    let mut h = Harness::new("browse", &[("t-1", Some(State::Running))]);
    h.app.cfg.sandbox = crate::config::SandboxKind::Incus;
    h.app.cfg.incus.browse = crate::config::BrowseConfig {
        ports: vec![8000],
        up: Some("echo booting".into()),
        ready: Some("echo not yet; false".into()),
        ready_timeout_secs: 1,
        ..Default::default()
    };
    h.app.browse("t-1");
    assert_eq!(
        h.app.status,
        "browse: t-1 has no slot or worktree, press A to start it"
    );
    assert_eq!(h.app.slots.acquire("t-1", 2), Some(0));
    std::fs::create_dir_all(h.app.git.worktree_path("t-1")).unwrap();
    let called = Instant::now();
    h.app.browse("t-1");
    assert!(called.elapsed() < Duration::from_millis(500));
    assert_eq!(h.app.status, "browse t-1: checking the app…");
    h.app.browse("t-1");
    assert_eq!(h.app.status, "browse t-1: already in progress");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while h.app.browsing.contains("t-1") {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("browse did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
            seen.push(h.app.status.clone());
        }
    }
    assert!(
        seen.contains(&"browse t-1: starting app…".to_string()),
        "{seen:?}"
    );
    let last = seen.last().unwrap();
    assert!(
        last.starts_with("browse t-1: app was not ready within 1s"),
        "{last}"
    );
    assert!(last.contains(" | booting"), "{last}");
    assert!(last.contains(" | not yet"), "{last}");
    assert!(!last.contains("--- "), "{last}");
    assert!(h.dir.join(".sulla/logs/t-1.app.log").exists());
    h.app.browse("stop t-1");
    assert_eq!(
        h.app.status,
        "browse: browse stop needs [incus.browse] down"
    );
}

fn browse_cfg(h: &mut Harness, browse: crate::config::BrowseConfig) {
    h.app.cfg.sandbox = crate::config::SandboxKind::Incus;
    h.app.cfg.incus.browse = browse;
}

fn drain_browse(h: &mut Harness, label: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while h.app.browsing.contains(label) {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("browse did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
            seen.push(h.app.status.clone());
        }
    }
    seen
}

#[test]
fn browse_records_the_app_owner_on_up_and_clears_it_on_down_or_reset() {
    let mut h = Harness::new("app-owner", &[("t-1", Some(State::Running))]);
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            up: Some("true".into()),
            down: Some("true".into()),
            ready: Some("false".into()),
            ready_timeout_secs: 1,
            ..Default::default()
        },
    );
    assert_eq!(h.app.slots.acquire("t-1", 2), Some(0));
    std::fs::create_dir_all(h.app.git.worktree_path("t-1")).unwrap();
    h.app.browse("t-1");
    drain_browse(&mut h, "t-1");
    assert_eq!(h.app.apps.owner(0), Some("t-1"));
    h.app.publish_slots();
    assert_eq!(browse::read_own_apps(&h.dir).owner(0), Some("t-1"));

    h.app.browse("stop t-1");
    drain_browse(&mut h, "t-1");
    assert_eq!(h.app.apps.owner(0), None);
    h.app.publish_slots();
    assert!(browse::read_own_apps(&h.dir).is_empty());

    h.app.apps.started(0, "t-1");
    h.app.slots.release("t-1");
    h.app.finish(Some(0), "t-1", Outcome::Exited);
    assert_eq!(h.app.apps.owner(0), Some("t-1"));
    h.app.finish(Some(0), "t-1", Outcome::Killed);
    assert_eq!(h.app.apps.owner(0), None);
    h.app.apps.started(0, "t-1");
    h.app.finish(Some(0), "t-1", Outcome::Discarded);
    assert_eq!(h.app.apps.owner(0), None);
}

#[test]
fn publishing_slots_keeps_app_owners_another_process_recorded() {
    let mut h = Harness::new("app-merge", &[("t-1", Some(State::Running))]);
    h.app.slots.acquire("t-1", 2);
    h.app.apps.started(0, "t-1");
    h.app.publish_slots();
    let mut theirs = browse::read_own_apps(&h.dir);
    theirs.started(1, "t-2");
    browse::write_slots(&h.dir, &h.app.slots.snapshot(), &theirs).unwrap();
    h.app.slots.acquire("t-3", 2);
    h.app.apps.stopped(0);
    h.app.publish_slots();
    let file = browse::read_own_apps(&h.dir);
    assert_eq!(file.owner(0), None);
    assert_eq!(file.owner(1), Some("t-2"));
    assert_eq!(h.app.apps, file);
}

#[test]
fn browse_asks_before_replacing_another_beads_app() {
    let mut h = Harness::new("app-stale", &[("t-1", Some(State::Running))]);
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            up: Some("true".into()),
            ready: Some("false".into()),
            ready_timeout_secs: 1,
            ..Default::default()
        },
    );
    assert_eq!(h.app.slots.acquire("t-1", 2), Some(0));
    std::fs::create_dir_all(h.app.git.worktree_path("t-1")).unwrap();
    h.app.apps.started(0, "o-9");
    h.app.browse("t-1");
    let Mode::Confirm { msg, action } = std::mem::replace(&mut h.app.mode, Mode::Normal) else {
        panic!("no confirm");
    };
    assert_eq!(msg, "slot 0 is serving o-9; restart app for t-1? y/n");
    assert!(!h.app.browsing.contains("t-1"));
    assert!(matches!(&action, Action::RestartApp(t) if t == "t-1"));
    h.app.confirm(action);
    assert!(h.app.browsing.contains("t-1"));
    let seen = drain_browse(&mut h, "t-1");
    assert!(
        seen.contains(&"browse t-1: starting app…".to_string()),
        "{seen:?}"
    );
    assert_eq!(h.app.apps.owner(0), Some("t-1"));
}

#[test]
fn probing_runs_off_the_ui_thread_and_skips_without_ports() {
    let mut h = Harness::new("app-probe", &[("t-1", Some(State::Running))]);
    h.app.slots.acquire("t-1", 2);
    h.app.probe_apps();
    assert!(!h.app.probing);
    assert!(h.app.app_live.is_empty());
    let held = std::net::TcpListener::bind("127.0.100.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    h.app.cfg.incus.browse.ports = vec![port];
    h.app.probed = None;
    let called = Instant::now();
    h.app.probe_apps();
    assert!(called.elapsed() < Duration::from_millis(100));
    assert!(h.app.probing);
    h.app.probe_apps();
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.app.probing {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("probe did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
        }
    }
    assert_eq!(h.app.app_live.get(&0), Some(&browse::Live::Up));
    assert_eq!(h.app.app_status("t-1"), Some((browse::Live::Up, None)));
    assert_eq!(h.app.app_status(&agent::review_id("t-1")), None);
    drop(held);
}

fn screen(app: &App, w: u16, ht: u16) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut t = Terminal::new(TestBackend::new(w, ht)).unwrap();
    t.draw(|f| crate::ui::draw(f, app)).unwrap();
    t.backend().buffer().clone()
}

fn find(buf: &ratatui::buffer::Buffer, text: &str) -> Option<ratatui::style::Color> {
    let area = buf.area;
    for y in 0..area.height {
        let row: Vec<&str> = (0..area.width).map(|x| buf[(x, y)].symbol()).collect();
        let line: String = row.concat();
        if let Some(at) = line.find(text) {
            let x = line[..at].chars().count() as u16;
            return Some(buf[(x, y)].fg);
        }
    }
    None
}

#[test]
fn app_status_shows_on_cards_titles_and_review_header() {
    use crate::ui::theme;
    let mut h = Harness::new("app-badge", &[("t-1", Some(State::Running))]);
    h.app.slots.acquire("t-1", 2);
    assert_eq!(find(&screen(&h.app, 200, 40), "app"), None);
    h.app.app_live.insert(0, browse::Live::Up);
    h.app.col = Column::ALL
        .iter()
        .position(|c| *c == Column::InProgress)
        .unwrap();
    for zoom in [false, true] {
        h.app.zoom = zoom;
        assert_eq!(find(&screen(&h.app, 200, 40), "▲ app"), Some(theme::GREEN));
    }
    h.app.app_live.insert(0, browse::Live::Partial);
    assert_eq!(find(&screen(&h.app, 200, 40), "◐ app"), Some(theme::YELLOW));
    h.app.app_live.insert(0, browse::Live::Down);
    assert_eq!(find(&screen(&h.app, 200, 40), "▼ app"), Some(theme::MUTED));
    h.app.app_live.insert(0, browse::Live::Up);
    h.app.apps.started(0, "o-9");
    assert_eq!(
        find(&screen(&h.app, 200, 40), "▲ app (o-9)"),
        Some(theme::PEACH)
    );
    h.app.view = View::Progress("t-1".into());
    assert_eq!(
        find(&screen(&h.app, 200, 40), "▲ app (o-9)"),
        Some(theme::PEACH)
    );
    h.app.apps.started(0, "t-1");
    let buf = screen(&h.app, 200, 40);
    assert_eq!(find(&buf, "▲ app"), Some(theme::GREEN));
    assert_eq!(find(&buf, "▲ app ("), None);
    let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    h.app.view = View::Review(crate::ui::review::Diff::parse("t-1", diff));
    assert_eq!(find(&screen(&h.app, 200, 40), "▲ app"), Some(theme::GREEN));
}

#[test]
fn browse_resumes_an_evicted_bead_and_protects_it_from_eviction() {
    let review = Some(State::Review);
    let mut h = Harness::new(
        "browse-resume",
        &[
            ("r-1", review),
            ("r-2", review),
            ("r-3", review),
            ("p-1", None),
        ],
    );
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            up: Some("true".into()),
            ready: Some("false".into()),
            ready_timeout_secs: 1,
            ..Default::default()
        },
    );
    park(&mut h, &["r-1", "r-2"]);
    h.app.git.ensure_worktree("r-3", "main").unwrap();
    h.app.browse("r-3");
    assert_eq!(h.finished(), [(0, "r-1".to_string(), Outcome::Evicted)]);
    assert!(h.app.is_starting("r-3"));
    h.pump();
    assert_eq!(h.app.slots.get("r-3"), Some(0));
    assert!(h.app.browsing.contains("r-3"));
    let seen = drain_browse(&mut h, "r-3");
    assert!(
        seen.contains(&"browse r-3: starting app…".to_string()),
        "{seen:?}"
    );
    assert_eq!(h.app.apps.owner(0), Some("r-3"));
    assert_eq!(h.state("r-3"), review);

    h.app.ready = vec![h.bead("p-1")];
    h.app.schedule();
    assert_eq!(
        h.finished().last(),
        Some(&(1, "r-2".to_string(), Outcome::Evicted))
    );
    assert!(h.app.agents.contains_key("r-3"));
}

#[test]
fn browse_without_a_free_slot_says_why() {
    let review = Some(State::Review);
    let mut h = Harness::new(
        "browse-full",
        &[("r-1", review), ("r-2", review), ("r-3", review)],
    );
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            ..Default::default()
        },
    );
    park(&mut h, &["r-1", "r-2"]);
    for id in ["r-1", "r-2"] {
        h.app
            .edit_local(id, |b| b.set_state_label(Some(State::Running)));
    }
    h.app.git.ensure_worktree("r-3", "main").unwrap();
    h.app.browse("r-3");
    assert!(
        h.app.status.starts_with("no free agent slot for r-3"),
        "{}",
        h.app.status
    );
    assert!(!h.app.is_starting("r-3"));
    assert!(h.finished().is_empty());
    h.app.browse("stop r-3");
    assert_eq!(h.app.status, "browse: r-3 has no slot, so no app to stop");
    h.app.browse("x-9");
    assert_eq!(h.app.status, "browse: no bead x-9");
}

mod config_editing {
    use super::*;
    use crate::config::effective::Source;
    use crate::ui::config::{ConfigView, Prompt, Row};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::os::unix::process::ExitStatusExt;

    fn press(h: &mut Harness, code: KeyCode) {
        h.app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl(h: &mut Harness, c: char) {
        h.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn typed(h: &mut Harness, s: &str) {
        for c in s.chars() {
            press(h, KeyCode::Char(c));
        }
    }

    fn view(h: &Harness) -> &ConfigView {
        match &h.app.mode {
            Mode::Config(v) => v,
            _ => panic!("config viewer not open"),
        }
    }

    fn view_mut(h: &mut Harness) -> &mut ConfigView {
        match &mut h.app.mode {
            Mode::Config(v) => v,
            _ => panic!("config viewer not open"),
        }
    }

    fn goto(h: &mut Harness, path: &str) {
        let v = view_mut(h);
        let row = v
            .sections
            .iter()
            .enumerate()
            .find_map(|(s, sec)| {
                sec.items
                    .iter()
                    .position(|i| i.path.join(".") == path)
                    .map(|i| Row::Item(s, i))
            })
            .unwrap_or_else(|| panic!("no {path}"));
        assert!(v.select(row));
    }

    fn editor(h: &Harness) -> &crate::ui::config::Editor {
        match &view(h).prompt {
            Some(Prompt::Edit(ed)) => ed,
            p => panic!("no editor: {p:?}"),
        }
    }

    fn with_global(name: &str) -> (Harness, PathBuf) {
        let h = Harness::new(name, &[]);
        let global = h.dir.join("home").join("sulla").join("config.toml");
        let mut h = h;
        h.app.layers = crate::config::effective::Layers::load_from(
            Some(&global),
            &Config::repo_path(&h.dir),
            toml::Table::new(),
        )
        .unwrap();
        (h, global)
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn inline_edit_writes_the_repo_file_and_reloads() {
        let mut h = Harness::new("cfg-inline", &[]);
        write_repo_config(&h, "# repo settings\nmax_agents = 4 # few\n");
        h.app.command("reload");
        h.app.command("config");
        goto(&mut h, "max_agents");
        press(&mut h, KeyCode::Enter);
        assert_eq!(editor(&h).buf, "4");
        assert_eq!(editor(&h).target, Source::Repo);
        ctrl(&mut h, 'u');
        typed(&mut h, "6");
        press(&mut h, KeyCode::Enter);
        assert!(view(&h).prompt.is_none());
        assert_eq!(
            read(&Config::repo_path(&h.dir)),
            "# repo settings\nmax_agents = 6 # few\n"
        );
        assert_eq!(h.app.cfg.max_agents, 6);
        assert_eq!(
            h.app.status,
            "set max_agents = 6 in repo · config reloaded: 1 changed (max_agents 4→6)"
        );
        assert_eq!(view(&h).flash.as_deref(), Some(h.app.status.as_str()));
        assert!(matches!(view(&h).current(), Some(Row::Item(..))));
        let max = view(&h)
            .sections
            .iter()
            .flat_map(|s| &s.items)
            .find(|i| i.dotted() == "max_agents")
            .unwrap();
        assert_eq!(max.source, Source::Repo);
    }

    #[test]
    fn inline_saves_replace_a_runtime_override_of_the_same_key() {
        let mut h = Harness::new("cfg-runtime", &[]);
        h.app.command("max 5");
        h.app.command("config");
        goto(&mut h, "max_agents");
        press(&mut h, KeyCode::Enter);
        assert_eq!(editor(&h).buf, "5");
        ctrl(&mut h, 'u');
        typed(&mut h, "3");
        press(&mut h, KeyCode::Enter);
        assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 3\n");
        assert_eq!(h.app.cfg.max_agents, 3);
        assert_eq!(h.app.cfg.poll_secs, 3600);
    }

    #[test]
    fn invalid_inline_values_keep_the_editor_open_and_the_file_untouched() {
        let mut h = Harness::new("cfg-invalid-inline", &[]);
        write_repo_config(&h, "max_agents = 4\n");
        h.app.command("reload");
        h.app.command("config");
        goto(&mut h, "max_agents");
        press(&mut h, KeyCode::Char('i'));
        ctrl(&mut h, 'u');
        typed(&mut h, "-2");
        press(&mut h, KeyCode::Enter);
        let err = editor(&h).error.clone().unwrap();
        assert!(err.contains("invalid value"), "{err}");
        assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 4\n");
        assert_eq!(h.app.cfg.max_agents, 4);
        press(&mut h, KeyCode::Backspace);
        assert!(editor(&h).error.is_none());
        press(&mut h, KeyCode::Esc);
        assert!(view(&h).prompt.is_none());
        assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 4\n");
    }

    #[test]
    fn enums_cycle_with_tab_and_bools_toggle_with_space() {
        let mut h = Harness::new("cfg-enum", &[]);
        h.app.command("config");
        goto(&mut h, "agent");
        press(&mut h, KeyCode::Enter);
        assert_eq!(editor(&h).buf, "claude");
        press(&mut h, KeyCode::Tab);
        assert_eq!(editor(&h).buf, "copilot");
        press(&mut h, KeyCode::BackTab);
        press(&mut h, KeyCode::BackTab);
        assert_eq!(editor(&h).buf, "copilot");
        press(&mut h, KeyCode::Enter);
        assert_eq!(read(&Config::repo_path(&h.dir)), "agent = \"copilot\"\n");
        assert_eq!(h.app.cfg.agent, AgentKind::Copilot);

        goto(&mut h, "pr.draft");
        press(&mut h, KeyCode::Char(' '));
        assert_eq!(
            read(&Config::repo_path(&h.dir)),
            "agent = \"copilot\"\n\n[pr]\ndraft = true\n"
        );
        assert!(h.app.cfg.pr.draft);
        press(&mut h, KeyCode::Char(' '));
        assert!(!h.app.cfg.pr.draft);
        goto(&mut h, "max_agents");
        press(&mut h, KeyCode::Char(' '));
        assert!(view(&h).flash.as_deref().unwrap().contains("Space toggles"));
    }

    #[test]
    fn ctrl_g_writes_the_global_file_and_warns_when_repo_shadows_it() {
        let (mut h, global) = with_global("cfg-global");
        write_repo_config(&h, "max_agents = 4\n");
        h.app.command("reload");
        h.app.command("config");
        goto(&mut h, "max_agents");
        press(&mut h, KeyCode::Enter);
        assert!(view(&h).shadowed(editor(&h)).is_none());
        ctrl(&mut h, 'g');
        assert_eq!(editor(&h).target, Source::Global);
        assert_eq!(
            view(&h).shadowed(editor(&h)).as_deref(),
            Some("repo overrides this")
        );
        ctrl(&mut h, 'u');
        typed(&mut h, "9");
        press(&mut h, KeyCode::Enter);
        assert_eq!(read(&global), "max_agents = 9\n");
        assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 4\n");
        assert_eq!(h.app.cfg.max_agents, 4);

        goto(&mut h, "incus.browse.url");
        press(&mut h, KeyCode::Enter);
        ctrl(&mut h, 'g');
        typed(&mut h, "http://app.test:8000");
        press(&mut h, KeyCode::Enter);
        assert_eq!(
            read(&global),
            "max_agents = 9\n\n[incus.browse]\nurl = \"http://app.test:8000\"\n"
        );
        assert_eq!(
            h.app.layers.config().unwrap().incus.browse.url.as_deref(),
            Some("http://app.test:8000")
        );
    }

    #[test]
    fn x_removes_the_key_after_confirming() {
        let (mut h, global) = with_global("cfg-remove");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(&global, "max_agents = 5\n").unwrap();
        write_repo_config(&h, "max_agents = 4\nnope = 1\n");
        h.app.command("reload");
        h.app.command("config");
        goto(&mut h, "max_agents");
        press(&mut h, KeyCode::Char('x'));
        match &view(&h).prompt {
            Some(Prompt::Remove {
                source, fallback, ..
            }) => {
                assert_eq!(*source, Source::Repo);
                assert_eq!(*fallback, Some(toml::Value::Integer(5)));
            }
            p => panic!("{p:?}"),
        }
        press(&mut h, KeyCode::Char('n'));
        assert!(view(&h).prompt.is_none());
        assert_eq!(view(&h).flash.as_deref(), Some("cancelled"));
        press(&mut h, KeyCode::Char('x'));
        press(&mut h, KeyCode::Char('y'));
        assert_eq!(read(&Config::repo_path(&h.dir)), "nope = 1\n");
        assert_eq!(h.app.cfg.max_agents, 5);
        assert!(h.app.status.starts_with("removed max_agents from repo · "));

        goto(&mut h, "nope");
        press(&mut h, KeyCode::Enter);
        assert!(view(&h).flash.as_deref().unwrap().contains("press x"));
        press(&mut h, KeyCode::Char('x'));
        press(&mut h, KeyCode::Char('y'));
        assert_eq!(read(&Config::repo_path(&h.dir)), "");

        goto(&mut h, "poll_secs");
        press(&mut h, KeyCode::Char('x'));
        assert!(view(&h).prompt.is_none());
        assert_eq!(
            view(&h).flash.as_deref(),
            Some("poll_secs is not set in a config file")
        );
    }

    #[test]
    fn arrays_and_multi_line_values_point_at_the_editor() {
        let mut h = Harness::new("cfg-arrays", &[]);
        write_repo_config(&h, "system_prompt = \"\"\"\none\ntwo\n\"\"\"\n");
        h.app.command("reload");
        h.app.command("config");
        goto(&mut h, "incus.mounts");
        press(&mut h, KeyCode::Enter);
        assert!(view(&h).prompt.is_none());
        assert_eq!(
            view(&h).flash.as_deref(),
            Some("incus.mounts can't be edited inline, press e to edit in $EDITOR")
        );
        goto(&mut h, "system_prompt");
        press(&mut h, KeyCode::Enter);
        assert!(view(&h).prompt.is_none());
        assert!(view(&h).flash.as_deref().unwrap().contains("press e"));
    }

    fn finish_editor(h: &mut Harness) {
        h.app
            .suspend_finished("editor", Ok(std::process::ExitStatus::from_raw(0)));
    }

    #[test]
    fn e_round_trips_through_the_editor_and_returns_to_the_popup() {
        let mut h = Harness::new("cfg-editor", &[]);
        h.app.command("config");
        goto(&mut h, "max_agents");
        let cursor = view(&h).current();
        press(&mut h, KeyCode::Char('e'));
        let repo = Config::repo_path(&h.dir);
        match &h.app.suspend {
            Some(Suspend::File(p)) => assert_eq!(*p, repo),
            _ => panic!("no editor requested"),
        }
        assert_eq!(read(&repo), "");
        assert!(matches!(h.app.mode, Mode::Config(_)));
        h.app.suspend = None;
        std::fs::write(&repo, "max_agents = 7\n").unwrap();
        finish_editor(&mut h);
        assert_eq!(h.app.cfg.max_agents, 7);
        assert_eq!(h.app.status, "config reloaded: 1 changed (max_agents 2→7)");
        assert_eq!(view(&h).current(), cursor);

        press(&mut h, KeyCode::Char('e'));
        h.app.suspend = None;
        std::fs::write(&repo, "max_agents = 8\n[claude\n").unwrap();
        finish_editor(&mut h);
        assert_eq!(h.app.cfg.max_agents, 7);
        match &view(&h).prompt {
            Some(Prompt::Invalid { source, error }) => {
                assert_eq!(*source, Source::Repo);
                assert!(error.contains("line 2"), "{error}");
            }
            p => panic!("{p:?}"),
        }
        press(&mut h, KeyCode::Char('j'));
        assert!(matches!(view(&h).prompt, Some(Prompt::Invalid { .. })));
        press(&mut h, KeyCode::Char('e'));
        assert!(matches!(h.app.suspend, Some(Suspend::File(_))));
        h.app.suspend = None;
        finish_editor(&mut h);
        assert!(matches!(view(&h).prompt, Some(Prompt::Invalid { .. })));
        press(&mut h, KeyCode::Esc);
        assert!(view(&h).prompt.is_none());
        assert_eq!(read(&repo), "max_agents = 8\n[claude\n");
        assert_eq!(h.app.cfg.max_agents, 7);
        assert!(h.app.reload.error.is_some());

        std::fs::write(&repo, "max_agents = 8\n").unwrap();
        press(&mut h, KeyCode::Char('e'));
        h.app.suspend = None;
        finish_editor(&mut h);
        assert_eq!(h.app.cfg.max_agents, 8);
        assert!(h.app.reload.error.is_none());
        assert!(view(&h).prompt.is_none());
    }

    #[test]
    fn capital_e_and_config_edit_open_the_global_file() {
        let (mut h, global) = with_global("cfg-editor-global");
        h.app.command("config edit global");
        match &h.app.suspend {
            Some(Suspend::File(p)) => assert_eq!(*p, global),
            _ => panic!("no editor requested"),
        }
        assert!(global.exists());
        h.app.suspend = None;
        std::fs::write(&global, "poll_secs = 9\n").unwrap();
        finish_editor(&mut h);
        assert_eq!(h.app.cfg.poll_secs, 9);
        assert!(matches!(h.app.mode, Mode::Config(_)));
        press(&mut h, KeyCode::Char('q'));
        h.app.command("config edit");
        match &h.app.suspend {
            Some(Suspend::File(p)) => assert_eq!(*p, Config::repo_path(&h.dir)),
            _ => panic!("no editor requested"),
        }
        h.app.suspend = None;
        press(&mut h, KeyCode::Char('E'));
        assert!(matches!(&h.app.suspend, Some(Suspend::File(p)) if *p == global));
        h.app.command("config bogus");
        assert!(h.app.status.contains("unknown :config argument"));

        let mut h = Harness::new("cfg-no-global", &[]);
        h.app.command("config");
        press(&mut h, KeyCode::Char('E'));
        assert!(h.app.suspend.is_none());
        assert_eq!(
            view(&h).flash.as_deref(),
            Some("no global config directory")
        );
        goto(&mut h, "max_agents");
        press(&mut h, KeyCode::Enter);
        ctrl(&mut h, 'g');
        assert_eq!(editor(&h).target, Source::Repo);
    }

    #[test]
    fn paste_goes_into_the_inline_editor() {
        let mut h = Harness::new("cfg-paste", &[]);
        h.app.command("config");
        goto(&mut h, "claude.bin");
        press(&mut h, KeyCode::Enter);
        ctrl(&mut h, 'u');
        h.app.on_paste("/opt/x\n");
        assert_eq!(editor(&h).buf, "/opt/x ");
        press(&mut h, KeyCode::Backspace);
        press(&mut h, KeyCode::Home);
        press(&mut h, KeyCode::Delete);
        press(&mut h, KeyCode::Char('/'));
        press(&mut h, KeyCode::End);
        assert_eq!(editor(&h).buf, "/opt/x");
        assert_eq!(editor(&h).at, 6);
    }
}
fn idle_agent(h: &mut Harness, id: &str) {
    let mut cmd = CommandBuilder::new("sleep");
    cmd.arg("30");
    let a = Agent::spawn(cmd, id, 24, 80, None, h.app.tx.clone()).unwrap();
    h.app.agents.insert(id.to_string(), a);
}

fn fired(h: &Harness) -> Vec<(Trigger, String)> {
    h.app.notifier.fired.clone()
}

#[test]
fn reviewer_and_chat_bells_do_not_notify() {
    let mut h = Harness::new("bell-quiet", &[("a", Some(State::Reviewing))]);
    let rid = agent::review_id("a");
    idle_agent(&mut h, &rid);
    h.app.on_bell(&rid);
    h.app.on_bell(CHAT_ID);
    h.app.confirm_waits(Instant::now() + WAIT_CONFIRM * 2);
    assert!(fired(&h).is_empty());
    h.app.cfg.notify.on.push(Trigger::Chat);
    h.app.notifier.reconfigure(h.app.cfg.notify.clone());
    h.app.on_bell(CHAT_ID);
    assert_eq!(fired(&h), [(Trigger::Chat, CHAT_ID.to_string())]);
}

#[test]
fn bell_then_review_notifies_review_once() {
    let mut h = Harness::new("bell-review", &[("a", Some(State::Running))]);
    idle_agent(&mut h, "a");
    let t0 = Instant::now();
    h.app.on_bell("a");
    h.app.confirm_waits(t0 + Duration::from_secs(5));
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Review)));
    h.app.ready_for_review("a", true);
    h.app.ready_for_review("a", false);
    h.app.confirm_waits(t0 + WAIT_CONFIRM * 2);
    assert_eq!(fired(&h), [(Trigger::Review, "a".to_string())]);
    assert!(h.app.waits.is_empty());
    h.app.notifier.clear("a");
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Running)));
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Review)));
    h.app.ready_for_review("a", false);
    assert_eq!(fired(&h).len(), 2);
}

#[test]
fn review_does_not_notify_while_an_ai_review_starts() {
    let mut h = Harness::new("review-auto", &[("a", Some(State::Review))]);
    h.app.git.ensure_worktree("a", "main").unwrap();
    h.app.cfg.ai_review.auto = true;
    h.app.ready_for_review("a", true);
    assert!(h.app.is_starting(&agent::review_id("a")));
    h.app.notify_review("a");
    h.pump();
    assert_eq!(h.state("a"), Some(State::Reviewing));
    h.app.notify_review("a");
    assert!(fired(&h).is_empty());
}

#[test]
fn bell_with_an_idle_agent_notifies_waiting_after_confirmation() {
    let mut h = Harness::new(
        "bell-waiting",
        &[("a", Some(State::Running)), ("b", Some(State::Running))],
    );
    idle_agent(&mut h, "a");
    idle_agent(&mut h, "b");
    let t0 = Instant::now();
    h.app.on_bell("a");
    h.app.on_bell("b");
    h.app.confirm_waits(t0 + WAIT_CONFIRM / 2);
    assert!(fired(&h).is_empty());
    h.app.agents.get_mut("b").unwrap().kill();
    h.app.confirm_waits(t0 + WAIT_CONFIRM * 2);
    assert_eq!(fired(&h), [(Trigger::Waiting, "a".to_string())]);
    assert!(h.app.waits.is_empty());
}

#[test]
fn dead_agent_notifies_dead() {
    let mut h = Harness::new("dead", &[("a", Some(State::Running))]);
    h.app.failed.insert("a".into());
    h.app.check_stalls();
    assert_eq!(h.app.stalls.get("a"), Some(&Stall::Dead));
    assert_eq!(fired(&h), [(Trigger::Dead, "a".to_string())]);
}

fn pr_comment(id: &str, author: &str) -> crate::pr::PrComment {
    crate::pr::PrComment {
        id: id.into(),
        author: author.into(),
        body: "rename this".into(),
        path: None,
        line: None,
        created_at: None,
        approval: false,
        bot: author.ends_with("[bot]"),
    }
}

#[test]
fn pr_comments_from_bots_or_the_user_do_not_notify() {
    let mut h = Harness::new("pr-bots", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let mut s = pr_status(crate::pr::PrState::Open, &[]);
    s.viewer = Some("me".into());
    s.comments = vec![pr_comment("c1", "me"), pr_comment("c2", "ci[bot]")];
    h.app.on_pr("a", 7, Ok(s.clone()));
    assert_eq!(h.app.status, "PR #7 for a: 2 new comments, U to revise");
    assert_eq!(h.app.pr_watch.beads["a"].comments.len(), 2);
    assert!(fired(&h).is_empty());
    s.comments.push(pr_comment("c3", "alice"));
    h.app.on_pr("a", 7, Ok(s));
    assert_eq!(fired(&h), [(Trigger::Pr, "a".to_string())]);
}

#[test]
fn failing_checks_notify_once_per_head() {
    let mut h = Harness::new("pr-checks", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let poll = |h: &mut Harness, head: &str, checks: crate::pr::Checks| {
        let mut s = pr_status(crate::pr::PrState::Open, &[]);
        s.head = Some(head.into());
        s.checks = checks;
        h.app.on_pr("a", 7, Ok(s));
        h.app.notifier.clear("a");
        fired(h).len()
    };
    let failing =
        |names: &[&str]| crate::pr::Checks::Failing(names.iter().map(|n| n.to_string()).collect());
    assert_eq!(poll(&mut h, "h1", failing(&["test"])), 1);
    assert_eq!(poll(&mut h, "h1", failing(&["lint", "test"])), 1);
    assert_eq!(poll(&mut h, "h1", crate::pr::Checks::Pending), 1);
    assert_eq!(poll(&mut h, "h1", failing(&["test"])), 1);
    assert_eq!(poll(&mut h, "h2", crate::pr::Checks::Pending), 1);
    assert_eq!(poll(&mut h, "h2", failing(&["test"])), 2);
    assert_eq!(poll(&mut h, "h2", crate::pr::Checks::Passing), 2);
}

#[test]
fn pr_approved_or_closed_notifies() {
    let mut h = Harness::new("pr-approved", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let mut s = pr_status(crate::pr::PrState::Open, &[]);
    let mut approve = pr_comment("r1", "bob");
    approve.approval = true;
    s.comments.push(approve);
    h.app.on_pr("a", 7, Ok(s.clone()));
    assert_eq!(
        h.app.status,
        "PR #7 for a: 1 new comment, approved, ready to merge"
    );
    assert_eq!(fired(&h).len(), 1);
    h.app.notifier.clear("a");
    s.state = crate::pr::PrState::Closed;
    h.app.on_pr("a", 7, Ok(s));
    assert_eq!(h.app.status, "PR #7 for a: closed without merging");
    assert_eq!(fired(&h).len(), 2);
}
