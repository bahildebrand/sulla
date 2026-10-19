use std::collections::HashSet;
use std::path::PathBuf;

use chrono::Utc;
use color_eyre::Result;

use crate::agent;
use crate::beads::Beads;
use crate::config::{AgentKind, PrTool, SandboxKind, Workflow};
use crate::graphite;
use crate::health::{self, Probe};
use crate::pr;
use crate::timeline;

use super::{App, Event, Mode, View};

impl App {
    pub(super) fn open_health(&mut self) {
        let used: HashSet<AgentKind> = std::iter::once(self.cfg.agent)
            .chain(
                self.all
                    .iter()
                    .filter(|b| b.status != "closed")
                    .map(|b| agent::selection(b, self.cfg.agent).0),
            )
            .collect();
        let mut probes = vec![
            Probe::new("bd", "bd"),
            Probe::new("dolt", "dolt")
                .version(&["version"])
                .off((!self.beads.server_mode()).then_some("not used (embedded database)")),
        ];
        for kind in AgentKind::ALL.into_iter().filter(|k| used.contains(k)) {
            let bin = match kind {
                AgentKind::Copilot => &self.cfg.copilot.bin,
                AgentKind::Claude => &self.cfg.claude.bin,
            };
            probes.push(Probe::new(self.backend(kind).name(), bin));
        }
        probes.extend([
            Probe::new("kache", "kache")
                .off((!self.cfg.kache).then_some("disabled (kache = false)")),
            Probe::new("incus", &self.cfg.incus.bin)
                .off(
                    (self.cfg.sandbox != SandboxKind::Incus)
                        .then_some("not used (sandbox = worktree)"),
                )
                .also(&["query", "/1.0"]),
        ]);
        if self.cfg.workflow == Workflow::Pr {
            probes.push(pr::probe());
            if self.cfg.pr.tool == PrTool::Graphite {
                probes.push(graphite::probe());
            }
        }
        health::spawn(probes, self.beads.clone(), self.tx.clone());
        self.mode = Mode::Health;
    }

    pub(super) fn agent_log(&self, id: &str) -> PathBuf {
        agent::log_path(&self.repo.join(".sulla").join("logs"), id)
    }

    pub(super) fn show_log(&mut self, id: String) {
        let (r, c) = self.pty_size();
        let render = |path: PathBuf| {
            std::fs::read(path)
                .ok()
                .map(|b| agent::render_log(&b, r, c))
        };
        let main = render(self.agent_log(&id));
        let review = render(self.agent_log(&agent::review_id(&id)));
        let text = match (main, review) {
            (None, None) => {
                self.status = format!("no session log for {id}");
                return;
            }
            (Some(m), None) => m,
            (None, Some(v)) => format!("--- ai review ---\n{v}"),
            (Some(m), Some(v)) => format!("{m}\n\n--- ai review ---\n{v}"),
        };
        let scroll = text.lines().count().saturating_sub(1) as u16;
        self.mode = Mode::Inspect { id, text, scroll };
    }

    pub(super) fn open_progress(&mut self, id: String) {
        if let Some(a) = self.agents.get_mut(&id) {
            a.scroll_reset();
        }
        self.view = View::Progress(id);
    }

    pub(super) fn swap_to_agent(&mut self, insert: bool) {
        let view = std::mem::replace(&mut self.view, View::Board);
        let View::Review(d) = view else {
            self.view = view;
            return;
        };
        let id = d.id.clone();
        self.parked_review = Some(d);
        self.open_progress(id.clone());
        if insert {
            self.enter_insert(&id);
        }
    }

    pub(super) fn back_to_review(&mut self, id: String) -> bool {
        match self.parked_review.take() {
            Some(d) if d.id == id => {
                self.view = View::Review(d);
                self.status = format!("back to review of {id} (R reloads diff)");
                true
            }
            _ => false,
        }
    }

    pub fn suspend_finished(&mut self, name: &str, r: std::io::Result<std::process::ExitStatus>) {
        match r {
            Ok(s) if s.success() => {}
            Ok(s) => self.status = format!("{name} exited with {s}"),
            Err(e) => self.status = format!("{name} failed: {e}"),
        }
        let View::Review(d) = &mut self.view else {
            return;
        };
        let (id, sel, comments) = (d.id.clone(), d.sel, std::mem::take(&mut d.comments));
        self.reload_review(id);
        if let View::Review(d) = &mut self.view {
            d.comments = comments;
            d.sel = sel.min(d.files.len().saturating_sub(1));
        }
    }

    pub(super) fn inspect(&mut self, id: String) {
        let mut sections = Vec::new();
        if let Some(u) = self.usage.get(&id) {
            sections.push(("USAGE", u.detail()));
        }
        if let Some(b) = self.budget_status(&id) {
            sections.push(("BUDGET", b));
        }
        if let Some(m) = self.over_budget.get(&id) {
            sections.push(("HELD", m.clone()));
        }
        self.load_inspect(id, move |beads, id| {
            let mut text = beads.show_text(id)?;
            for (heading, body) in sections {
                text = format!("{}\n\n{heading}\n  {body}\n", text.trim_end());
            }
            Ok(text)
        });
    }

    pub(super) fn show_timeline(&mut self, id: String) {
        let cached = self.bead(&id).cloned();
        self.load_inspect(id, move |beads, id| {
            let bead = cached.map_or_else(|| beads.show(id), Ok)?;
            let events = beads.events(id)?;
            let comments = beads.comments(id)?;
            let t = timeline::build(&bead, &events, &comments, Utc::now());
            Ok(timeline::render(&bead, &t))
        });
    }

    fn load_inspect<F>(&mut self, id: String, load: F)
    where
        F: FnOnce(&Beads, &str) -> Result<String> + Send + 'static,
    {
        self.status = format!("loading {id}...");
        let beads = self.beads.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = load(&beads, &id);
            let _ = tx.send(Event::Inspected(id, r));
        });
    }

    pub(super) fn on_inspected(&mut self, id: String, r: Result<String>) {
        let waiting = matches!(self.view, View::Board)
            && match &self.mode {
                Mode::Normal => self.selected().is_some_and(|b| b.id == id),
                Mode::Inspect { id: open, .. } => *open == id,
                _ => false,
            };
        if !waiting {
            return;
        }
        if self.status == format!("loading {id}...") {
            self.status.clear();
        }
        if let Some(text) = self.report(r) {
            self.mode = Mode::Inspect {
                id,
                text,
                scroll: 0,
            };
        }
    }
}
