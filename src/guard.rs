use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use color_eyre::{Result, eyre::eyre};
use serde_json::{Value, json};

const WRITE_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
const SEARCH_TOOLS: [&str; 2] = ["Grep", "Glob"];
const SECRETS: [&str; 5] = [".ssh", ".gnupg", ".aws", ".config/gh", ".netrc"];
const MAX_LINKS: usize = 40;

pub fn hooks(exe: &Path, root: &Path) -> Value {
    let command = format!(
        "{} guard {} || exit 2",
        quote(&exe.to_string_lossy()),
        quote(&root.to_string_lossy())
    );
    let mut tools: Vec<&str> = vec!["Read"];
    tools.extend(SEARCH_TOOLS);
    tools.extend(WRITE_TOOLS);
    json!([{
        "matcher": tools.join("|"),
        "hooks": [{"type": "command", "command": command}],
    }])
}

pub fn add_settings(argv: &mut Vec<OsString>, hooks: Value, cwd: &Path) -> Result<()> {
    let found = argv.iter().enumerate().skip(1).find_map(|(i, a)| {
        let a = a.to_string_lossy();
        if a == "--settings" {
            Some((i, i + 1, None))
        } else {
            a.strip_prefix("--settings=")
                .map(|v| (i, i, Some(v.to_string())))
        }
    });
    let Some((flag, at, inline)) = found else {
        let settings = json!({"hooks": {"PreToolUse": hooks}});
        argv.splice(1..1, ["--settings".into(), settings.to_string().into()]);
        return Ok(());
    };
    let given = match inline {
        Some(v) => v,
        None => argv
            .get(at)
            .map(|v| v.to_string_lossy().into_owned())
            .ok_or_else(|| eyre!("claude: --settings needs a value"))?,
    };
    let mut settings = load_settings(&given, cwd)?;
    let pre = settings
        .as_object_mut()
        .ok_or_else(|| eyre!("claude: --settings must be a JSON object"))?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| eyre!("claude: --settings hooks must be a JSON object"))?
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| eyre!("claude: --settings hooks.PreToolUse must be a JSON array"))?;
    pre.extend(hooks.as_array().into_iter().flatten().cloned());
    let merged = settings.to_string();
    if flag == at {
        argv[at] = format!("--settings={merged}").into();
    } else {
        argv[at] = merged.into();
    }
    Ok(())
}

fn load_settings(given: &str, cwd: &Path) -> Result<Value> {
    let text = if given.trim_start().starts_with('{') {
        given.to_string()
    } else {
        let path = cwd.join(given);
        std::fs::read_to_string(&path)
            .map_err(|e| eyre!("claude: reading --settings {}: {e}", path.display()))?
    };
    serde_json::from_str(&text).map_err(|e| eyre!("claude: parsing --settings {given}: {e}"))
}

pub fn run(root: &Path) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let input: Value = serde_json::from_str(&input)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let here = std::env::current_dir()?;
    if let Some(reason) = verdict(&input, root, home.as_deref(), &here) {
        let out = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        });
        println!("{out}");
    }
    Ok(())
}

fn verdict(input: &Value, root: &Path, home: Option<&Path>, here: &Path) -> Option<String> {
    let tool = input["tool_name"].as_str().unwrap_or_default();
    let args = &input["tool_input"];
    let cwd = input["cwd"]
        .as_str()
        .map(PathBuf::from)
        .filter(|c| c.is_absolute())
        .unwrap_or_else(|| here.to_path_buf());
    let text = |key: &str| args[key].as_str().filter(|s| !s.is_empty());
    if WRITE_TOOLS.contains(&tool) {
        let given = text("file_path").or_else(|| text("notebook_path"))?;
        return check_write(tool, Path::new(given), root, &cwd);
    }
    let search = SEARCH_TOOLS.contains(&tool);
    let target = match tool {
        "Read" => PathBuf::from(text("file_path")?),
        "Grep" => PathBuf::from(text("path").unwrap_or(".")),
        "Glob" => {
            let base = Path::new(text("path").unwrap_or("."));
            literal_prefix(&base.join(text("pattern").unwrap_or_default()))
        }
        _ => return None,
    };
    check_read(tool, &target, search, home?, &cwd)
}

