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
    };
    let status = crate::pr::PrStatus {
        state: crate::pr::PrState::Open,
        comments: vec![comment],
        checks: crate::pr::Checks::None,
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
            })
            .collect(),
        checks: crate::pr::Checks::None,
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
