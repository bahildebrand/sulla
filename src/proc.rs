use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use color_eyre::{Result, eyre::eyre};

pub const NO_PROMPTS: [(&str, &str); 3] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GCM_INTERACTIVE", "never"),
    ("GH_PROMPT_DISABLED", "1"),
];

pub const BD: Duration = Duration::from_secs(60);
pub const GIT: Duration = Duration::from_secs(120);
pub const NETWORK: Duration = Duration::from_secs(120);
pub const INCUS: Duration = Duration::from_secs(600);

const POLL_MIN: Duration = Duration::from_millis(1);
const POLL_MAX: Duration = Duration::from_millis(25);
const DESCRIBE_MAX: usize = 120;

pub fn output(cmd: &mut Command, input: Option<&[u8]>, timeout: Duration) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    cmd.envs(NO_PROMPTS)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let mut poll = POLL_MIN;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            return Err(timed_out(cmd, timeout));
        }
        std::thread::sleep(poll.min(left));
        poll = (poll * 2).min(POLL_MAX);
    };
    let collect = |rx: Receiver<Vec<u8>>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    };
    match (collect(stdout), collect(stderr)) {
        (Some(stdout), Some(stderr)) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        _ => {
            kill_group(child.id());
            Err(timed_out(cmd, timeout))
        }
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

fn kill_group(pgid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    if s.chars().count() > DESCRIBE_MAX {
        s = s.chars().take(DESCRIBE_MAX).collect::<String>() + "…";
    }
    s
}

fn timed_out(cmd: &Command, timeout: Duration) -> color_eyre::Report {
    eyre!(
        "`{}` timed out after {}s and was killed",
        describe(cmd),
        timeout.as_secs_f32()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn captures_stdout_and_stderr_on_success() {
        let out = output(&mut sh("echo out; echo err >&2"), None, GIT).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn reports_non_zero_exit() {
        let out = output(&mut sh("echo nope >&2; exit 3"), None, GIT).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stderr, b"nope\n");
    }

    #[test]
    fn timeout_kills_the_child_and_its_children() {
        let dir = std::env::temp_dir().join(format!("sulla-proc-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("survived");
        let script = format!("(sleep 1; touch {}) & sleep 30", marker.display());
        let started = Instant::now();
        let err = output(&mut sh(&script), None, Duration::from_millis(200)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        let msg = err.to_string();
        assert!(msg.contains("timed out after 0.2s"), "{msg}");
        assert!(msg.contains("sh -c"), "{msg}");
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "background child outlived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sets_no_prompt_env_and_null_stdin() {
        let out = output(
            &mut sh("echo $GIT_TERMINAL_PROMPT $GCM_INTERACTIVE $GH_PROMPT_DISABLED; cat"),
            None,
            GIT,
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "0 never 1\n");
    }

    #[test]
    fn feeds_stdin_when_given() {
        let out = output(&mut Command::new("cat"), Some(b"hello"), GIT).unwrap();
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn missing_program_is_a_not_found_error() {
        let err = output(&mut Command::new("sulla-no-such-program"), None, GIT).unwrap_err();
        let io = err.downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    }
}