fn check_write(tool: &str, given: &Path, root: &Path, cwd: &Path) -> Option<String> {
    let deny = || {
        Some(format!(
            "sulla: {tool} may only change files inside this bead's worktree {}, and {} is outside it. Work inside {} instead.",
            root.display(),
            given.display(),
            root.display()
        ))
    };
    let Some(root) = resolve(root, cwd) else {
        return deny();
    };
    match resolve(given, cwd) {
        Some(p) if p.starts_with(&root) => None,
        _ => deny(),
    }
}

fn check_read(tool: &str, given: &Path, search: bool, home: &Path, cwd: &Path) -> Option<String> {
    let target = resolve(given, cwd);
    for name in SECRETS {
        let secret = home.join(name);
        let resolved = resolve(&secret, cwd).unwrap_or_else(|| secret.clone());
        let hit = match &target {
            Some(t) => t.starts_with(&resolved) || (search && resolved.starts_with(t)),
            None => true,
        };
        if hit {
            return Some(format!(
                "sulla: {tool} may not look at {} because it is or contains {}, which holds secrets. Use a path that leaves it out, such as one inside this bead's worktree.",
                given.display(),
                secret.display()
            ));
        }
    }
    None
}

fn literal_prefix(pattern: &Path) -> PathBuf {
    pattern
        .components()
        .take_while(|c| {
            !c.as_os_str()
                .to_string_lossy()
                .contains(['*', '?', '[', '{'])
        })
        .collect()
}

fn resolve(path: &Path, cwd: &Path) -> Option<PathBuf> {
    let full = cwd.join(path);
    if !full.is_absolute() {
        return None;
    }
    let mut todo = parts(&full);
    let mut out = PathBuf::from("/");
    let mut links = 0;
    while let Some(part) = todo.pop() {
        if part == ".." {
            out.pop();
            continue;
        }
        let next = out.join(&part);
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return None;
                }
                let target = std::fs::read_link(&next).ok()?;
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                todo.extend(parts(&target));
            }
            _ => out = next,
        }
    }
    Some(out)
}

