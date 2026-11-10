use std::time::Instant;

use color_eyre::Result;

use crate::config::reload::{self, FAILED};
use crate::ui::config::ConfigView;

use super::{App, Mode, backends};

impl App {
    pub(super) fn watch_config(&mut self) {
        if self.config_watch.poll(Instant::now()) {
            self.reload_config();
        }
    }

    pub(super) fn reload_config(&mut self) {
        self.config_watch.sync();
        match self.apply_reload() {
            Ok(summary) => {
                tracing::info!("{summary}");
                self.reload.error = None;
                self.status = summary;
            }
            Err(e) => {
                tracing::warn!("config reload: {e:#}");
                let msg = reload::describe(&e);
                self.status = format!("{FAILED}{msg}");
                self.reload.error = Some(msg);
            }
        }
        let mut mode = std::mem::replace(&mut self.mode, Mode::Normal);
        if let Mode::Config(v) = &mut mode {
            self.refresh_config_view(v);
        }
        self.mode = mode;
    }

    pub(super) fn refresh_config_view(&self, v: &mut ConfigView) {
        match ConfigView::new(&self.layers, &self.cfg, &self.reload) {
            Ok(mut fresh) => {
                fresh.keep_ui(v);
                *v = fresh;
                v.flash = Some(self.status.clone());
            }
            Err(e) => v.flash = Some(format!("config: {e:#}")),
        }
    }

    fn apply_reload(&mut self) -> Result<String> {
        let layers = self.layers.reload()?;
        let new = layers.config()?;
        let old = self.layers.config()?;
        let plan = reload::plan(&self.cfg, &old, &new, &self.reload.restart)?;
        let summary = plan.summary();
        let (backends_changed, notify_changed) = (
            plan.touched("copilot") || plan.touched("claude"),
            plan.touched("notify"),
        );
        self.layers = layers;
        self.cfg = plan.cfg;
        self.reload.restart = plan.restart;
        self.reload.changed = plan.changed.iter().map(|c| c.path.clone()).collect();
        self.reload.at = Some(Instant::now());
        if backends_changed {
            self.backends = backends(&self.cfg);
        }
        if notify_changed {
            self.notifier.reconfigure(self.cfg.notify.clone());
        }
        self.poll_every.set(self.cfg.poll_secs);
        self.pr_every.set(self.cfg.pr.poll_secs);
        if !plan.changed.is_empty() {
            self.schedule();
        }
        Ok(summary)
    }
}
