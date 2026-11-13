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

mod board;
mod browsing;
mod closed;
mod config;
mod config_editing;
mod evict;
mod launch;
mod merge;
mod notify;
mod publish;
mod release_reclaim;
mod review;

type Finished = Arc<Mutex<Vec<(usize, String, Outcome)>>>;
type Sessions = Arc<Mutex<Vec<(String, String)>>>;

struct Gated {
    gate: Arc<Mutex<()>>,
    prepared: Arc<Mutex<Vec<String>>>,
    finished: Finished,
    sessions: Sessions,
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
        launch: &Launch,
        cmd: CommandBuilder,
    ) -> Result<CommandBuilder> {
        self.sessions
            .lock()
            .unwrap()
            .push((launch.id.clone(), launch.session.clone()));
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
    sessions: Sessions,
    skipped: Vec<Event>,
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
        let sessions = Sessions::default();
        app.sandbox = Box::new(Gated {
            gate: gate.clone(),
            prepared: prepared.clone(),
            finished: finished.clone(),
            sessions: sessions.clone(),
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
            sessions,
            skipped: Vec::new(),
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
            } else {
                self.skipped.push(ev);
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

    fn sessions(&self, id: &str) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(i, _)| i == id)
            .map(|(_, s)| s.clone())
            .collect()
    }

    fn dispatch_exit(&mut self, id: &str) {
        let exited = |ev: &Event| matches!(ev, Event::AgentExited(i, _) if i == id);
        if let Some(at) = self.skipped.iter().position(exited) {
            let ev = self.skipped.remove(at);
            self.app.dispatch(ev);
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            let ev = self.rx.recv_timeout(wait).expect("agent did not exit");
            if exited(&ev) {
                self.app.dispatch(ev);
                return;
            }
            if matches!(ev, Event::Finished(_)) {
                self.app.dispatch(ev);
            }
        }
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

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
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

fn write_repo_config(h: &Harness, body: &str) {
    let path = Config::repo_path(&h.dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}
