use super::*;
use crate::config::effective::Source;
use crate::ui::config::{ConfigView, Prompt, Row};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::os::unix::process::ExitStatusExt;

fn press(h: &mut Harness, code: KeyCode) {
    h.app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn ctrl(h: &mut Harness, c: char) {
    h.app
        .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
}

fn typed(h: &mut Harness, s: &str) {
    for c in s.chars() {
        press(h, KeyCode::Char(c));
    }
}

fn view(h: &Harness) -> &ConfigView {
    match &h.app.mode {
        Mode::Config(v) => v,
        _ => panic!("config viewer not open"),
    }
}

fn view_mut(h: &mut Harness) -> &mut ConfigView {
    match &mut h.app.mode {
        Mode::Config(v) => v,
        _ => panic!("config viewer not open"),
    }
}

fn goto(h: &mut Harness, path: &str) {
    let v = view_mut(h);
    let row = v
        .sections
        .iter()
        .enumerate()
        .find_map(|(s, sec)| {
            sec.items
                .iter()
                .position(|i| i.path.join(".") == path)
                .map(|i| Row::Item(s, i))
        })
        .unwrap_or_else(|| panic!("no {path}"));
    assert!(v.select(row));
}

fn editor(h: &Harness) -> &crate::ui::config::Editor {
    match &view(h).prompt {
        Some(Prompt::Edit(ed)) => ed,
        p => panic!("no editor: {p:?}"),
    }
}

fn with_global(name: &str) -> (Harness, PathBuf) {
    let h = Harness::new(name, &[]);
    let global = h.dir.join("home").join("sulla").join("config.toml");
    let mut h = h;
    h.app.layers = crate::config::effective::Layers::load_from(
        Some(&global),
        &Config::repo_path(&h.dir),
        toml::Table::new(),
    )
    .unwrap();
    (h, global)
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn inline_edit_writes_the_repo_file_and_reloads() {
    let mut h = Harness::new("cfg-inline", &[]);
    write_repo_config(&h, "# repo settings\nmax_agents = 4 # few\n");
    h.app.command("reload");
    h.app.command("config");
    goto(&mut h, "max_agents");
    press(&mut h, KeyCode::Enter);
    assert_eq!(editor(&h).buf, "4");
    assert_eq!(editor(&h).target, Source::Repo);
    ctrl(&mut h, 'u');
    typed(&mut h, "6");
    press(&mut h, KeyCode::Enter);
    assert!(view(&h).prompt.is_none());
    assert_eq!(
        read(&Config::repo_path(&h.dir)),
        "# repo settings\nmax_agents = 6 # few\n"
    );
    assert_eq!(h.app.cfg.max_agents, 6);
    assert_eq!(
        h.app.status,
        "set max_agents = 6 in repo · config reloaded: 1 changed (max_agents 4→6)"
    );
    assert_eq!(view(&h).flash.as_deref(), Some(h.app.status.as_str()));
    assert!(matches!(view(&h).current(), Some(Row::Item(..))));
    let max = view(&h)
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.dotted() == "max_agents")
        .unwrap();
    assert_eq!(max.source, Source::Repo);
}

#[test]
fn inline_saves_replace_a_runtime_override_of_the_same_key() {
    let mut h = Harness::new("cfg-runtime", &[]);
    h.app.command("max 5");
    h.app.command("config");
    goto(&mut h, "max_agents");
    press(&mut h, KeyCode::Enter);
    assert_eq!(editor(&h).buf, "5");
    ctrl(&mut h, 'u');
    typed(&mut h, "3");
    press(&mut h, KeyCode::Enter);
    assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 3\n");
    assert_eq!(h.app.cfg.max_agents, 3);
    assert_eq!(h.app.cfg.poll_secs, 3600);
}

#[test]
fn invalid_inline_values_keep_the_editor_open_and_the_file_untouched() {
    let mut h = Harness::new("cfg-invalid-inline", &[]);
    write_repo_config(&h, "max_agents = 4\n");
    h.app.command("reload");
    h.app.command("config");
    goto(&mut h, "max_agents");
    press(&mut h, KeyCode::Char('i'));
    ctrl(&mut h, 'u');
    typed(&mut h, "-2");
    press(&mut h, KeyCode::Enter);
    let err = editor(&h).error.clone().unwrap();
    assert!(err.contains("invalid value"), "{err}");
    assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 4\n");
    assert_eq!(h.app.cfg.max_agents, 4);
    press(&mut h, KeyCode::Backspace);
    assert!(editor(&h).error.is_none());
    press(&mut h, KeyCode::Esc);
    assert!(view(&h).prompt.is_none());
    assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 4\n");
}

#[test]
fn enums_cycle_with_tab_and_bools_toggle_with_space() {
    let mut h = Harness::new("cfg-enum", &[]);
    h.app.command("config");
    goto(&mut h, "agent");
    press(&mut h, KeyCode::Enter);
    assert_eq!(editor(&h).buf, "claude");
    press(&mut h, KeyCode::Tab);
    assert_eq!(editor(&h).buf, "copilot");
    press(&mut h, KeyCode::BackTab);
    press(&mut h, KeyCode::BackTab);
    assert_eq!(editor(&h).buf, "copilot");
    press(&mut h, KeyCode::Enter);
    assert_eq!(read(&Config::repo_path(&h.dir)), "agent = \"copilot\"\n");
    assert_eq!(h.app.cfg.agent, AgentKind::Copilot);

    goto(&mut h, "pr.draft");
    press(&mut h, KeyCode::Char(' '));
    assert_eq!(
        read(&Config::repo_path(&h.dir)),
        "agent = \"copilot\"\n\n[pr]\ndraft = true\n"
    );
    assert!(h.app.cfg.pr.draft);
    press(&mut h, KeyCode::Char(' '));
    assert!(!h.app.cfg.pr.draft);
    goto(&mut h, "max_agents");
    press(&mut h, KeyCode::Char(' '));
    assert!(view(&h).flash.as_deref().unwrap().contains("Space toggles"));
}

#[test]
fn ctrl_g_writes_the_global_file_and_warns_when_repo_shadows_it() {
    let (mut h, global) = with_global("cfg-global");
    write_repo_config(&h, "max_agents = 4\n");
    h.app.command("reload");
    h.app.command("config");
    goto(&mut h, "max_agents");
    press(&mut h, KeyCode::Enter);
    assert!(view(&h).shadowed(editor(&h)).is_none());
    ctrl(&mut h, 'g');
    assert_eq!(editor(&h).target, Source::Global);
    assert_eq!(
        view(&h).shadowed(editor(&h)).as_deref(),
        Some("repo overrides this")
    );
    ctrl(&mut h, 'u');
    typed(&mut h, "9");
    press(&mut h, KeyCode::Enter);
    assert_eq!(read(&global), "max_agents = 9\n");
    assert_eq!(read(&Config::repo_path(&h.dir)), "max_agents = 4\n");
    assert_eq!(h.app.cfg.max_agents, 4);

    goto(&mut h, "incus.browse.url");
    press(&mut h, KeyCode::Enter);
    ctrl(&mut h, 'g');
    typed(&mut h, "http://app.test:8000");
    press(&mut h, KeyCode::Enter);
    assert_eq!(
        read(&global),
        "max_agents = 9\n\n[incus.browse]\nurl = \"http://app.test:8000\"\n"
    );
    assert_eq!(
        h.app.layers.config().unwrap().incus.browse.url.as_deref(),
        Some("http://app.test:8000")
    );
}

#[test]
fn x_removes_the_key_after_confirming() {
    let (mut h, global) = with_global("cfg-remove");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, "max_agents = 5\n").unwrap();
    write_repo_config(&h, "max_agents = 4\nnope = 1\n");
    h.app.command("reload");
    h.app.command("config");
    goto(&mut h, "max_agents");
    press(&mut h, KeyCode::Char('x'));
    match &view(&h).prompt {
        Some(Prompt::Remove {
            source, fallback, ..
        }) => {
            assert_eq!(*source, Source::Repo);
            assert_eq!(*fallback, Some(toml::Value::Integer(5)));
        }
        p => panic!("{p:?}"),
    }
    press(&mut h, KeyCode::Char('n'));
    assert!(view(&h).prompt.is_none());
    assert_eq!(view(&h).flash.as_deref(), Some("cancelled"));
    press(&mut h, KeyCode::Char('x'));
    press(&mut h, KeyCode::Char('y'));
    assert_eq!(read(&Config::repo_path(&h.dir)), "nope = 1\n");
    assert_eq!(h.app.cfg.max_agents, 5);
    assert!(h.app.status.starts_with("removed max_agents from repo · "));

    goto(&mut h, "nope");
    press(&mut h, KeyCode::Enter);
    assert!(view(&h).flash.as_deref().unwrap().contains("press x"));
    press(&mut h, KeyCode::Char('x'));
    press(&mut h, KeyCode::Char('y'));
    assert_eq!(read(&Config::repo_path(&h.dir)), "");

    goto(&mut h, "poll_secs");
    press(&mut h, KeyCode::Char('x'));
    assert!(view(&h).prompt.is_none());
    assert_eq!(
        view(&h).flash.as_deref(),
        Some("poll_secs is not set in a config file")
    );
}

