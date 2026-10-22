use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use color_eyre::{Result, eyre::eyre};
use portable_pty::CommandBuilder;
use serde_json::{Value, json};

use super::{Outcome, Prepare, Sandbox};
use crate::agent::Launch;
use crate::beads;
use crate::config::{AgentKind, IncusConfig};
use crate::sessions;

const MAX_NAME: usize = 63;
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const BOOT_POLL: Duration = Duration::from_secs(1);
const ADDR_TRIES: u32 = 30;
const BEADS_DEVICE: &str = "beads-dolt";
const USER_SCRIPT: &str = r#"set -e
uid=$1 gid=$2 user=$3 home=$4
if ! getent group "$gid" >/dev/null; then
    if getent group "$user" >/dev/null; then
        groupmod -g "$gid" "$user"
    else
        groupadd -g "$gid" "$user"
    fi
fi
name=$(getent passwd "$uid" | cut -d: -f1)
if [ -z "$name" ]; then
    useradd -u "$uid" -g "$gid" -M -d "$home" -s /bin/bash "$user"
    name=$user
elif [ "$(id -g "$name")" != "$gid" ]; then
    usermod -g "$gid" "$name"
fi
"#;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reset {
    None,
    Light,
    Hard,
}

pub fn decide(outcome: Outcome, count: u32, reset_every: u32, light_ok: bool) -> Reset {
    match outcome {
        Outcome::Exited => Reset::None,
        Outcome::Discarded => Reset::Hard,
        _ if reset_every > 0 && count >= reset_every => Reset::Hard,
        _ if !light_ok => Reset::Hard,
        _ => Reset::Light,
    }
}

fn light_script(uid: u32, volumes: bool) -> String {
    let mut s = String::from("docker ps -aq | xargs -r docker rm -f && docker network prune -f");
    if volumes {
        s.push_str(" && docker volume prune -af");
    }
    s.push_str(&format!(" && {{ pkill -KILL -u {uid}; [ $? -le 1 ]; }}"));
    s
}

#[derive(Debug, Clone)]
struct Cli {
    bin: String,
}

