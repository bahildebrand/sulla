use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::edit::{self, Kind};
use crate::config::effective::Source;
use crate::config::reload;
use crate::config::show::dotted;
use crate::ui::config::{ConfigView, Editor, Prompt, inline};

use super::{App, Mode, Suspend};

impl App {
    pub(super) fn edit_config_file(&mut self, source: Source) -> Result<(), String> {
        let path = edit::path(&self.layers, source).map_err(|e| edit::message(&e))?;
        edit::ensure(&path).map_err(|e| format!("{e:#}"))?;
        self.config_edit = Some(source);
        self.suspend = Some(Suspend::File(path));
        Ok(())
    }

    pub(super) fn config_command(&mut self, rest: &str) {
        let source = match rest {
            "" => return self.open_config(),
            "edit" | "edit repo" => Source::Repo,
            "edit global" => Source::Global,
            _ => {
                self.status =
                    format!("unknown :config argument: {rest} (try :config edit [global])");
                return;
            }
        };
        self.open_config();
        if let Err(e) = self.edit_config_file(source) {
            self.status = e;
        }
    }

    pub(super) fn config_edited(&mut self) {
        let Some(source) = self.config_edit.take() else {
            return;
        };
        if !matches!(self.mode, Mode::Config(_)) {
            self.open_config();
        }
        match self.layers.reload() {
            Ok(_) => self.reload_config(),
            Err(e) => {
                self.config_watch.sync();
                let error = reload::describe(&e);
                self.status = format!("{}{error}", reload::FAILED);
                if let Mode::Config(v) = &mut self.mode {
                    v.prompt = Some(Prompt::Invalid { source, error });
                }
            }
        }
    }

    fn config_saved(&mut self, v: &mut ConfigView, key: &[String], what: String) {
        v.prompt = None;
        self.reload_config_forcing(Some(key));
        if self.reload.error.is_none() {
            self.status = format!("{what} · {}", self.status);
        }
        self.refresh_config_view(v);
    }

    fn save_config_value(&mut self, v: &mut ConfigView, ed: &Editor) -> Result<(), String> {
        let value = edit::set(&self.layers, ed.target, &ed.path, ed.kind, &ed.buf)
            .map_err(|e| edit::message(&e))?;
        let what = format!(
            "set {} = {} in {}",
            dotted(&ed.path),
            inline(&value),
            ed.target.name()
        );
        self.config_saved(v, &ed.path, what);
        Ok(())
    }

    pub(super) fn config_toggle(&mut self, v: &mut ConfigView) {
        let Some((path, value)) = v.toggled() else {
            v.flash = Some("Space toggles true/false keys, Enter edits others".into());
            return;
        };
        let ed = Editor {
            path,
            kind: Kind::Bool,
            buf: value.to_string(),
            at: 0,
            target: Source::Repo,
            error: None,
        };
        if let Err(e) = self.save_config_value(v, &ed) {
            v.flash = Some(e);
        }
    }

    pub(super) fn config_prompt_key(&mut self, k: KeyEvent, v: &mut ConfigView, prompt: Prompt) {
        match prompt {
            Prompt::Edit(ed) => self.config_editor_key(k, v, ed),
            Prompt::Remove { path, source, .. } => {
                if !matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    v.flash = Some("cancelled".into());
                    return;
                }
                match edit::remove(&self.layers, source, &path) {
                    Ok(()) => {
                        let what = format!("removed {} from {}", dotted(&path), source.name());
                        self.config_saved(v, &path, what);
                    }
                    Err(e) => v.flash = Some(edit::message(&e)),
                }
            }
            Prompt::Invalid { source, error } => match k.code {
                KeyCode::Char('e') => {
                    if let Err(e) = self.edit_config_file(source) {
                        v.flash = Some(e);
                    }
                }
                KeyCode::Esc => {
                    self.reload_config();
                    self.refresh_config_view(v);
                }
                _ => v.prompt = Some(Prompt::Invalid { source, error }),
            },
        }
    }

    fn config_editor_key(&mut self, k: KeyEvent, v: &mut ConfigView, mut ed: Editor) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return,
            KeyCode::Enter => match self.save_config_value(v, &ed) {
                Ok(()) => return,
                Err(e) => ed.error = Some(e),
            },
            KeyCode::Char('g') if ctrl => {
                if !ed.switch(self.layers.global.is_some()) {
                    v.flash = Some("no global config directory".into());
                }
            }
            KeyCode::Tab => {
                ed.cycle(true);
            }
            KeyCode::BackTab => {
                ed.cycle(false);
            }
            KeyCode::Char(' ') if ed.kind == Kind::Bool => {
                ed.cycle(true);
            }
            KeyCode::Char('u') if ctrl => ed.clear(),
            KeyCode::Char('a') if ctrl => ed.home(),
            KeyCode::Char('e') if ctrl => ed.end(),
            KeyCode::Home => ed.home(),
            KeyCode::End => ed.end(),
            KeyCode::Left => ed.left(),
            KeyCode::Right => ed.right(),
            KeyCode::Backspace => ed.backspace(),
            KeyCode::Delete => ed.delete(),
            KeyCode::Char(c) if !ctrl => ed.insert(c.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        v.prompt = Some(Prompt::Edit(ed));
    }
}
