use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::eyre};
use serde::{Deserialize, Serialize};

mod lifecycle;

use crate::agent;
use crate::config::{BrowseConfig, Config, SandboxKind};
use crate::sandbox::{self, Sandbox};
use lifecycle::Plan;

const BROWSERS: [&str; 3] = ["google-chrome", "chromium", "chromium-browser"];
const CHROMIUM: [&str; 6] = ["chrome", "chromium", "brave", "edge", "vivaldi", "opera"];
const MAX_SLOTS: usize = 254;
const PROBE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    Status(String),
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    Up,
    Partial,
    Down,
}

impl Live {
    pub fn word(self) -> &'static str {
        match self {
            Live::Up => "up",
            Live::Partial => "partial",
            Live::Down => "down",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Live::Up => "▲",
            Live::Partial => "◐",
            Live::Down => "▼",
        }
    }
}

pub fn classify(answered: usize, total: usize) -> Live {
    match answered {
        0 => Live::Down,
        n if n >= total => Live::Up,
        _ => Live::Partial,
    }
}

pub fn probe(slots: &[(usize, String)], ports: &[u16]) -> BTreeMap<usize, Live> {
    std::thread::scope(|s| {
        let pending: Vec<_> = slots
            .iter()
            .map(|(slot, ip)| {
                let checks: Vec<_> = ports
                    .iter()
                    .filter_map(|p| format!("{ip}:{p}").parse::<SocketAddr>().ok())
                    .map(|addr| s.spawn(move || lifecycle::answers(addr, PROBE, PROBE)))
                    .collect();
                (*slot, checks)
            })
            .collect();
        pending
            .into_iter()
            .map(|(slot, checks)| {
                let up = checks
                    .into_iter()
                    .map(|c| c.join().unwrap_or(false))
                    .filter(|ok| *ok)
                    .count();
                (slot, classify(up, ports.len()))
            })
            .collect()
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub bead: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Apps(BTreeMap<usize, Owner>);

impl Apps {
    pub fn started(&mut self, slot: usize, bead: &str) {
        self.0.insert(
            slot,
            Owner {
                bead: bead.to_string(),
                started_at: Utc::now(),
            },
        );
    }

    pub fn stopped(&mut self, slot: usize) {
        self.0.remove(&slot);
    }

    pub fn owner(&self, slot: usize) -> Option<&str> {
        self.0.get(&slot).map(|o| o.bead.as_str())
    }

    pub fn stale(&self, slot: usize, bead: &str) -> Option<&str> {
        self.owner(slot).filter(|o| *o != bead)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn merge(&self, base: &Apps, theirs: &Apps) -> Apps {
        let slots = self.0.keys().chain(base.0.keys()).chain(theirs.0.keys());
        Apps(
            slots
                .filter_map(|s| {
                    let ours = self.0.get(s);
                    let pick = if ours != base.0.get(s) {
                        ours
                    } else {
                        theirs.0.get(s)
                    };
                    pick.map(|o| (*s, o.clone()))
                })
                .collect(),
        )
    }
}

pub fn loopback(slot: usize) -> Option<String> {
    (slot < MAX_SLOTS).then(|| format!("127.0.100.{}", slot + 1))
}

pub fn url(cfg: &BrowseConfig, ip: &str) -> Option<String> {
    match &cfg.url {
        Some(u) => Some(u.replace("{ip}", ip)),
        None => cfg.ports.first().map(|p| format!("http://{ip}:{p}")),
    }
}

fn resolver_rules(hosts: &[String], ip: &str) -> Option<String> {
    (!hosts.is_empty()).then(|| {
        hosts
            .iter()
            .map(|h| format!("MAP {h} {ip}"))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

pub fn profile_dir(repo: &Path, slot: usize) -> PathBuf {
    repo.join(".sulla")
        .join("browser")
        .join(format!("slot-{slot}"))
}

fn browser_args(profile: &Path, cfg: &BrowseConfig, ip: &str) -> Vec<OsString> {
    let mut user_data = OsString::from("--user-data-dir=");
    user_data.push(profile);
    let mut out = vec![user_data];
    if let Some(rules) = resolver_rules(&cfg.hosts, ip) {
        out.push(format!("--host-resolver-rules={rules}").into());
    }
    out.push("--no-first-run".into());
    out.extend(url(cfg, ip).map(OsString::from));
    out
}

#[derive(Debug, PartialEq, Eq)]
enum Browser {
    Chromium(PathBuf),
    Default,
}

fn pick(
    over: Option<OsString>,
    mapped: bool,
    default: impl FnOnce() -> Option<PathBuf>,
    path: Option<OsString>,
) -> Option<Browser> {
    if let Some(b) = over.filter(|b| !b.is_empty()) {
        return Some(Browser::Chromium(b.into()));
    }
    if !mapped {
        return Some(Browser::Default);
    }
    default()
        .filter(|b| is_chromium(b))
        .or_else(|| find_on_path(path))
        .map(Browser::Chromium)
}

fn is_chromium(bin: &Path) -> bool {
    let name = bin
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    CHROMIUM.iter().any(|c| name.contains(c))
}

fn find_on_path(path: Option<OsString>) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = path.map(|p| std::env::split_paths(&p).collect())?;
    BROWSERS
        .iter()
        .flat_map(|b| dirs.iter().map(move |d| d.join(b)))
        .find(|p| p.is_file())
}

fn default_browser() -> Option<PathBuf> {
    let out = Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || id.is_empty() {
        return None;
    }
    desktop_exec(&id, &data_dirs())
}

fn data_dirs() -> Vec<PathBuf> {
    let set = |v: &str| std::env::var_os(v).filter(|v| !v.is_empty());
    let home = set("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let rest = set("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(std::env::split_paths(&rest))
        .collect()
}

fn desktop_exec(id: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let body = dirs
        .iter()
        .find_map(|d| std::fs::read_to_string(d.join("applications").join(id)).ok())?;
    body.lines()
        .find_map(|l| l.strip_prefix("Exec="))?
        .split_whitespace()
        .find(|t| *t != "env" && !t.contains('='))
        .map(PathBuf::from)
}

fn spawn(cmd: &mut Command) -> Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| eyre!("starting {}: {e}", cmd.get_program().to_string_lossy()))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

pub fn supported(cfg: &Config) -> Result<()> {
    if cfg.sandbox != SandboxKind::Incus {
        return Err(eyre!("browse needs sandbox = \"incus\""));
    }
    if cfg.incus.browse.ports.is_empty() {
        return Err(eyre!("browse needs [incus.browse] ports"));
    }
    Ok(())
}

pub fn launch(cfg: &Config, repo: &Path, slot: usize) -> Result<String> {
    supported(cfg)?;
    let browse = &cfg.incus.browse;
    let ip = loopback(slot).ok_or_else(|| eyre!("slot {slot} has no loopback address"))?;
    let target = url(browse, &ip).unwrap_or_else(|| ip.clone());
    let browser = pick(
        std::env::var_os("SULLA_BROWSER"),
        !browse.hosts.is_empty(),
        default_browser,
        std::env::var_os("PATH"),
    )
    .ok_or_else(|| {
        eyre!(
            "mapping [incus.browse] hosts needs a Chromium-based browser, set SULLA_BROWSER or install one of {}",
            BROWSERS.join(", ")
        )
    })?;
    match browser {
        Browser::Default => {
            spawn(Command::new("xdg-open").arg(&target))?;
            Ok(format!(
                "opened {target} for slot {slot} in the default browser"
            ))
        }
        Browser::Chromium(bin) => {
            let profile = profile_dir(repo, slot);
            std::fs::create_dir_all(&profile)?;
            spawn(Command::new(&bin).args(browser_args(&profile, browse, &ip)))?;
            Ok(format!(
                "opened {target} for slot {slot} in {}",
                bin.display()
            ))
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SlotFile {
    pid: u32,
    slots: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Apps::is_empty")]
    apps: Apps,
}

fn slots_path(repo: &Path) -> PathBuf {
    repo.join(".sulla").join("slots.json")
}

fn write_file(repo: &Path, file: &SlotFile) -> Result<()> {
    let path = slots_path(repo);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(file)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn write_slots(repo: &Path, slots: &BTreeMap<String, usize>, apps: &Apps) -> Result<()> {
    write_file(
        repo,
        &SlotFile {
            pid: std::process::id(),
            slots: slots.clone(),
            apps: apps.clone(),
        },
    )
}

fn read_file(repo: &Path) -> Option<SlotFile> {
    std::fs::read_to_string(slots_path(repo))
        .ok()
        .and_then(|s| serde_json::from_str::<SlotFile>(&s).ok())
}

fn read_live(repo: &Path) -> Option<SlotFile> {
    read_file(repo).filter(|f| Path::new(&format!("/proc/{}", f.pid)).exists())
}

pub fn read_own_apps(repo: &Path) -> Apps {
    read_file(repo)
        .filter(|f| f.pid == std::process::id())
        .map(|f| f.apps)
        .unwrap_or_default()
}

fn record(repo: &Path, f: impl FnOnce(&mut Apps)) -> Result<()> {
    let Some(mut file) = read_live(repo) else {
        return Ok(());
    };
    f(&mut file.apps);
    write_file(repo, &file)
}

pub fn resolve(target: &str, slots: &BTreeMap<String, usize>) -> Result<(Option<String>, usize)> {
    if let Ok(n) = target.parse::<usize>() {
        let bead = slots
            .iter()
            .filter(|(_, s)| **s == n)
            .map(|(id, _)| agent::reviewed_bead(id).unwrap_or(id).to_string())
            .next();
        return Ok((bead, n));
    }
    slots
        .get(target)
        .or_else(|| slots.get(&agent::review_id(target)))
        .map(|s| (Some(target.to_string()), *s))
        .ok_or_else(|| {
            eyre!("{target} has no slot, run :browse {target} in the sulla TUI to resume it")
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Open,
    Stop,
    Restart,
}

pub struct Job {
    pub label: String,
    pub bead: Option<String>,
    pub slot: usize,
    kind: Kind,
    plan: Option<Plan>,
    cfg: Config,
    repo: PathBuf,
}

pub fn needs_sandbox(cfg: &Config, kind: Kind) -> bool {
    kind != Kind::Open || cfg.incus.browse.up_script().is_some()
}

pub fn job(
    cfg: &Config,
    repo: &Path,
    target: &str,
    slots: &BTreeMap<String, usize>,
    kind: Kind,
    sandbox: Option<&dyn Sandbox>,
) -> Result<Job> {
    supported(cfg)?;
    let (bead, slot) = resolve(target, slots)?;
    let browse = &cfg.incus.browse;
    if kind == Kind::Stop && browse.down_script().is_none() {
        return Err(eyre!("browse stop needs [incus.browse] down"));
    }
    if kind == Kind::Restart && browse.up_script().is_none() {
        return Err(eyre!("browse restart needs [incus.browse] up"));
    }
    let plan = if needs_sandbox(cfg, kind) {
        let bead = bead
            .clone()
            .ok_or_else(|| eyre!("slot {slot} has no bead, press A on one to resume it"))?;
        let sandbox = sandbox.ok_or_else(|| eyre!("browse needs the incus sandbox"))?;
        Some(lifecycle::plan(browse, repo, &bead, slot, sandbox)?)
    } else {
        None
    };
    Ok(Job {
        label: bead.clone().unwrap_or_else(|| format!("slot {slot}")),
        bead,
        slot,
        kind,
        plan,
        cfg: cfg.clone(),
        repo: repo.to_path_buf(),
    })
}

impl Job {
    pub fn stale<'a>(&self, apps: &'a Apps) -> Option<&'a str> {
        if self.kind != Kind::Open || self.plan.is_none() {
            return None;
        }
        apps.stale(self.slot, self.bead.as_deref()?)
    }

    pub fn restart_question(&self, other: &str) -> String {
        format!(
            "slot {} is serving {other}; restart app for {}?",
            self.slot, self.label
        )
    }

    pub fn run(&self, note: &mut dyn FnMut(Note)) -> Result<String> {
        match (&self.plan, self.kind) {
            (Some(plan), Kind::Stop) => {
                plan.stop(note)?;
                Ok(format!("stopped the app for slot {}", self.slot))
            }
            (Some(plan), Kind::Restart) => {
                if plan.has_down() {
                    plan.stop(note)?;
                }
                plan.ensure_up(true, note)?;
                launch(&self.cfg, &self.repo, self.slot)
            }
            (plan, _) => {
                if let Some(plan) = plan {
                    plan.ensure_up(false, note)?;
                }
                launch(&self.cfg, &self.repo, self.slot)
            }
        }
    }
}

pub fn run(cfg: &Config, repo: &Path, target: &str, kind: Kind) -> Result<()> {
    supported(cfg)?;
    let file = read_live(repo).unwrap_or_default();
    resolve(target, &file.slots)?;
    let sandbox = needs_sandbox(cfg, kind)
        .then(|| sandbox::build(cfg, repo))
        .transpose()?;
    let job = job(cfg, repo, target, &file.slots, kind, sandbox.as_deref())?;
    let label = job.label.clone();
    if let Some(other) = job.stale(&file.apps) {
        return Err(eyre!(
            "browse {label}: slot {} is serving {other}, run `sulla browse {target} --restart` to restart its app for {label}",
            job.slot
        ));
    }
    let (slot, bead) = (job.slot, job.bead.clone().unwrap_or_default());
    let msg = job
        .run(&mut |n| match n {
            Note::Status(m) => eprintln!("browse {label}: {m}"),
            Note::Up => warn_record(record(repo, |a| a.started(slot, &bead))),
            Note::Down => warn_record(record(repo, |a| a.stopped(slot))),
        })
        .map_err(|e| eyre!("browse {label}: {e}"))?;
    println!("{msg}");
    Ok(())
}

fn warn_record(r: Result<()>) {
    if let Err(e) = r {
        eprintln!("recording the slot app in .sulla/slots.json failed: {e:#}");
    }
}

fn app_cell(live: Option<Live>, owner: Option<&str>) -> String {
    match (live, owner) {
        (None, _) => "-".into(),
        (Some(l), None) => l.word().into(),
        (Some(l), Some(b)) => format!("{} for {b}", l.word()),
    }
}

fn status_lines(
    cfg: &Config,
    repo: &Path,
    slots: &BTreeMap<String, usize>,
    apps: &Apps,
    live: &BTreeMap<usize, Live>,
) -> Vec<String> {
    let count = slot_count(cfg, slots);
    let incus = cfg.sandbox == SandboxKind::Incus;
    let browse = incus && !cfg.incus.browse.ports.is_empty();
    let rows: Vec<[String; 6]> = (0..count)
        .map(|slot| {
            let beads: Vec<&str> = slots
                .iter()
                .filter(|(_, s)| **s == slot)
                .map(|(id, _)| id.as_str())
                .collect();
            let ip = loopback(slot).filter(|_| browse);
            [
                format!("slot {slot}"),
                if incus {
                    sandbox::container_name(&cfg.incus.prefix, repo, slot)
                } else {
                    "-".into()
                },
                ip.clone().unwrap_or_else(|| "-".into()),
                if beads.is_empty() {
                    "idle".into()
                } else {
                    beads.join(", ")
                },
                app_cell(live.get(&slot).copied(), apps.owner(slot)),
                ip.and_then(|ip| url(&cfg.incus.browse, &ip))
                    .unwrap_or_default(),
            ]
        })
        .collect();
    let width = |i: usize| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0);
    let widths = [width(0), width(1), width(2), width(3), width(4)];
    rows.iter()
        .map(|r| {
            let mut line = String::new();
            for (cell, w) in r.iter().zip(widths) {
                line.push_str(&format!("{cell:<w$}  "));
            }
            line.push_str(&r[5]);
            line.trim_end().to_string()
        })
        .collect()
}

fn slot_count(cfg: &Config, slots: &BTreeMap<String, usize>) -> usize {
    slots
        .values()
        .map(|s| s + 1)
        .max()
        .unwrap_or(0)
        .max(cfg.max_agents)
}

pub fn status(cfg: &Config, repo: &Path) -> Result<()> {
    let file = read_live(repo).unwrap_or_default();
    let browse = &cfg.incus.browse;
    let targets: Vec<(usize, String)> = if cfg.sandbox == SandboxKind::Incus {
        (0..slot_count(cfg, &file.slots))
            .filter_map(|s| loopback(s).map(|ip| (s, ip)))
            .collect()
    } else {
        Vec::new()
    };
    let live = if browse.ports.is_empty() {
        BTreeMap::new()
    } else {
        probe(&targets, &browse.ports)
    };
    for line in status_lines(cfg, repo, &file.slots, &file.apps, &live) {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memfault() -> BrowseConfig {
        BrowseConfig {
            ports: vec![8000, 8001, 8002],
            hosts: vec!["*.memfault.test".into()],
            url: Some("http://app.memfault.test:8000".into()),
            ..BrowseConfig::default()
        }
    }

    #[test]
    fn loopback_skips_zero_and_stays_in_range() {
        assert_eq!(loopback(0).as_deref(), Some("127.0.100.1"));
        assert_eq!(loopback(2).as_deref(), Some("127.0.100.3"));
        assert_eq!(loopback(253).as_deref(), Some("127.0.100.254"));
        assert_eq!(loopback(254), None);
    }

    #[test]
    fn browser_args_map_hosts_to_slot_ip() {
        let args = browser_args(
            Path::new("/r/.sulla/browser/slot-1"),
            &memfault(),
            "127.0.100.2",
        );
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user-data-dir=/r/.sulla/browser/slot-1",
                "--host-resolver-rules=MAP *.memfault.test 127.0.100.2",
                "--no-first-run",
                "http://app.memfault.test:8000",
            ]
        );
        let cfg = BrowseConfig {
            ports: vec![3000],
            hosts: vec!["a.test".into(), "*.b.test".into()],
            url: None,
            ..BrowseConfig::default()
        };
        let args = browser_args(Path::new("/p"), &cfg, "127.0.100.1");
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user-data-dir=/p",
                "--host-resolver-rules=MAP a.test 127.0.100.1, MAP *.b.test 127.0.100.1",
                "--no-first-run",
                "http://127.0.100.1:3000",
            ]
        );
    }

    #[test]
    fn url_substitutes_ip_and_defaults_to_first_port() {
        let mut cfg = BrowseConfig {
            url: Some("http://{ip}:9000/x".into()),
            ..BrowseConfig::default()
        };
        assert_eq!(
            url(&cfg, "127.0.100.4").as_deref(),
            Some("http://127.0.100.4:9000/x")
        );
        cfg.url = None;
        assert_eq!(url(&cfg, "127.0.100.4"), None);
        cfg.ports = vec![8080, 9090];
        assert_eq!(
            url(&cfg, "127.0.100.4").as_deref(),
            Some("http://127.0.100.4:8080")
        );
    }

    #[test]
    fn find_on_path_follows_candidate_order() {
        let d = std::env::temp_dir().join(format!("sulla-browse-{}", std::process::id()));
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("chromium"), "").unwrap();
        std::fs::write(a.join("chromium-browser"), "").unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(find_on_path(Some(path.clone())), Some(b.join("chromium")));
        std::fs::write(a.join("google-chrome"), "").unwrap();
        assert_eq!(find_on_path(Some(path)), Some(a.join("google-chrome")));
        assert_eq!(find_on_path(Some(d.join("none").into())), None);
        assert_eq!(find_on_path(None), None);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn pick_uses_default_browser_unless_hosts_need_chromium() {
        let d = std::env::temp_dir().join(format!("sulla-pick-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("chromium"), "").unwrap();
        let path = Some(d.clone().into_os_string());
        let firefox = || Some(PathBuf::from("/snap/bin/firefox"));
        let chrome = || Some(PathBuf::from("/usr/bin/google-chrome-stable"));
        let chromium = |p: &str| Some(Browser::Chromium(p.into()));
        assert_eq!(
            pick(Some("/opt/brave".into()), false, firefox, path.clone()),
            chromium("/opt/brave")
        );
        assert_eq!(
            pick(Some("".into()), false, firefox, path.clone()),
            Some(Browser::Default)
        );
        assert_eq!(pick(None, false, || None, None), Some(Browser::Default));
        assert_eq!(
            pick(None, true, chrome, path.clone()),
            chromium("/usr/bin/google-chrome-stable")
        );
        assert_eq!(
            pick(None, true, firefox, path.clone()),
            chromium(d.join("chromium").to_str().unwrap())
        );
        assert_eq!(pick(None, true, firefox, None), None);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn is_chromium_matches_chromium_family_binaries() {
        for b in [
            "/usr/bin/google-chrome-stable",
            "/snap/bin/chromium",
            "brave-browser",
            "/usr/bin/microsoft-edge-stable",
            "vivaldi-stable",
        ] {
            assert!(is_chromium(Path::new(b)), "{b}");
        }
        assert!(!is_chromium(Path::new("/snap/bin/firefox")));
        assert!(!is_chromium(Path::new("/usr/bin/flatpak")));
    }

    #[test]
    fn desktop_exec_reads_first_exec_binary() {
        let d = std::env::temp_dir().join(format!("sulla-desktop-{}", std::process::id()));
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(a.join("applications")).unwrap();
        std::fs::create_dir_all(b.join("applications")).unwrap();
        std::fs::write(
            b.join("applications/chromium_chromium.desktop"),
            "[Desktop Entry]\nName=Chromium\nExec=env BAMF_DESKTOP_FILE_HINT=/x.desktop /snap/bin/chromium %U\n[Desktop Action new]\nExec=/other\n",
        )
        .unwrap();
        std::fs::write(
            a.join("applications/google-chrome.desktop"),
            "[Desktop Entry]\nExec=/usr/bin/google-chrome-stable %U\n",
        )
        .unwrap();
        let dirs = [a, b];
        assert_eq!(
            desktop_exec("chromium_chromium.desktop", &dirs),
            Some(PathBuf::from("/snap/bin/chromium"))
        );
        assert_eq!(
            desktop_exec("google-chrome.desktop", &dirs),
            Some(PathBuf::from("/usr/bin/google-chrome-stable"))
        );
        assert_eq!(desktop_exec("firefox.desktop", &dirs), None);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn resolve_takes_slot_number_or_bead_or_its_reviewer() {
        let slots = BTreeMap::from([
            ("a-1".to_string(), 0),
            ("a-1~review".to_string(), 0),
            ("b-2~review".to_string(), 2),
        ]);
        let some = |b: &str, s| (Some(b.to_string()), s);
        assert_eq!(resolve("3", &slots).unwrap(), (None, 3));
        assert_eq!(resolve("0", &slots).unwrap(), some("a-1", 0));
        assert_eq!(resolve("2", &slots).unwrap(), some("b-2", 2));
        assert_eq!(resolve("a-1", &slots).unwrap(), some("a-1", 0));
        assert_eq!(resolve("b-2", &slots).unwrap(), some("b-2", 2));
        let err = resolve("c-3", &slots).unwrap_err().to_string();
        assert_eq!(
            err,
            "c-3 has no slot, run :browse c-3 in the sulla TUI to resume it"
        );
    }

    fn incus(browse: BrowseConfig) -> Config {
        let mut cfg = Config {
            sandbox: SandboxKind::Incus,
            ..Config::default()
        };
        cfg.incus.browse = browse;
        cfg
    }

    #[test]
    fn job_without_up_opens_the_slot_as_before() {
        let repo = Path::new("/r");
        let slots = BTreeMap::from([("a-1".to_string(), 0)]);
        let cfg = incus(memfault());
        let j = job(&cfg, repo, "a-1", &slots, Kind::Open, None).unwrap();
        assert!(j.plan.is_none());
        assert_eq!((j.label.as_str(), j.slot), ("a-1", 0));
        let j = job(&cfg, repo, "5", &slots, Kind::Open, None).unwrap();
        assert!(j.plan.is_none());
        assert_eq!((j.label.as_str(), j.slot), ("slot 5", 5));
        assert!(!needs_sandbox(&cfg, Kind::Open));
        let err = job(&cfg, repo, "b-2", &slots, Kind::Open, None)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            err,
            "b-2 has no slot, run :browse b-2 in the sulla TUI to resume it"
        );
        let err = job(&cfg, repo, "a-1", &slots, Kind::Stop, None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("needs [incus.browse] down"), "{err}");
        let mut worktree = cfg.clone();
        worktree.sandbox = SandboxKind::Worktree;
        let err = job(&worktree, repo, "a-1", &slots, Kind::Open, None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("sandbox = \"incus\""), "{err}");
    }

    #[test]
    fn job_with_up_plans_for_the_bead_holding_the_slot() {
        let repo = Path::new("/r");
        let slots = BTreeMap::from([("a-1~review".to_string(), 1)]);
        let cfg = incus(BrowseConfig {
            up: Some("make up".into()),
            down: Some("make down".into()),
            ..memfault()
        });
        assert!(needs_sandbox(&cfg, Kind::Open));
        let sb = sandbox::Worktree;
        let j = job(&cfg, repo, "1", &slots, Kind::Open, Some(&sb)).unwrap();
        assert!(j.plan.is_some());
        assert_eq!((j.label.as_str(), j.slot), ("a-1", 1));
        assert!(job(&cfg, repo, "a-1", &slots, Kind::Stop, Some(&sb)).is_ok());
        let err = job(&cfg, repo, "4", &slots, Kind::Open, Some(&sb))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("slot 4 has no bead"), "{err}");
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sulla-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn slot_file_round_trips_only_while_owner_lives() {
        let d = scratch("slots");
        assert!(read_live(&d).is_none());
        let slots = BTreeMap::from([("a-1".to_string(), 1)]);
        let mut apps = Apps::default();
        apps.started(1, "a-1");
        write_slots(&d, &slots, &apps).unwrap();
        assert_eq!(read_live(&d).unwrap().slots, slots);
        assert_eq!(read_own_apps(&d), apps);
        let dead = SlotFile {
            pid: u32::MAX,
            slots: slots.clone(),
            apps: apps.clone(),
        };
        std::fs::write(slots_path(&d), serde_json::to_string(&dead).unwrap()).unwrap();
        assert!(read_live(&d).is_none());
        assert!(read_own_apps(&d).is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn slot_file_without_apps_still_reads() {
        let d = scratch("slots-old");
        std::fs::create_dir_all(d.join(".sulla")).unwrap();
        let old = format!(
            "{{\"pid\": {}, \"slots\": {{\"a-1\": 0}}}}",
            std::process::id()
        );
        std::fs::write(slots_path(&d), old).unwrap();
        let file = read_live(&d).unwrap();
        assert_eq!(file.slots, BTreeMap::from([("a-1".to_string(), 0)]));
        assert!(file.apps.is_empty());
        write_slots(&d, &file.slots, &Apps::default()).unwrap();
        let body = std::fs::read_to_string(slots_path(&d)).unwrap();
        assert!(!body.contains("apps"), "{body}");
        let mut apps = Apps::default();
        apps.started(0, "a-1");
        write_slots(&d, &file.slots, &apps).unwrap();
        let body = std::fs::read_to_string(slots_path(&d)).unwrap();
        assert!(body.contains("\"apps\""), "{body}");
        assert!(body.contains("\"started_at\""), "{body}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn record_updates_apps_of_the_running_sulla_only() {
        let d = scratch("record");
        record(&d, |a| a.started(0, "a-1")).unwrap();
        assert!(read_file(&d).is_none());
        let slots = BTreeMap::from([("a-1".to_string(), 0)]);
        write_slots(&d, &slots, &Apps::default()).unwrap();
        record(&d, |a| a.started(0, "a-1")).unwrap();
        let file = read_live(&d).unwrap();
        assert_eq!(file.slots, slots);
        assert_eq!(file.apps.owner(0), Some("a-1"));
        record(&d, |a| a.stopped(0)).unwrap();
        assert!(read_live(&d).unwrap().apps.is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn apps_track_the_bead_each_slot_was_started_for() {
        let mut apps = Apps::default();
        assert_eq!(apps.owner(1), None);
        assert_eq!(apps.stale(1, "a-1"), None);
        apps.started(1, "b-2");
        assert_eq!(apps.owner(1), Some("b-2"));
        assert_eq!(apps.stale(1, "a-1"), Some("b-2"));
        assert_eq!(apps.stale(1, "b-2"), None);
        assert_eq!(apps.stale(0, "a-1"), None);
        apps.started(1, "a-1");
        assert_eq!(apps.stale(1, "a-1"), None);
        apps.stopped(1);
        assert_eq!(apps.owner(1), None);
        assert!(apps.is_empty());
    }

    #[test]
    fn merge_keeps_our_changes_and_takes_theirs_elsewhere() {
        let mut base = Apps::default();
        base.started(0, "a-1");
        base.started(1, "b-2");
        let mut ours = base.clone();
        ours.stopped(0);
        let mut theirs = base.clone();
        theirs.started(1, "c-3");
        theirs.started(2, "d-4");
        theirs.started(0, "e-5");
        let m = ours.merge(&base, &theirs);
        assert_eq!(m.owner(0), None);
        assert_eq!(m.owner(1), Some("c-3"));
        assert_eq!(m.owner(2), Some("d-4"));
        assert_eq!(base.merge(&base, &Apps::default()), Apps::default());
    }

    #[test]
    fn probe_classifies_all_some_or_no_ports_answering() {
        assert_eq!(classify(3, 3), Live::Up);
        assert_eq!(classify(1, 3), Live::Partial);
        assert_eq!(classify(0, 3), Live::Down);
        let held = std::net::TcpListener::bind("127.0.100.249:0").unwrap();
        let open = held.local_addr().unwrap().port();
        let closed = {
            let l = std::net::TcpListener::bind("127.0.100.249:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let keep = std::thread::spawn(move || {
            let mut conns = Vec::new();
            for _ in 0..2 {
                conns.push(held.accept().unwrap());
            }
            std::thread::sleep(Duration::from_millis(500));
        });
        let ip = "127.0.100.249".to_string();
        let live = probe(&[(0, ip.clone())], &[open]);
        assert_eq!(live, BTreeMap::from([(0, Live::Up)]));
        let live = probe(
            &[(0, ip.clone()), (1, "127.0.100.248".into())],
            &[open, closed],
        );
        assert_eq!(live, BTreeMap::from([(0, Live::Partial), (1, Live::Down)]));
        keep.join().unwrap();
    }

    #[test]
    fn stale_only_guards_opening_another_beads_app() {
        let repo = Path::new("/r");
        let slots = BTreeMap::from([("a-1~review".to_string(), 1)]);
        let cfg = incus(BrowseConfig {
            up: Some("make up".into()),
            ..memfault()
        });
        let sb = sandbox::Worktree;
        let open = job(&cfg, repo, "a-1", &slots, Kind::Open, Some(&sb)).unwrap();
        let mut apps = Apps::default();
        assert_eq!(open.stale(&apps), None);
        apps.started(1, "a-1");
        assert_eq!(open.stale(&apps), None);
        apps.started(1, "b-2");
        assert_eq!(open.stale(&apps), Some("b-2"));
        assert_eq!(
            open.restart_question("b-2"),
            "slot 1 is serving b-2; restart app for a-1?"
        );
        let restart = job(&cfg, repo, "a-1", &slots, Kind::Restart, Some(&sb)).unwrap();
        assert_eq!(restart.stale(&apps), None);
        let no_up = incus(memfault());
        let plain = job(&no_up, repo, "a-1", &slots, Kind::Open, None).unwrap();
        assert_eq!(plain.stale(&apps), None);
        let err = job(&no_up, repo, "a-1", &slots, Kind::Restart, Some(&sb))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("restart needs [incus.browse] up"), "{err}");
    }

    #[test]
    fn restart_runs_down_then_up_for_the_bead() {
        let d = scratch("restart");
        let wt = crate::git::worktree_dir(&d, "a-1");
        std::fs::create_dir_all(&wt).unwrap();
        let slots = BTreeMap::from([("a-1".to_string(), 0)]);
        let mut cfg = incus(BrowseConfig {
            ports: vec![1],
            up: Some("true".into()),
            down: Some("true".into()),
            ready: Some("false".into()),
            ready_timeout_secs: 1,
            ..BrowseConfig::default()
        });
        let sb = sandbox::Worktree;
        let j = job(&cfg, &d, "a-1", &slots, Kind::Restart, Some(&sb)).unwrap();
        let mut notes = Vec::new();
        let err = j.run(&mut |n| notes.push(n)).unwrap_err().to_string();
        assert!(err.starts_with("app was not ready within 1s"), "{err}");
        assert_eq!(
            notes[..3],
            [
                Note::Status("stopping app…".into()),
                Note::Down,
                Note::Status("starting app…".into())
            ]
        );
        assert_eq!(notes.get(3), Some(&Note::Up));
        cfg.incus.browse.down = None;
        let j = job(&cfg, &d, "a-1", &slots, Kind::Restart, Some(&sb)).unwrap();
        let mut notes = Vec::new();
        let _ = j.run(&mut |n| notes.push(n));
        assert_eq!(notes[..2], [Note::Status("starting app…".into()), Note::Up]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn status_lists_each_slot_with_ip_beads_and_app() {
        let mut cfg = Config {
            max_agents: 2,
            sandbox: SandboxKind::Incus,
            ..Config::default()
        };
        cfg.incus.browse = memfault();
        let repo = Path::new("/home/blake/git/memfault");
        let name = |s| sandbox::container_name("sulla", repo, s);
        let slots = BTreeMap::from([
            ("a-1".to_string(), 0),
            ("a-1~review".to_string(), 0),
            ("c-3".to_string(), 2),
        ]);
        let mut apps = Apps::default();
        apps.started(0, "a-1");
        apps.started(2, "b-2");
        let live = BTreeMap::from([(0, Live::Up), (1, Live::Down), (2, Live::Partial)]);
        let lines = status_lines(&cfg, repo, &slots, &apps, &live);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            format!(
                "slot 0  {}  127.0.100.1  a-1, a-1~review  up for a-1       http://app.memfault.test:8000",
                name(0)
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "slot 1  {}  127.0.100.2  idle             down             http://app.memfault.test:8000",
                name(1)
            )
        );
        assert!(
            lines[2].contains("127.0.100.3  c-3              partial for b-2"),
            "{}",
            lines[2]
        );

        cfg.sandbox = SandboxKind::Worktree;
        let lines = status_lines(
            &cfg,
            repo,
            &BTreeMap::new(),
            &Apps::default(),
            &BTreeMap::new(),
        );
        assert_eq!(lines, ["slot 0  -  -  idle  -", "slot 1  -  -  idle  -"]);
    }
}