impl Cli {
    fn run<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<String> {
        let out = Command::new(&self.bin).args(args).output()?;
        if !out.status.success() {
            let argv: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy()).collect();
            return Err(eyre!(
                "{} {}: {}",
                self.bin,
                argv.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn feed<S: AsRef<std::ffi::OsStr>>(&self, args: &[S], input: &str) -> Result<()> {
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or_else(|| eyre!("{} has no stdin", self.bin))?
            .write_all(input.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(eyre!(
                "{} exec: {}",
                self.bin,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    fn query(&self, path: &str) -> Result<serde_json::Value> {
        Ok(serde_json::from_str(&self.run(&["query", path])?)?)
    }

    fn exists(&self, name: &str) -> bool {
        self.run(&["query", &format!("/1.0/instances/{name}")])
            .is_ok()
    }

    fn running(&self, name: &str) -> Result<bool> {
        let v = self.query(&format!("/1.0/instances/{name}/state"))?;
        Ok(v["status"].as_str() == Some("Running"))
    }
}

struct Resetter {
    cli: Cli,
    name: String,
    snapshot: String,
    script: String,
    outcome: Outcome,
    count: u32,
    reset_every: u32,
}

impl Resetter {
    fn run(self) -> Result<Reset> {
        let mut plan = decide(self.outcome, self.count, self.reset_every, true);
        if plan == Reset::Light {
            tracing::info!("incus: light reset of {}", self.name);
            let r = self
                .cli
                .run(&["exec", &self.name, "--", "sh", "-c", &self.script]);
            if let Err(e) = &r {
                tracing::warn!("incus: light reset of {} failed: {e:#}", self.name);
            }
            plan = decide(self.outcome, self.count, self.reset_every, r.is_ok());
        }
        if plan == Reset::Hard {
            tracing::info!("incus: hard reset of {} to {}", self.name, self.snapshot);
            if self.cli.running(&self.name)? {
                self.cli.run(&["stop", "--force", &self.name])?;
            }
            self.cli
                .run(&["snapshot", "restore", &self.name, &self.snapshot])?;
        }
        Ok(plan)
    }
}

#[derive(Clone)]
struct Driver {
    cfg: IncusConfig,
    repo: PathBuf,
    host: Host,
    cli: Cli,
    addrs: Arc<Mutex<HashMap<usize, String>>>,
}

type Counts = Arc<Mutex<HashMap<usize, u32>>>;

pub struct Incus {
    driver: Driver,
    counts: Counts,
    resets: HashMap<usize, JoinHandle<Result<Reset>>>,
    busy: HashMap<usize, Arc<Mutex<()>>>,
}

impl Incus {
    pub fn new(mut cfg: IncusConfig, repo: PathBuf) -> Result<Self> {
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
        if beads::server_mode(&repo) {
            cfg.env
                .entry("BEADS_DOLT_AUTO_START".into())
                .or_insert_with(|| "0".into());
        }
        let host = Host::detect()?;
        let cli = Cli {
            bin: cfg.bin.clone(),
        };
        let driver = Driver {
            cfg,
            repo,
            host,
            cli,
            addrs: Arc::default(),
        };
        if !driver.exists(&driver.cfg.golden) {
            return Err(eyre!(
                "incus sandbox: golden instance `{}` does not exist, build it first or set [incus] golden",
                driver.cfg.golden
            ));
        }
        Ok(Self {
            driver,
            counts: Counts::default(),
            resets: HashMap::new(),
            busy: HashMap::new(),
        })
    }

    fn settle(&mut self, slot: usize) {
        if let Some(h) = self.resets.remove(&slot) {
            settle(h, &self.driver.name(slot), slot, &self.counts);
        }
    }
}

fn settle(h: JoinHandle<Result<Reset>>, name: &str, slot: usize, counts: &Counts) {
    match h.join() {
        Ok(Ok(Reset::Hard)) => {
            counts.lock().unwrap().insert(slot, 0);
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("incus: resetting {name} failed: {e:#}"),
        Err(_) => tracing::warn!("incus: reset thread for {name} panicked"),
    }
}

impl Driver {
    fn name(&self, slot: usize) -> String {
        container_name(&self.cfg.prefix, &self.repo, slot)
    }

    fn run<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> Result<String> {
        self.cli.run(args)
    }

    fn exists(&self, name: &str) -> bool {
        self.cli.exists(name)
    }

    fn running(&self, name: &str) -> Result<bool> {
        self.cli.running(name)
    }

    fn docker_volume(&self, name: &str) -> Result<()> {
        let pool = &self.cfg.storage_pool;
        let vol = format!("{name}-docker");
        let path = format!("/1.0/storage-pools/{pool}/volumes/custom/{vol}");
        if self.cli.query(&path).is_err() {
            tracing::info!("incus: creating storage volume {pool}/{vol}");
            self.run(&[
                "storage",
                "volume",
                "create",
                pool,
                &vol,
                "--type=filesystem",
            ])?;
        }
        let inst = self.cli.query(&format!("/1.0/instances/{name}"))?;
        if inst["devices"].get("docker").is_none() {
            tracing::info!("incus: attaching {vol} to {name}");
            self.run(&[
                "config",
                "device",
                "add",
                name,
                "docker",
                "disk",
                &format!("pool={pool}"),
                &format!("source={vol}"),
                "path=/var/lib/docker",
            ])?;
        }
        Ok(())
    }

    fn beads_proxy(&self, name: &str) -> Result<()> {
        if !beads::server_mode(&self.repo) {
            return Ok(());
        }
        beads::start_dolt(&self.repo)
            .map_err(|e| eyre!("incus: {name} needs the host Dolt server: {e:#}"))?;
        let Some(port) = beads::dolt_port(&self.repo) else {
            return Err(eyre!(
                "incus: {name} needs the host Dolt server, but .beads/dolt-server.port is missing after `bd dolt start`"
            ));
        };
        let addr = format!("tcp:127.0.0.1:{port}");
        let inst = self.cli.query(&format!("/1.0/instances/{name}"))?;
        let dev = &inst["devices"][BEADS_DEVICE];
        if dev["listen"] == addr.as_str() && dev["connect"] == addr.as_str() {
            return Ok(());
        }
        if !dev.is_null() {
            self.run(&["config", "device", "remove", name, BEADS_DEVICE])?;
        }
        tracing::info!("incus: forwarding {addr} in {name} to the host Dolt server");
        self.run(&proxy_args(name, &addr)).map(drop)
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
        self.docker_volume(name)?;
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

    fn refresh_address(&self, slot: usize) {
        self.addrs.lock().unwrap().remove(&slot);
        let addrs = self.addrs.clone();
        let cli = self.cli.clone();
        let name = self.name(slot);
        std::thread::spawn(move || {
            for _ in 0..ADDR_TRIES {
                let ip = cli
                    .run(&["list", &name, "--format", "json"])
                    .ok()
                    .and_then(|out| parse_ipv4(&out, &name));
                if let Some(ip) = ip {
                    addrs.lock().unwrap().insert(slot, ip);
                    return;
                }
                std::thread::sleep(BOOT_POLL);
            }
            tracing::warn!("incus: no IPv4 on eth0 for {name}");
        });
    }

    fn ensure_user(&self, name: &str) -> Result<()> {
        let uid = self.host.uid.to_string();
        let gid = self.host.gid.to_string();
        self.run(&[
            "exec",
            name,
            "--",
            "sh",
            "-c",
            USER_SCRIPT,
            "sh",
            &uid,
            &gid,
            &self.host.user,
            &self.host.home,
        ])
        .map(drop)
        .map_err(|e| eyre!("incus: setting up user {} in {name}: {e:#}", self.host.user))
    }

    fn seed_claude(&self, name: &str) -> Result<()> {
        let path = claude_config(&self.cfg, &self.host.home);
        let path = path.to_string_lossy();
        let uid = self.host.uid.to_string();
        let gid = self.host.gid.to_string();
        let user = ["exec", name, "--user", &uid, "--group", &gid, "--"];
        let read = [&user[..], &["cat", &path]].concat();
        let mut v = match self.run(&read) {
            Err(_) => json!({}),
            Ok(s) => match serde_json::from_str::<Value>(&s) {
                Ok(v) if v.is_object() => v,
                _ => return Err(eyre!("{path} is not a JSON object, leaving it alone")),
            },
        };
        if !trust_claude(&mut v, &self.repo) {
            return Ok(());
        }
        tracing::info!("incus: seeding {path} in {name}");
        let script = "umask 077 && cat > \"$1.sulla\" && mv \"$1.sulla\" \"$1\"";
        let write = [&user[..], &["sh", "-c", script, "sh", &path]].concat();
        self.cli.feed(&write, &serde_json::to_string_pretty(&v)?)
    }

    fn prepare(&self, slot: usize, id: &str) -> Result<()> {
        let name = self.name(slot);
        tracing::info!("incus: preparing {name} for {id}");
        if !self.exists(&name) {
            self.create(&name)?;
        }
        if !self.running(&name)? {
            self.docker_volume(&name)?;
            tracing::info!("incus: starting {name}");
            self.run(&["start", &name])
                .map_err(|e| self.idmap_error(e))?;
        }
        self.wait_ready(&name)?;
        self.ensure_user(&name)?;
        if let Err(e) = self.seed_claude(&name) {
            tracing::warn!("incus: seeding Claude config in {name} failed: {e:#}");
        }
        self.beads_proxy(&name)?;
        tracing::info!("incus: {name} ready");
        self.refresh_address(slot);
        Ok(())
    }
}

impl Sandbox for Incus {
    fn name(&self) -> &str {
        "incus"
    }

    fn prepare(&mut self, slot: usize, id: &str) -> Prepare {
        let reset = self.resets.remove(&slot);
        let busy = self.busy.entry(slot).or_default().clone();
        let counts = self.counts.clone();
        let driver = self.driver.clone();
        let id = id.to_string();
        Box::new(move || {
            let _busy = busy.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(h) = reset {
                settle(h, &driver.name(slot), slot, &counts);
            }
            driver.prepare(slot, &id)
        })
    }

    fn wrap(&self, slot: usize, launch: &Launch, cmd: CommandBuilder) -> Result<CommandBuilder> {
        let cwd = cmd
            .get_cwd()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|| launch.worktree.to_string_lossy().into_owned());
        let d = &self.driver;
        let env = exec_env(cmd.iter_extra_env_as_str(), &d.host, &d.cfg.env);
        let argv = exec_argv(
            &d.cfg.bin,
            &d.name(slot),
            &d.host,
            &cwd,
            &env,
            &init_argv(d.cfg.init.as_deref(), cmd.get_argv()),
            true,
        );
        let mut out = CommandBuilder::from_argv(argv);
        out.cwd(&cwd);
        Ok(out)
    }

    fn finished(&mut self, slot: usize, id: &str, outcome: Outcome) -> Result<()> {
        let d = &self.driver;
        let name = d.name(slot);
        tracing::info!("incus: {id} finished on {name} ({outcome:?})");
        if outcome == Outcome::Exited {
            return Ok(());
        }
        self.settle(slot);
        let d = &self.driver;
        d.addrs.lock().unwrap().remove(&slot);
        let mut counts = self.counts.lock().unwrap();
        let count = counts.entry(slot).or_insert(0);
        *count += 1;
        let job = Resetter {
            cli: d.cli.clone(),
            name: name.clone(),
            snapshot: d.cfg.snapshot.clone(),
            script: light_script(d.host.uid, d.cfg.reset_volumes),
            outcome,
            count: *count,
            reset_every: d.cfg.reset_every,
        };
        drop(counts);
        let h = std::thread::Builder::new()
            .name(format!("reset-{name}"))
            .spawn(move || job.run())?;
        self.resets.insert(slot, h);
        Ok(())
    }

    fn shutdown(&mut self) {
        let slots: Vec<usize> = self.resets.keys().copied().collect();
        for slot in slots {
            self.settle(slot);
        }
    }

    fn instance(&self, slot: usize) -> Option<String> {
        Some(self.driver.name(slot))
    }

    fn address(&self, slot: usize) -> Option<String> {
        self.driver.addrs.lock().unwrap().get(&slot).cloned()
    }

    fn isolated(&self) -> bool {
        true
    }

    fn session_root(&self, kind: AgentKind, _host: &Path) -> Option<PathBuf> {
        session_root(&self.driver.cfg, &self.driver.host.home, kind)
    }

    fn command(
        &self,
        slot: usize,
        cwd: &Path,
        argv: &[OsString],
        env: &[(String, String)],
    ) -> Command {
        self.driver.exec(slot, cwd, argv, env, false)
    }

    fn shell(&self, slot: usize, cwd: &Path, env: &[(String, String)]) -> Command {
        self.driver.exec(slot, cwd, &["bash".into()], env, true)
    }
}

impl Driver {
    fn exec(
        &self,
        slot: usize,
        cwd: &Path,
        argv: &[OsString],
        env: &[(String, String)],
        tty: bool,
    ) -> Command {
        let env = exec_env(
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &self.host,
            &self.cfg.env,
        );
        let cwd = cwd.to_string_lossy();
        let argv = exec_argv(
            &self.cfg.bin,
            &self.name(slot),
            &self.host,
            &cwd,
            &env,
            &init_argv(self.cfg.init.as_deref(), argv),
            tty,
        );
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    }
}

fn proxy_args(name: &str, addr: &str) -> Vec<String> {
    [
        "config",
        "device",
        "add",
        name,
        BEADS_DEVICE,
        "proxy",
        &format!("listen={addr}"),
        &format!("connect={addr}"),
        "bind=instance",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn parse_ipv4(json: &str, name: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let inst = v.as_array()?.iter().find(|i| i["name"] == name)?;
    inst["state"]["network"]["eth0"]["addresses"]
        .as_array()?
        .iter()
        .find(|a| a["family"] == "inet" && a["scope"] == "global")?["address"]
        .as_str()
        .map(String::from)
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

fn session_root(cfg: &IncusConfig, home: &str, kind: AgentKind) -> Option<PathBuf> {
    let root = match cfg.env.get(sessions::env_var(kind)) {
        Some(v) => PathBuf::from(expand_home(v, home)),
        None => sessions::default_root(kind, Path::new(home)),
    };
    cfg.mounts
        .iter()
        .any(|m| root.starts_with(expand_home(m, home)))
        .then_some(root)
}

fn claude_config(cfg: &IncusConfig, home: &str) -> PathBuf {
    match cfg.env.get(sessions::env_var(AgentKind::Claude)) {
        Some(dir) => PathBuf::from(expand_home(dir, home)).join(".claude.json"),
        None => Path::new(home).join(".claude.json"),
    }
}

fn trust_claude(v: &mut Value, repo: &Path) -> bool {
    let mut changed = false;
    if v["hasCompletedOnboarding"] != true {
        v["hasCompletedOnboarding"] = true.into();
        changed = true;
    }
    if !v["projects"].is_object() {
        v["projects"] = json!({});
    }
    let project = &mut v["projects"][repo.to_string_lossy().as_ref()];
    if !project.is_object() {
        *project = json!({});
    }
    if project["hasTrustDialogAccepted"] != true {
        project["hasTrustDialogAccepted"] = true.into();
        changed = true;
    }
    changed
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

fn init_argv(init: Option<&str>, argv: &[OsString]) -> Vec<OsString> {
    let Some(init) = init.filter(|s| !s.trim().is_empty()) else {
        return argv.to_vec();
    };
    let mut out: Vec<OsString> = ["bash", "-c", &format!("{init}\nexec \"$@\""), "sulla"]
        .into_iter()
        .map(OsString::from)
        .collect();
    out.extend(argv.iter().cloned());
    out
}

fn exec_argv(
    bin: &str,
    name: &str,
    host: &Host,
    cwd: &str,
    env: &[(String, String)],
    argv: &[OsString],
    tty: bool,
) -> Vec<OsString> {
    let mut out: Vec<OsString> = [
        bin,
        "exec",
        name,
        "--cwd",
        cwd,
        if tty { "-t" } else { "-T" },
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for (k, v) in env {
        out.push("--env".into());
        out.push(format!("{k}={v}").into());
    }
    out.extend(
        [
            "--".into(),
            "setpriv".into(),
            format!("--reuid={}", host.uid),
            format!("--regid={}", host.gid),
            "--init-groups".into(),
            "--".into(),
        ]
        .map(OsString::from),
    );
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
    fn session_root_requires_a_host_mount() {
        let mut cfg = IncusConfig::default();
        assert_eq!(
            session_root(&cfg, "/h", AgentKind::Copilot),
            Some(PathBuf::from("/h/.copilot"))
        );
        cfg.env
            .insert("CLAUDE_CONFIG_DIR".into(), "~/.claude/alt".into());
        assert_eq!(
            session_root(&cfg, "/h", AgentKind::Claude),
            Some(PathBuf::from("/h/.claude/alt"))
        );
        cfg.env
            .insert("CLAUDE_CONFIG_DIR".into(), "/opt/claude".into());
        assert_eq!(session_root(&cfg, "/h", AgentKind::Claude), None);
        cfg.mounts.clear();
        assert_eq!(session_root(&cfg, "/h", AgentKind::Copilot), None);
    }

    #[test]
    fn claude_config_follows_config_dir() {
        let mut cfg = IncusConfig::default();
        assert_eq!(claude_config(&cfg, "/h"), PathBuf::from("/h/.claude.json"));
        cfg.env
            .insert("CLAUDE_CONFIG_DIR".into(), "~/.claude/alt".into());
        assert_eq!(
            claude_config(&cfg, "/h"),
            PathBuf::from("/h/.claude/alt/.claude.json")
        );
    }

    #[test]
    fn trust_claude_skips_onboarding_and_trusts_repo() {
        let repo = Path::new("/home/blake/git/memfault");
        let mut v = json!({});
        assert!(trust_claude(&mut v, repo));
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(
            v["projects"]["/home/blake/git/memfault"]["hasTrustDialogAccepted"],
            true
        );
        assert!(!trust_claude(&mut v, repo));
    }

    #[test]
    fn trust_claude_keeps_existing_settings() {
        let repo = Path::new("/r");
        let mut v = json!({
            "numStartups": 3,
            "projects": {
                "/r": {"allowedTools": ["x"]},
                "/other": {"hasTrustDialogAccepted": false}
            }
        });
        assert!(trust_claude(&mut v, repo));
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["projects"]["/r"]["allowedTools"], json!(["x"]));
        assert_eq!(v["projects"]["/r"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"]["/other"]["hasTrustDialogAccepted"], false);
    }

    #[test]
    fn expand_home_only_leading_tilde() {
        assert_eq!(expand_home("~/.claude", "/h"), "/h/.claude");
        assert_eq!(expand_home("~", "/h"), "/h");
        assert_eq!(expand_home("/abs/~x", "/h"), "/abs/~x");
        assert_eq!(expand_home("~other/x", "/h"), "~other/x");
    }

    #[test]
    fn beads_proxy_listens_on_container_loopback_only() {
        let args = proxy_args("c", "tcp:127.0.0.1:46509");
        assert_eq!(
            args,
            [
                "config",
                "device",
                "add",
                "c",
                "beads-dolt",
                "proxy",
                "listen=tcp:127.0.0.1:46509",
                "connect=tcp:127.0.0.1:46509",
                "bind=instance",
            ]
        );
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
            true,
        );
        let argv: Vec<_> = argv.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            argv,
            [
                "incus",
                "exec",
                "sulla-r-abcdef-1",
                "--cwd",
                "/repo/.sulla/worktrees/x-1",
                "-t",
                "--env",
                "A=1",
                "--env",
                "B=x y",
                "--",
                "setpriv",
                "--reuid=1000",
                "--regid=1001",
                "--init-groups",
                "--",
                "copilot",
                "--yolo",
                "-i",
                "do it",
            ]
        );
        let argv = exec_argv("incus", "n", &host(), "/w", &[], &["sh".into()], false);
        assert!(argv.contains(&"-T".into()) && !argv.contains(&"-t".into()));
    }

    #[test]
    fn init_argv_runs_init_then_execs_argv() {
        let argv: Vec<OsString> = vec!["claude".into(), "-p".into(), "do it".into()];
        assert_eq!(init_argv(None, &argv), argv);
        assert_eq!(init_argv(Some("  "), &argv), argv);
        let out = init_argv(Some("source /x && activate"), &argv);
        let out: Vec<_> = out.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            out,
            [
                "bash",
                "-c",
                "source /x && activate\nexec \"$@\"",
                "sulla",
                "claude",
                "-p",
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
    fn decide_reset_kind() {
        use Outcome::*;
        assert_eq!(decide(Exited, 1, 10, true), Reset::None);
        assert_eq!(decide(Exited, 10, 10, false), Reset::None);
        assert_eq!(decide(Merged, 1, 10, true), Reset::Light);
        assert_eq!(decide(Killed, 9, 10, true), Reset::Light);
        assert_eq!(decide(Evicted, 1, 10, true), Reset::Light);
        assert_eq!(decide(Evicted, 10, 10, true), Reset::Hard);
        assert_eq!(decide(Discarded, 1, 10, true), Reset::Hard);
        assert_eq!(decide(Merged, 10, 10, true), Reset::Hard);
        assert_eq!(decide(Killed, 11, 10, true), Reset::Hard);
        assert_eq!(decide(Merged, 100, 0, true), Reset::Light);
        assert_eq!(decide(Merged, 1, 0, false), Reset::Hard);
        assert_eq!(decide(Killed, 1, 10, false), Reset::Hard);
        assert_eq!(decide(Merged, 1, 1, true), Reset::Hard);
    }

    #[test]
    fn light_script_keeps_images() {
        let s = light_script(1000, true);
        assert_eq!(
            s,
            "docker ps -aq | xargs -r docker rm -f && docker network prune -f && docker volume prune -af && { pkill -KILL -u 1000; [ $? -le 1 ]; }"
        );
        let s = light_script(1000, false);
        assert!(!s.contains("volume"));
        assert!(!s.contains("image") && !s.contains("builder") && !s.contains("system prune"));
    }

    #[test]
    fn parse_ipv4_picks_global_inet_on_eth0_for_exact_name() {
        let json = r#"[
          {"name": "sulla-proj-1a2b3c-10", "state": {"network": {"eth0": {"addresses": [
            {"family": "inet", "address": "10.10.0.99", "scope": "global"}
          ]}}}},
          {"name": "sulla-proj-1a2b3c-1", "status": "Running", "state": {"network": {
            "lo": {"addresses": [{"family": "inet", "address": "127.0.0.1", "scope": "local"}]},
            "docker0": {"addresses": [{"family": "inet", "address": "172.17.0.1", "scope": "global"}]},
            "eth0": {"addresses": [
              {"family": "inet6", "address": "fd42::1", "scope": "global"},
              {"family": "inet", "address": "169.254.0.5", "scope": "link"},
              {"family": "inet", "address": "10.10.0.23", "netmask": "24", "scope": "global"},
              {"family": "inet", "address": "10.10.0.24", "scope": "global"}
            ]}
          }}}
        ]"#;
        assert_eq!(
            parse_ipv4(json, "sulla-proj-1a2b3c-1").as_deref(),
            Some("10.10.0.23")
        );
        assert_eq!(
            parse_ipv4(json, "sulla-proj-1a2b3c-10").as_deref(),
            Some("10.10.0.99")
        );
        assert_eq!(parse_ipv4(json, "sulla-proj-1a2b3c-2"), None);
        assert_eq!(
            parse_ipv4(r#"[{"name": "a", "state": {"network": null}}]"#, "a"),
            None
        );
        assert_eq!(parse_ipv4(r#"[{"name": "a", "state": null}]"#, "a"), None);
        assert_eq!(parse_ipv4("not json", "a"), None);
    }

    #[test]
    fn idmap_handles_distinct_gid() {
        assert_eq!(host().idmap(), "uid 1000 1000\ngid 1001 1001");
        let mut h = host();
        h.gid = 1000;
        assert_eq!(h.idmap(), "both 1000 1000");
    }
}
