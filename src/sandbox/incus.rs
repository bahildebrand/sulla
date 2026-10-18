use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use color_eyre::{Result, eyre::eyre};
use portable_pty::CommandBuilder;

use super::{Outcome, Sandbox};
use crate::agent::Launch;
use crate::config::IncusConfig;

const MAX_NAME: usize = 63;
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const BOOT_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct Host {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub home: String,
}

impl Host {
    fn detect() -> Result<Self> {
        let id = |flag: &str| -> Result<String> {
            let out = Command::new("id").arg(flag).output()?;
            if !out.status.success() {
                return Err(eyre!("id {flag} failed"));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        Ok(Self {
            uid: id("-u")?.parse()?,
            gid: id("-g")?.parse()?,
            user: id("-un")?,
            home: std::env::var("HOME").map_err(|_| eyre!("HOME is not set"))?,
        })
    }

    fn idmap(&self) -> String {
        if self.uid == self.gid {
            format!("both {} {}", self.uid, self.uid)
        } else {
            format!(
                "uid {} {}\ngid {} {}",
                self.uid, self.uid, self.gid, self.gid
            )
        }
    }
}

pub struct Incus {
    cfg: IncusConfig,
    repo: PathBuf,
    host: Host,
}

impl Incus {
    pub fn new(cfg: IncusConfig, repo: PathBuf) -> Result<Self> {
        match Command::new(&cfg.bin).arg("--version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(eyre!(
                    "incus sandbox: `{}` not found on PATH, install incus or set [incus] bin",
                    cfg.bin
                ));
            }
            Err(e) => return Err(eyre!("incus sandbox: running `{}`: {e}", cfg.bin)),
            Ok(_) => {}
        }
        let host = Host::detect()?;
        let me = Self { cfg, repo, host };
        if !me.exists(&me.cfg.golden) {
            return Err(eyre!(
                "incus sandbox: golden instance `{}` does not exist, build it first or set [incus] golden",
                me.cfg.golden
            ));
        }
        Ok(me)
    }

    fn name(&self, slot: usize) -> String {
        container_name(&self.cfg.prefix, &self.repo, slot)
    }

    fn run<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<String> {
        let out = Command::new(&self.cfg.bin).args(args).output()?;
        if !out.status.success() {
            let argv: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy()).collect();
            return Err(eyre!(
                "{} {}: {}",
                self.cfg.bin,
                argv.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn exists(&self, name: &str) -> bool {
        self.run(&["query", &format!("/1.0/instances/{name}")])
            .is_ok()
    }

    fn running(&self, name: &str) -> Result<bool> {
        let out = self.run(&["query", &format!("/1.0/instances/{name}/state")])?;
        let v: serde_json::Value = serde_json::from_str(&out)?;
        Ok(v["status"].as_str() == Some("Running"))
    }

    fn idmap_error(&self, e: color_eyre::Report) -> color_eyre::Report {
        let msg = format!("{e:#}");
        if msg.contains("idmap") || msg.contains("subuid") || msg.contains("subgid") {
            eyre!(
                "{msg}\nincus cannot map uid {uid}/gid {gid}: add `root:{uid}:1` to /etc/subuid and `root:{gid}:1` to /etc/subgid, then restart incus",
                uid = self.host.uid,
                gid = self.host.gid
            )
        } else {
            e
        }
    }

    fn create(&self, name: &str) -> Result<()> {
        tracing::info!("incus: copying {} to {name}", self.cfg.golden);
        self.run(&[
            "copy",
            &self.cfg.golden,
            name,
            "--storage",
            &self.cfg.storage_pool,
        ])?;
        let r = self.configure(name);
        if r.is_err() {
            tracing::warn!("incus: configuring {name} failed, deleting it");
            let _ = self.run(&["delete", "--force", name]);
        }
        r
    }

    fn configure(&self, name: &str) -> Result<()> {
        let mut set = vec![
            "config".to_string(),
            "set".into(),
            name.into(),
            "security.nesting=true".into(),
            "security.syscalls.intercept.mknod=true".into(),
            "security.syscalls.intercept.setxattr=true".into(),
            format!("limits.memory={}", self.cfg.memory),
            format!("limits.kernel.nofile={}", self.cfg.nofile),
        ];
        if let Some(c) = self.cfg.cpus {
            set.push(format!("limits.cpu={c}"));
        }
        self.run(&set)?;
        self.run(&["config", "set", name, "raw.idmap", &self.host.idmap()])
            .map_err(|e| self.idmap_error(e))?;
        let repo = self.repo.to_string_lossy();
        self.add_disk(name, "repo", &repo)?;
        for (i, m) in self.cfg.mounts.iter().enumerate() {
            let path = expand_home(m, &self.host.home);
            if !Path::new(&path).exists() {
                tracing::info!("incus: skipping missing mount {path}");
                continue;
            }
            self.add_disk(name, &format!("mount{i}"), &path)?;
        }
        tracing::info!("incus: snapshotting {name} as {}", self.cfg.snapshot);
        self.run(&["snapshot", "create", name, &self.cfg.snapshot])?;
        Ok(())
    }

    fn add_disk(&self, name: &str, device: &str, path: &str) -> Result<()> {
        self.run(&[
            "config",
            "device",
            "add",
            name,
            device,
            "disk",
            &format!("source={path}"),
            &format!("path={path}"),
        ])
        .map(|_| ())
    }

    fn wait_ready(&self, name: &str) -> Result<()> {
        let start = Instant::now();
        let probes: [&[&str]; 2] = [
            &["systemctl", "is-system-running", "--wait"],
            &["docker", "info"],
        ];
        loop {
            for p in probes {
                let mut args = vec!["exec", name, "--"];
                args.extend_from_slice(p);
                if self.run(&args).is_ok() {
                    return Ok(());
                }
            }
            if start.elapsed() >= BOOT_TIMEOUT {
                return Err(eyre!(
                    "incus: {name} not ready after {}s",
                    BOOT_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(BOOT_POLL);
        }
    }
}

impl Sandbox for Incus {
    fn name(&self) -> &str {
        "incus"
    }

    fn prepare(&mut self, slot: usize, launch: &Launch) -> Result<()> {
        let name = self.name(slot);
        tracing::info!("incus: preparing {name} for {}", launch.id);
        if !self.exists(&name) {
            self.create(&name)?;
        }
        if !self.running(&name)? {
            tracing::info!("incus: starting {name}");
            self.run(&["start", &name])
                .map_err(|e| self.idmap_error(e))?;
        }
        self.wait_ready(&name)?;
        tracing::info!("incus: {name} ready");
        Ok(())
    }

    fn wrap(&self, slot: usize, launch: &Launch, cmd: CommandBuilder) -> Result<CommandBuilder> {
        let cwd = cmd
            .get_cwd()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|| launch.worktree.to_string_lossy().into_owned());
        let env = exec_env(cmd.iter_extra_env_as_str(), &self.host, &self.cfg.env);
        let argv = exec_argv(
            &self.cfg.bin,
            &self.name(slot),
            &self.host,
            &cwd,
            &env,
            cmd.get_argv(),
        );
        let mut out = CommandBuilder::from_argv(argv);
        out.cwd(&cwd);
        Ok(out)
    }

    fn finished(&mut self, slot: usize, id: &str, outcome: Outcome) -> Result<()> {
        tracing::info!("incus: {id} finished on {} ({outcome:?})", self.name(slot));
        Ok(())
    }

    fn shutdown(&mut self) {}
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        out.push(c);
    }
    out.trim_end_matches('-').to_string()
}

pub fn container_name(prefix: &str, repo: &Path, slot: usize) -> String {
    let mut prefix = sanitize(prefix);
    if !prefix.starts_with(|c: char| c.is_ascii_alphabetic()) {
        prefix = format!("s{prefix}");
    }
    let hash = format!("{:06x}", fnv(repo.to_string_lossy().as_bytes()) & 0xff_ffff);
    let tail = format!("{hash}-{slot}");
    let dir = repo
        .file_name()
        .map(|n| sanitize(&n.to_string_lossy()))
        .unwrap_or_default();
    let keep = MAX_NAME - tail.len() - 1;
    let prefix = prefix[..prefix.len().min(keep)].trim_end_matches('-');
    let room = keep.saturating_sub(prefix.len() + 1);
    let dir = dir[..dir.len().min(room)].trim_end_matches('-');
    let mut name = prefix.to_string();
    if !dir.is_empty() {
        name.push('-');
        name.push_str(dir);
    }
    name.push('-');
    name.push_str(&tail);
    name
}

fn expand_home(path: &str, home: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

fn exec_env<'a>(
    extra: impl Iterator<Item = (&'a str, &'a str)>,
    host: &Host,
    cfg: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: &str| match env.iter_mut().find(|(ek, _)| ek == k) {
        Some(e) => e.1 = v.to_string(),
        None => env.push((k.to_string(), v.to_string())),
    };
    for (k, v) in extra {
        put(k, v);
    }
    put("HOME", &host.home);
    put("USER", &host.user);
    for (k, v) in cfg {
        put(k, v);
    }
    env
}

fn exec_argv(
    bin: &str,
    name: &str,
    host: &Host,
    cwd: &str,
    env: &[(String, String)],
    argv: &[OsString],
) -> Vec<OsString> {
    let mut out: Vec<OsString> = [
        bin,
        "exec",
        name,
        "--user",
        &host.uid.to_string(),
        "--group",
        &host.gid.to_string(),
        "--cwd",
        cwd,
        "-t",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for (k, v) in env {
        out.push("--env".into());
        out.push(format!("{k}={v}").into());
    }
    out.push("--".into());
    out.extend(argv.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            uid: 1000,
            gid: 1001,
            user: "blake".into(),
            home: "/home/blake".into(),
        }
    }

    fn valid(name: &str) -> bool {
        name.len() <= MAX_NAME
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && !name.ends_with('-')
            && !name.contains("--")
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    #[test]
    fn container_name_is_stable_slug() {
        let repo = Path::new("/home/blake/git/My_Repo.rs");
        let a = container_name("sulla", repo, 2);
        assert_eq!(a, container_name("sulla", repo, 2));
        assert!(a.starts_with("sulla-my-repo-rs-"), "{a}");
        assert!(a.ends_with("-2"));
        let hash = a
            .trim_start_matches("sulla-my-repo-rs-")
            .trim_end_matches("-2");
        assert_eq!(hash.len(), 6);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(valid(&a));
        assert_ne!(
            a,
            container_name("sulla", Path::new("/other/My_Repo.rs"), 2)
        );
        assert_ne!(a, container_name("sulla", repo, 3));
    }

    #[test]
    fn container_name_stays_valid_when_long() {
        let long = format!("/x/{}", "Very-Long_Name.".repeat(10));
        let a = container_name("sulla", Path::new(&long), 11);
        assert!(valid(&a), "{a}");
        assert!(a.starts_with("sulla-very-long-name"));
        assert!(a.ends_with("-11"));

        let b = container_name("9_", Path::new("/"), 0);
        assert!(valid(&b), "{b}");

        let c = container_name(&"p".repeat(80), Path::new("/r"), 1);
        assert!(valid(&c), "{c}");
        assert!(c.ends_with("-1"));
    }

    #[test]
    fn expand_home_only_leading_tilde() {
        assert_eq!(expand_home("~/.claude", "/h"), "/h/.claude");
        assert_eq!(expand_home("~", "/h"), "/h");
        assert_eq!(expand_home("/abs/~x", "/h"), "/abs/~x");
        assert_eq!(expand_home("~other/x", "/h"), "~other/x");
    }

    #[test]
    fn wrap_argv_forwards_env_and_argv() {
        let mut cmd = CommandBuilder::new("copilot");
        cmd.args(["--yolo", "-i", "do it"]);
        cmd.cwd("/repo/.sulla/worktrees/x-1");
        cmd.env("TERM", "xterm-256color");
        cmd.env("SULLA_SLOT", "1");
        cmd.env("HOME", "/elsewhere");
        let mut cfg = BTreeMap::new();
        cfg.insert("FOO".to_string(), "bar".to_string());
        cfg.insert("TERM".to_string(), "dumb".to_string());
        let env = exec_env(cmd.iter_extra_env_as_str(), &host(), &cfg);
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|e| e.1.as_str());
        assert_eq!(get("HOME"), Some("/home/blake"));
        assert_eq!(get("USER"), Some("blake"));
        assert_eq!(get("SULLA_SLOT"), Some("1"));
        assert_eq!(get("FOO"), Some("bar"));
        assert_eq!(get("TERM"), Some("dumb"));
        assert_eq!(env.iter().filter(|(k, _)| k == "HOME").count(), 1);

        let argv = exec_argv(
            "incus",
            "sulla-r-abcdef-1",
            &host(),
            "/repo/.sulla/worktrees/x-1",
            &[("A".into(), "1".into()), ("B".into(), "x y".into())],
            cmd.get_argv(),
        );
        let argv: Vec<_> = argv.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            argv,
            [
                "incus",
                "exec",
                "sulla-r-abcdef-1",
                "--user",
                "1000",
                "--group",
                "1001",
                "--cwd",
                "/repo/.sulla/worktrees/x-1",
                "-t",
                "--env",
                "A=1",
                "--env",
                "B=x y",
                "--",
                "copilot",
                "--yolo",
                "-i",
                "do it",
            ]
        );
    }

    #[test]
    fn missing_binary_fails_with_clear_error() {
        let cfg = IncusConfig {
            bin: "sulla-no-such-incus".into(),
            ..IncusConfig::default()
        };
        let err = Incus::new(cfg, "/r".into()).err().unwrap().to_string();
        assert!(
            err.contains("`sulla-no-such-incus` not found on PATH"),
            "{err}"
        );
    }

    #[test]
    fn idmap_handles_distinct_gid() {
        assert_eq!(host().idmap(), "uid 1000 1000\ngid 1001 1001");
        let mut h = host();
        h.gid = 1000;
        assert_eq!(h.idmap(), "both 1000 1000");
    }
}