#[test]
fn arrays_and_multi_line_values_point_at_the_editor() {
    let mut h = Harness::new("cfg-arrays", &[]);
    write_repo_config(&h, "system_prompt = \"\"\"\none\ntwo\n\"\"\"\n");
    h.app.command("reload");
    h.app.command("config");
    goto(&mut h, "incus.mounts");
    press(&mut h, KeyCode::Enter);
    assert!(view(&h).prompt.is_none());
    assert_eq!(
        view(&h).flash.as_deref(),
        Some("incus.mounts can't be edited inline, press e to edit in $EDITOR")
    );
    goto(&mut h, "system_prompt");
    press(&mut h, KeyCode::Enter);
    assert!(view(&h).prompt.is_none());
    assert!(view(&h).flash.as_deref().unwrap().contains("press e"));
}

fn finish_editor(h: &mut Harness) {
    h.app
        .suspend_finished("editor", Ok(std::process::ExitStatus::from_raw(0)));
}

#[test]
fn e_round_trips_through_the_editor_and_returns_to_the_popup() {
    let mut h = Harness::new("cfg-editor", &[]);
    h.app.command("config");
    goto(&mut h, "max_agents");
    let cursor = view(&h).current();
    press(&mut h, KeyCode::Char('e'));
    let repo = Config::repo_path(&h.dir);
    match &h.app.suspend {
        Some(Suspend::File(p)) => assert_eq!(*p, repo),
        _ => panic!("no editor requested"),
    }
    assert_eq!(read(&repo), "");
    assert!(matches!(h.app.mode, Mode::Config(_)));
    h.app.suspend = None;
    std::fs::write(&repo, "max_agents = 7\n").unwrap();
    finish_editor(&mut h);
    assert_eq!(h.app.cfg.max_agents, 7);
    assert_eq!(h.app.status, "config reloaded: 1 changed (max_agents 2→7)");
    assert_eq!(view(&h).current(), cursor);

    press(&mut h, KeyCode::Char('e'));
    h.app.suspend = None;
    std::fs::write(&repo, "max_agents = 8\n[claude\n").unwrap();
    finish_editor(&mut h);
    assert_eq!(h.app.cfg.max_agents, 7);
    match &view(&h).prompt {
        Some(Prompt::Invalid { source, error }) => {
            assert_eq!(*source, Source::Repo);
            assert!(error.contains("line 2"), "{error}");
        }
        p => panic!("{p:?}"),
    }
    press(&mut h, KeyCode::Char('j'));
    assert!(matches!(view(&h).prompt, Some(Prompt::Invalid { .. })));
    press(&mut h, KeyCode::Char('e'));
    assert!(matches!(h.app.suspend, Some(Suspend::File(_))));
    h.app.suspend = None;
    finish_editor(&mut h);
    assert!(matches!(view(&h).prompt, Some(Prompt::Invalid { .. })));
    press(&mut h, KeyCode::Esc);
    assert!(view(&h).prompt.is_none());
    assert_eq!(read(&repo), "max_agents = 8\n[claude\n");
    assert_eq!(h.app.cfg.max_agents, 7);
    assert!(h.app.reload.error.is_some());

    std::fs::write(&repo, "max_agents = 8\n").unwrap();
    press(&mut h, KeyCode::Char('e'));
    h.app.suspend = None;
    finish_editor(&mut h);
    assert_eq!(h.app.cfg.max_agents, 8);
    assert!(h.app.reload.error.is_none());
    assert!(view(&h).prompt.is_none());
}

