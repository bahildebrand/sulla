use std::path::PathBuf;

use color_eyre::{Result, eyre::eyre};

use crate::beads::State;
use crate::checks::{self, Check};
use crate::sandbox;

use super::{App, Mode};

impl App {
    fn check_cmd(&self) -> Option<String> {
        self.cfg.check.clone().filter(|c| !c.trim().is_empty())
    }

    fn in_review(&self, id: &str) -> bool {
        self.bead(id).is_some_and(|b| {
            b.status != "closed" && matches!(b.state(), Some(State::Review | State::Reviewing))
        })
    }

    fn check_key(&self, id: &str) -> Result<checks::Key> {
        Ok(checks::Key {
            head: self.git.head(id)?,
            dirty: self.git.dirty(id)?,
        })
    }

    fn check_log(&self, id: &str) -> PathBuf {
        checks::log_path(&self.repo.join(".sulla").join("checks"), id)
    }

    pub(super) fn sync_checks(&mut self) {
        let stale: Vec<String> = self
            .checks
            .keys()
            .filter(|id| self.check_cmd().is_none() || !self.in_review(id))
            .cloned()
            .collect();
        for id in stale {
            self.stop_check(&id);
        }
        if self.check_cmd().is_none() {
            return;
        }
        let ids: Vec<String> = self
            .all
            .iter()
            .filter(|b| self.in_review(&b.id))
            .map(|b| b.id.clone())
            .collect();
        for id in ids {
            if !self.git.worktree_path(&id).join(".git").exists() {
                continue;
            }
            let Ok(key) = self.check_key(&id) else {
                continue;
            };
            if self.checks.get(&id).is_some_and(|c| c.key == key) {
                continue;
            }
            self.stop_check(&id);
            if let Some(c) = Check::restore(self.check_log(&id), &key) {
                self.checks.insert(id, c);
            } else if !self.sandbox.isolated() || self.slots.get(&id).is_some() {
                self.enqueue_check(&id, key, false);
            }
        }
        self.pump_checks();
    }

    fn enqueue_check(&mut self, id: &str, key: checks::Key, host: bool) {
        self.stop_check(id);
        self.check_runs += 1;
        let c = Check::queued(key, self.check_log(id), self.check_runs, host);
        self.checks.insert(id.to_string(), c);
    }

    fn pump_checks(&mut self) {
        if self
            .checks
            .values()
            .any(|c| c.status == checks::Status::Running)
        {
            return;
        }
        let Some(id) = self
            .checks
            .iter()
            .filter(|(_, c)| c.status == checks::Status::Queued)
            .min_by_key(|(_, c)| c.queued)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let Some(cmd) = self.check_cmd() else {
            return;
        };
        let dir = self.git.worktree_path(&id);
        let mut env = vec![("SULLA_BEAD".to_string(), id.clone())];
        if self.cfg.kache {
            env.push(("RUSTC_WRAPPER".into(), "kache".into()));
        }
        let argv: Vec<std::ffi::OsString> = ["sh", "-c", &cmd].map(Into::into).into();
        let host = self.checks[&id].host;
        let command = if !self.sandbox.isolated() || host {
            Some(sandbox::host_command(&dir, &argv, &env))
        } else {
            self.slots
                .get(&id)
                .map(|slot| self.sandbox.command(slot, &dir, &argv, &env))
        };
        let tx = self.tx.clone();
        let c = self.checks.get_mut(&id).unwrap();
        let r = match command {
            Some(command) => c.launch(&id, command, tx),
            None => Err(eyre!(
                "no sandbox container for {id}, T runs checks on the host"
            )),
        };
        if let Err(e) = r {
            tracing::warn!("check for {id} did not start: {e:#}");
            c.fail(&e.to_string());
            self.pump_checks();
        }
    }

    pub(super) fn check_done(&mut self, d: checks::Done) {
        let key = self.check_key(&d.id).ok();
        if let Some(c) = self.checks.get_mut(&d.id)
            && c.run == d.run
            && c.status == checks::Status::Running
        {
            c.finish(d.ok, key);
            self.status = format!("{}: {}", d.id, c.summary());
        }
        self.pump_checks();
    }

    pub(super) fn stop_check(&mut self, id: &str) {
        if let Some(mut c) = self.checks.remove(id) {
            c.stop();
        }
    }

    pub(super) fn rerun_check(&mut self, id: &str) {
        if self.check_cmd().is_none() {
            self.status = "no check command configured".into();
            return;
        }
        if self.refuse_released(id) {
            return;
        }
        if !self.in_review(id) {
            self.status = format!("{id} is not in review");
            return;
        }
        let r = self.check_key(id);
        let Some(key) = self.report(r) else {
            return;
        };
        let host = self.sandbox.isolated() && self.slots.get(id).is_none();
        self.enqueue_check(id, key, host);
        self.pump_checks();
        self.status = format!(
            "checks for {id} queued{}",
            if host {
                " on the host, no container"
            } else {
                ""
            }
        );
    }

    pub(super) fn check_blocker(&self, id: &str) -> Option<String> {
        self.check_cmd()?;
        if !self.git.worktree_path(id).join(".git").exists() {
            return None;
        }
        let key = match self.check_key(id) {
            Ok(k) => k,
            Err(_) => return Some("worktree state is unreadable".into()),
        };
        if !key.dirty.is_empty() {
            return Some("the worktree has uncommitted changes".into());
        }
        let Some(c) = self.checks.get(id) else {
            return Some("checks have not run (T runs them)".into());
        };
        match c.status {
            checks::Status::Queued => Some("checks are queued".into()),
            checks::Status::Running => Some("checks are still running".into()),
            _ if c.key != key => Some("checks are stale".into()),
            checks::Status::Fail => Some("checks failed".into()),
            _ => None,
        }
    }

    pub(super) fn show_check(&mut self, id: String) {
        let (Some(cmd), Some(c)) = (self.check_cmd(), self.checks.get(&id)) else {
            self.status = format!("no check results for {id}");
            return;
        };
        let text = format!("$ {cmd}\n{}\n\n{}", c.summary(), c.output());
        let scroll = text.lines().count().saturating_sub(1) as u16;
        self.mode = Mode::Inspect { id, text, scroll };
    }
}