fn parts(path: &Path) -> Vec<OsString> {
    path.components()
        .rev()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_os_string()),
            Component::ParentDir => Some(OsStr::new("..").to_os_string()),
            _ => None,
        })
        .collect()
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sulla-guard-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn call(tool: &str, args: Value, cwd: &Path) -> Value {
        json!({"tool_name": tool, "tool_input": args, "cwd": cwd})
    }

    fn write(path: &str, root: &Path, cwd: &Path) -> Option<String> {
        verdict(
            &call("Write", json!({"file_path": path}), cwd),
            root,
            None,
            Path::new("/"),
        )
    }

    struct Tree {
        base: PathBuf,
        wt: PathBuf,
        home: PathBuf,
    }

    impl Tree {
        fn new(tag: &str) -> Tree {
            let base = scratch(tag);
            let wt = base.join("wt");
            let home = base.join("home");
            std::fs::create_dir_all(wt.join("src")).unwrap();
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            std::fs::create_dir_all(base.join("other")).unwrap();
            Tree { base, wt, home }
        }

        fn read(&self, tool: &str, args: Value) -> Option<String> {
            verdict(
                &call(tool, args, &self.wt),
                &self.wt,
                Some(&self.home),
                Path::new("/"),
            )
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn writes_inside_the_worktree_are_allowed() {
        let t = Tree::new("inside");
        let wt = t.wt.to_string_lossy().into_owned();
        assert_eq!(write(&format!("{wt}/src/main.rs"), &t.wt, &t.wt), None);
        assert_eq!(
            write(&format!("{wt}/new/deep/dir/f.rs"), &t.wt, &t.wt),
            None
        );
        assert_eq!(write(&format!("{wt}/src/../new.rs"), &t.wt, &t.wt), None);
        assert_eq!(write(&format!("{wt}/a/../../wt/b.rs"), &t.wt, &t.wt), None);
        for tool in WRITE_TOOLS {
            let key = if tool == "NotebookEdit" {
                "notebook_path"
            } else {
                "file_path"
            };
            let input = call(tool, json!({key: format!("{wt}/n.ipynb")}), &t.wt);
            assert_eq!(verdict(&input, &t.wt, None, Path::new("/")), None);
        }
    }

    #[test]
    fn writes_outside_the_worktree_are_denied_with_its_path() {
        let t = Tree::new("outside");
        let other = t.base.join("other/x.rs").to_string_lossy().into_owned();
        let reason = write(&other, &t.wt, &t.wt).unwrap();
        assert!(reason.contains(&format!("Work inside {}", t.wt.display())));
        assert!(reason.contains(&other));
        let wt2 = format!("{}2/x.rs", t.wt.display());
        assert!(write(&wt2, &t.wt, &t.wt).is_some());
        assert!(write("/etc/passwd", &t.wt, &t.wt).is_some());
        let up = format!("{}/new/../../other/x.rs", t.wt.display());
        assert!(write(&up, &t.wt, &t.wt).is_some());
        let input = call(
            "NotebookEdit",
            json!({"notebook_path": other.as_str()}),
            &t.wt,
        );
        assert!(verdict(&input, &t.wt, None, Path::new("/")).is_some());
    }

    #[test]
    fn relative_paths_resolve_against_the_session_cwd() {
        let t = Tree::new("relative");
        let src = t.wt.join("src");
        assert_eq!(write("lib.rs", &t.wt, &src), None);
        assert_eq!(write("../new/lib.rs", &t.wt, &src), None);
        assert!(write("../../other/lib.rs", &t.wt, &src).is_some());
        let no_cwd = json!({"tool_name": "Write", "tool_input": {"file_path": "x.rs"}});
        assert_eq!(verdict(&no_cwd, &t.wt, None, &t.wt), None);
        assert!(verdict(&no_cwd, &t.wt, None, &t.base).is_some());
    }

    #[test]
    fn symlinks_out_of_the_worktree_are_followed() {
        let t = Tree::new("symlink");
        symlink(t.base.join("other"), t.wt.join("escape")).unwrap();
        symlink("../other/x.rs", t.wt.join("file")).unwrap();
        symlink(t.base.join("other/missing.rs"), t.wt.join("dangling")).unwrap();
        symlink("src", t.wt.join("inner")).unwrap();
        symlink(t.wt.join("loop"), t.wt.join("loop")).unwrap();
        let p = |s: &str| t.wt.join(s).to_string_lossy().into_owned();
        assert!(write(&p("escape/x.rs"), &t.wt, &t.wt).is_some());
        assert!(write(&p("escape/new/x.rs"), &t.wt, &t.wt).is_some());
        assert!(write(&p("file"), &t.wt, &t.wt).is_some());
        assert!(write(&p("dangling"), &t.wt, &t.wt).is_some());
        assert!(write(&p("loop"), &t.wt, &t.wt).is_some());
        assert!(write(&p("inner/../../other/x.rs"), &t.wt, &t.wt).is_some());
        assert_eq!(write(&p("inner/x.rs"), &t.wt, &t.wt), None);
        assert_eq!(write(&p("escape/../wt/x.rs"), &t.wt, &t.wt), None);
    }

    #[test]
    fn a_symlinked_worktree_still_accepts_its_own_files() {
        let t = Tree::new("linked-root");
        let link = t.base.join("link");
        symlink(&t.wt, &link).unwrap();
        let real = t.wt.join("x.rs").to_string_lossy().into_owned();
        let via = link.join("x.rs").to_string_lossy().into_owned();
        assert_eq!(write(&real, &link, &link), None);
        assert_eq!(write(&via, &t.wt, &t.wt), None);
    }

    #[test]
    fn reads_outside_the_worktree_are_allowed_except_secrets() {
        let t = Tree::new("reads");
        let other = t.base.join("other/x.rs").to_string_lossy().into_owned();
        assert_eq!(t.read("Read", json!({"file_path": other})), None);
        assert_eq!(t.read("Read", json!({"file_path": "src/lib.rs"})), None);
        let key = t
            .home
            .join(".ssh/id_ed25519")
            .to_string_lossy()
            .into_owned();
        let reason = t.read("Read", json!({"file_path": key})).unwrap();
        assert!(reason.contains(".ssh"));
        let gpg = t.home.join(".gnupg").to_string_lossy().into_owned();
        assert!(
            t.read("Grep", json!({"pattern": "x", "path": gpg}))
                .is_some()
        );
        symlink(t.home.join(".ssh"), t.wt.join("keys")).unwrap();
        assert!(t.read("Read", json!({"file_path": "keys/id"})).is_some());
        assert_eq!(t.read("Read", json!({"file_path": "src/../keysx"})), None);
    }

    #[test]
    fn searches_that_cover_a_secrets_dir_are_denied() {
        let t = Tree::new("search");
        let home = t.home.to_string_lossy().into_owned();
        assert!(
            t.read("Grep", json!({"pattern": "x", "path": home}))
                .is_some()
        );
        assert!(
            t.read("Glob", json!({"pattern": "**/*", "path": home}))
                .is_some()
        );
        let abs = format!("{home}/.ssh/*");
        assert!(t.read("Glob", json!({"pattern": abs})).is_some());
        assert!(
            t.read("Glob", json!({"pattern": "../home/.ssh/*"}))
                .is_some()
        );
        assert_eq!(t.read("Grep", json!({"pattern": "x"})), None);
        assert_eq!(t.read("Glob", json!({"pattern": "src/**/*.rs"})), None);
        assert_eq!(t.read("Read", json!({"file_path": home})), None);
        let other = t.base.join("other").to_string_lossy().into_owned();
        assert_eq!(t.read("Grep", json!({"pattern": "x", "path": other})), None);
    }

    #[test]
    fn other_tools_pass() {
        let t = Tree::new("others");
        assert_eq!(t.read("Bash", json!({"command": "cat ~/.ssh/id"})), None);
        assert_eq!(t.read("WebFetch", json!({"url": "x"})), None);
    }

    #[test]
    fn hooks_match_file_tools_and_fail_closed() {
        let h = hooks(Path::new("/bin/sul'la"), Path::new("/w t"));
        assert_eq!(
            h[0]["matcher"],
            "Read|Grep|Glob|Edit|Write|MultiEdit|NotebookEdit"
        );
        assert_eq!(
            h[0]["hooks"][0]["command"],
            "'/bin/sul'\\''la' guard '/w t' || exit 2"
        );
    }

    fn argv(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    fn settings_at(argv: &[OsString], i: usize) -> Value {
        let s = argv[i].to_string_lossy();
        serde_json::from_str(s.strip_prefix("--settings=").unwrap_or(&s)).unwrap()
    }

    #[test]
    fn settings_are_added_after_the_program() {
        let mut a = argv(&["claude", "--resume", "s", "do it"]);
        add_settings(&mut a, json!([{"matcher": "Write"}]), Path::new("/")).unwrap();
        assert_eq!(a[1], "--settings");
        assert_eq!(a[3..], argv(&["--resume", "s", "do it"]));
        assert_eq!(
            settings_at(&a, 2),
            json!({"hooks": {"PreToolUse": [{"matcher": "Write"}]}})
        );
    }

    #[test]
    fn existing_settings_are_merged_not_duplicated() {
        let d = scratch("settings");
        let given = json!({
            "model": "opus",
            "hooks": {"PreToolUse": [{"matcher": "Bash"}], "Stop": []},
        });
        std::fs::write(d.join("s.json"), given.to_string()).unwrap();
        let ours = json!([{"matcher": "Write"}]);
        let want = json!({
            "model": "opus",
            "hooks": {"PreToolUse": [{"matcher": "Bash"}, {"matcher": "Write"}], "Stop": []},
        });
        for mut a in [
            argv(&["claude", "--settings", "s.json", "p"]),
            argv(&["claude", "--settings", &given.to_string(), "p"]),
            argv(&["claude", &format!("--settings={given}"), "p"]),
        ] {
            add_settings(&mut a, ours.clone(), &d).unwrap();
            let flags = a
                .iter()
                .filter(|x| x.to_string_lossy().starts_with("--settings"))
                .count();
            assert_eq!(flags, 1);
            let i = a.iter().position(|x| x == "p").unwrap() - 1;
            assert_eq!(settings_at(&a, i), want);
        }
        let mut bare = argv(&["claude", "--settings", "{}"]);
        add_settings(&mut bare, ours.clone(), &d).unwrap();
        assert_eq!(
            settings_at(&bare, 2),
            json!({"hooks": {"PreToolUse": [{"matcher": "Write"}]}})
        );
        let mut missing = argv(&["claude", "--settings", "nope.json"]);
        assert!(add_settings(&mut missing, ours.clone(), &d).is_err());
        let mut bad = argv(&["claude", "--settings", "{\"hooks\": []}"]);
        assert!(add_settings(&mut bad, ours, &d).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }
}
