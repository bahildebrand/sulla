use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use color_eyre::{Result, eyre::eyre};
use serde::{Deserialize, Serialize};

use crate::agent;
use crate::config::{BrowseConfig, Config, SandboxKind};
use crate::sandbox;

const BROWSERS: [&str; 3] = ["google-chrome", "chromium", "chromium-browser"];
const MAX_SLOTS: usize = 254;

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

fn find_browser(over: Option<OsString>, path: Option<OsString>) -> Option<PathBuf> {
    if let Some(b) = over.filter(|b| !b.is_empty()) {
        return Some(b.into());
    }
    let dirs: Vec<PathBuf> = path.map(|p| std::env::split_paths(&p).collect())?;
    BROWSERS
        .iter()
        .flat_map(|b| dirs.iter().map(move |d| d.join(b)))
        .find(|p| p.is_file())
}

pub fn launch(cfg: &Config, repo: &Path, slot: usize) -> Result<String> {
    if cfg.sandbox != SandboxKind::Incus {
        return Err(eyre!("browse needs sandbox = \"incus\""));
    }
    let browse = &cfg.incus.browse;
    if browse.ports.is_empty() {
        return Err(eyre!("browse needs [incus.browse] ports"));
    }
    let ip = loopback(slot).ok_or_else(|| eyre!("slot {slot} has no loopback address"))?;
    let bin = find_browser(std::env::var_os("SULLA_BROWSER"), std::env::var_os("PATH"))
        .ok_or_else(|| {
            eyre!(
                "no browser found, set SULLA_BROWSER or install one of {}",
                BROWSERS.join(", ")
            )
        })?;
    let profile = profile_dir(repo, slot);
    std::fs::create_dir_all(&profile)?;
    let mut child = Command::new(&bin)
        .args(browser_args(&profile, browse, &ip))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| eyre!("starting {}: {e}", bin.display()))?;
    std::thread::spawn(move || child.wait());
    let target = url(browse, &ip).unwrap_or(ip);
    Ok(format!("opened {target} for slot {slot}"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SlotFile {
    pid: u32,
    slots: BTreeMap<String, usize>,
}

fn slots_path(repo: &Path) -> PathBuf {
    repo.join(".sulla").join("slots.json")
}

pub fn write_slots(repo: &Path, slots: &BTreeMap<String, usize>) -> Result<()> {
    let path = slots_path(repo);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = SlotFile {
        pid: std::process::id(),
        slots: slots.clone(),
    };
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&file)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_slots(repo: &Path) -> BTreeMap<String, usize> {
    std::fs::read_to_string(slots_path(repo))
        .ok()
        .and_then(|s| serde_json::from_str::<SlotFile>(&s).ok())
        .filter(|f| Path::new(&format!("/proc/{}", f.pid)).exists())
        .map(|f| f.slots)
        .unwrap_or_default()
}

pub fn resolve(target: &str, slots: &BTreeMap<String, usize>) -> Result<usize> {
    if let Ok(n) = target.parse() {
        return Ok(n);
    }
    slots
        .get(target)
        .or_else(|| slots.get(&agent::review_id(target)))
        .copied()
        .ok_or_else(|| eyre!("{target} has no slot, is sulla running it?"))
}

pub fn run(cfg: &Config, repo: &Path, target: &str) -> Result<()> {
    let slot = resolve(target, &read_slots(repo))?;
    println!("{}", launch(cfg, repo, slot)?);
    Ok(())
}

fn status_lines(cfg: &Config, repo: &Path, slots: &BTreeMap<String, usize>) -> Vec<String> {
    let count = slots
        .values()
        .map(|s| s + 1)
        .max()
        .unwrap_or(0)
        .max(cfg.max_agents);
    let incus = cfg.sandbox == SandboxKind::Incus;
    let browse = incus && !cfg.incus.browse.ports.is_empty();
    let rows: Vec<[String; 5]> = (0..count)
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
                ip.and_then(|ip| url(&cfg.incus.browse, &ip))
                    .unwrap_or_default(),
            ]
        })
        .collect();
    let width = |i: usize| rows.iter().map(|r| r[i].len()).max().unwrap_or(0);
    let widths = [width(0), width(1), width(2), width(3)];
    rows.iter()
        .map(|r| {
            let mut line = String::new();
            for (cell, w) in r.iter().zip(widths) {
                line.push_str(&format!("{cell:<w$}  "));
            }
            line.push_str(&r[4]);
            line.trim_end().to_string()
        })
        .collect()
}

pub fn status(cfg: &Config, repo: &Path) -> Result<()> {
    for line in status_lines(cfg, repo, &read_slots(repo)) {
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
    fn find_browser_prefers_env_then_path_order() {
        let d = std::env::temp_dir().join(format!("sulla-browse-{}", std::process::id()));
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("chromium"), "").unwrap();
        std::fs::write(a.join("chromium-browser"), "").unwrap();
        let path = std::env::join_paths([&a, &b]).unwrap();
        assert_eq!(
            find_browser(Some("/opt/chrome".into()), Some(path.clone())),
            Some(PathBuf::from("/opt/chrome"))
        );
        assert_eq!(
            find_browser(Some("".into()), Some(path.clone())),
            Some(b.join("chromium"))
        );
        std::fs::write(a.join("google-chrome"), "").unwrap();
        assert_eq!(
            find_browser(None, Some(path)),
            Some(a.join("google-chrome"))
        );
        assert_eq!(find_browser(None, Some(d.join("none").into())), None);
        assert_eq!(find_browser(None, None), None);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn resolve_takes_slot_number_or_bead_or_its_reviewer() {
        let slots = BTreeMap::from([("a-1".to_string(), 0), ("b-2~review".to_string(), 2)]);
        assert_eq!(resolve("3", &slots).unwrap(), 3);
        assert_eq!(resolve("a-1", &slots).unwrap(), 0);
        assert_eq!(resolve("b-2", &slots).unwrap(), 2);
        let err = resolve("c-3", &slots).unwrap_err().to_string();
        assert!(err.contains("c-3 has no slot"), "{err}");
    }

    #[test]
    fn slot_file_round_trips_only_while_owner_lives() {
        let d = std::env::temp_dir().join(format!("sulla-slots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(read_slots(&d).is_empty());
        let slots = BTreeMap::from([("a-1".to_string(), 1)]);
        write_slots(&d, &slots).unwrap();
        assert_eq!(read_slots(&d), slots);
        let dead = SlotFile {
            pid: u32::MAX,
            slots: slots.clone(),
        };
        std::fs::write(slots_path(&d), serde_json::to_string(&dead).unwrap()).unwrap();
        assert!(read_slots(&d).is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn status_lists_each_slot_with_ip_and_beads() {
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
        let lines = status_lines(&cfg, repo, &slots);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            format!(
                "slot 0  {}  127.0.100.1  a-1, a-1~review  http://app.memfault.test:8000",
                name(0)
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "slot 1  {}  127.0.100.2  idle             http://app.memfault.test:8000",
                name(1)
            )
        );
        assert!(lines[2].contains("127.0.100.3  c-3"), "{}", lines[2]);

        cfg.sandbox = SandboxKind::Worktree;
        let lines = status_lines(&cfg, repo, &BTreeMap::new());
        assert_eq!(lines, ["slot 0  -  -  idle", "slot 1  -  -  idle"]);
    }
}