#[test]
fn capital_e_and_config_edit_open_the_global_file() {
    let (mut h, global) = with_global("cfg-editor-global");
    h.app.command("config edit global");
    match &h.app.suspend {
        Some(Suspend::File(p)) => assert_eq!(*p, global),
        _ => panic!("no editor requested"),
    }
    assert!(global.exists());
    h.app.suspend = None;
    std::fs::write(&global, "poll_secs = 9\n").unwrap();
    finish_editor(&mut h);
    assert_eq!(h.app.cfg.poll_secs, 9);
    assert!(matches!(h.app.mode, Mode::Config(_)));
    press(&mut h, KeyCode::Char('q'));
    h.app.command("config edit");
    match &h.app.suspend {
        Some(Suspend::File(p)) => assert_eq!(*p, Config::repo_path(&h.dir)),
        _ => panic!("no editor requested"),
    }
    h.app.suspend = None;
    press(&mut h, KeyCode::Char('E'));
    assert!(matches!(&h.app.suspend, Some(Suspend::File(p)) if *p == global));
    h.app.command("config bogus");
    assert!(h.app.status.contains("unknown :config argument"));

    let mut h = Harness::new("cfg-no-global", &[]);
    h.app.command("config");
    press(&mut h, KeyCode::Char('E'));
    assert!(h.app.suspend.is_none());
    assert_eq!(
        view(&h).flash.as_deref(),
        Some("no global config directory")
    );
    goto(&mut h, "max_agents");
    press(&mut h, KeyCode::Enter);
    ctrl(&mut h, 'g');
    assert_eq!(editor(&h).target, Source::Repo);
}

#[test]
fn paste_goes_into_the_inline_editor() {
    let mut h = Harness::new("cfg-paste", &[]);
    h.app.command("config");
    goto(&mut h, "claude.bin");
    press(&mut h, KeyCode::Enter);
    ctrl(&mut h, 'u');
    h.app.on_paste("/opt/x\n");
    assert_eq!(editor(&h).buf, "/opt/x ");
    press(&mut h, KeyCode::Backspace);
    press(&mut h, KeyCode::Home);
    press(&mut h, KeyCode::Delete);
    press(&mut h, KeyCode::Char('/'));
    press(&mut h, KeyCode::End);
    assert_eq!(editor(&h).buf, "/opt/x");
    assert_eq!(editor(&h).at, 6);
}
