mod claude;
mod copilot;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use color_eyre::{Result, eyre::eyre};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub use claude::Claude;
pub use copilot::Copilot;

use crate::app::Event;
use crate::beads::Bead;

pub struct Launch {
    pub bead: Bead,
    pub worktree: PathBuf,
    pub resume: bool,
    pub prompt: Option<String>,
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    fn command(&self, launch: &Launch) -> CommandBuilder;
}

pub fn task_prompt(bead: &Bead) -> String {
    let id = &bead.id;
    let desc = if bead.description.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{}", bead.description.trim())
    };
    format!(
        "You are working on bead {id}: {title}{desc}\n\n\
         You are in a dedicated git worktree on branch sulla/{id}. Commit your work to this \
         branch. Do not switch branches, rebase, or touch other worktrees. Rust builds are cached \
         with kache through RUSTC_WRAPPER, leave that alone.\n\n\
         When the task is complete and committed, run \
         `bd set-state {id} sulla=review --reason \"ready for review\"` then stop and wait. \
         A human will review the diff and may send feedback.",
        title = bead.title,
    )
}

pub fn session_id(repo: &std::path::Path, bead_id: &str) -> String {
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        bytes
            .iter()
            .fold(seed, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
    }
    let key = format!("{}\0{bead_id}", repo.display());
    let a = fnv(0xcbf29ce484222325, key.as_bytes());
    let b = fnv(a ^ 0x9e3779b97f4a7c15, key.as_bytes());
    let a = (a & 0xffff_ffff_ffff_0fff) | 0x4000;
    let b = (b & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0xffff,
        b >> 48,
        b & 0xffff_ffff_ffff
    )
}

pub struct Agent {
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub alive: bool,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Agent {
    pub fn spawn(
        backend: &dyn Backend,
        launch: &Launch,
        kache: bool,
        rows: u16,
        cols: u16,
        tx: Sender<Event>,
    ) -> Result<Self> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size).map_err(|e| eyre!(e))?;
        let mut cmd = backend.command(launch);
        cmd.cwd(&launch.worktree);
        cmd.env("TERM", "xterm-256color");
        cmd.env("SULLA_BEAD", &launch.bead.id);
        if kache {
            cmd.env("RUSTC_WRAPPER", "kache");
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| eyre!(e))?;
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 5000)));
        let mut reader = pair.master.try_clone_reader().map_err(|e| eyre!(e))?;
        let writer = pair.master.take_writer().map_err(|e| eyre!(e))?;
        let id = launch.bead.id.clone();
        let p = parser.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        p.lock().unwrap().process(&buf[..n]);
                        if tx.send(Event::AgentOutput).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Event::AgentExited(id));
        });

        Ok(Self {
            parser,
            alive: true,
            writer,
            master: pair.master,
            child,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.alive {
            let _ = self.writer.write_all(bytes);
            let _ = self.writer.flush();
        }
    }

    pub fn submit(&mut self, text: &str) {
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        let mut bytes = crate::ui::keys::paste(text, bracketed);
        bytes.push(b'\r');
        self.write(&bytes);
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    pub fn scroll(&mut self, delta: isize) {
        let mut p = self.parser.lock().unwrap();
        let cur = p.screen().scrollback() as isize;
        p.screen_mut().set_scrollback((cur + delta).max(0) as usize);
    }

    pub fn scroll_reset(&mut self) {
        self.parser.lock().unwrap().screen_mut().set_scrollback(0);
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.alive = false;
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if self.alive {
            self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_stable_uuid() {
        let repo = std::path::Path::new("/r");
        let a = session_id(repo, "x-1");
        assert_eq!(a, session_id(repo, "x-1"));
        assert_ne!(a, session_id(repo, "x-2"));
        let parts: Vec<_> = a.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }
}
