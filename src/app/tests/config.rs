use super::*;

#[test]
fn config_command_opens_the_viewer_with_runtime_changes() {
    use crate::config::effective::Source;
    use crate::ui::config::Row;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut h = Harness::new("config-view", &[]);
    let key = |h: &mut Harness, c: char| {
        h.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    };
    let source = |h: &Harness, path: &str| {
        let Mode::Config(v) = &h.app.mode else {
            panic!("config viewer not open");
        };
        v.sections
            .iter()
            .flat_map(|s| &s.items)
            .find(|i| i.path.join(".") == path)
            .map(|i| (i.source, i.value.clone()))
            .unwrap()
    };
    h.app.command("max 5");
    h.app.command("config");
    assert_eq!(
        source(&h, "max_agents"),
        (Source::Runtime, Some(toml::Value::Integer(5)))
    );
    assert_eq!(
        source(&h, "poll_secs"),
        (Source::Runtime, Some(toml::Value::Integer(3600)))
    );
    assert_eq!(source(&h, "branch_prefix").0, Source::Default);
    key(&mut h, '/');
    for c in "max_ag".chars() {
        key(&mut h, c);
    }
    h.app
        .on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    key(&mut h, 'y');
    assert_eq!(h.app.status, "max_agents = 5");
    let Mode::Config(v) = &h.app.mode else {
        panic!("closed");
    };
    assert!(!v.typing);
    assert_eq!(v.filter, "max_ag");
    assert!(matches!(v.current(), Some(Row::Item(..))));
    h.app
        .on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let Mode::Config(v) = &h.app.mode else {
        panic!("Esc closed before clearing the filter");
    };
    assert!(v.filter.is_empty());
    key(&mut h, 'z');
    key(&mut h, 'M');
    let Mode::Config(v) = &h.app.mode else {
        panic!("closed");
    };
    assert!(v.rows().iter().all(|r| matches!(r, Row::Header(_))));
    key(&mut h, 'q');
    assert!(matches!(h.app.mode, Mode::Normal));
}

fn launched_args(app: &App) -> Vec<String> {
    let launch = crate::agent::Launch {
        id: "x-1".into(),
        session: "s-1".into(),
        bead: true,
        worktree: PathBuf::from("/w"),
        resume: false,
        prompt: None,
        model: None,
    };
    app.backend(AgentKind::Claude)
        .command(&launch)
        .get_argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn reload_applies_live_keys_and_holds_restart_keys() {
    use crate::config::SandboxKind;
    let mut h = Harness::new("reload-live", &[]);
    assert!(!launched_args(&h.app).contains(&"opus".to_string()));
    write_repo_config(
        &h,
        "max_agents = 4\nsandbox = \"incus\"\n[claude]\nmodel = \"opus\"\n[incus]\nmemory = \"4GiB\"\n[pr]\npoll_secs = 30",
    );
    h.app.command("reload");
    assert_eq!(
        h.app.status,
        "config reloaded: 3 changed (claude.model, max_agents 2→4, pr.poll_secs 120→30) · restart needed for incus.memory, sandbox"
    );
    assert_eq!(h.app.cfg.max_agents, 4);
    assert_eq!(h.app.cfg.claude.model.as_deref(), Some("opus"));
    assert_eq!(h.app.cfg.claude.bin, "true");
    assert_eq!(h.app.cfg.poll_secs, 3600);
    assert_eq!(h.app.cfg.sandbox, SandboxKind::Worktree);
    assert_eq!(h.app.cfg.incus.memory, "8GiB");
    assert_eq!(h.app.pr_every.get(), Duration::from_secs(30));
    assert_eq!(h.app.poll_every.get(), Duration::from_secs(3600));
    let args = launched_args(&h.app);
    assert_eq!(args[0], "true");
    assert!(
        args.windows(2).any(|w| w == ["--model", "opus"]),
        "{args:?}"
    );
    assert_eq!(h.app.reload.restart.len(), 2);

    h.app.command("config");
    let Mode::Config(v) = &h.app.mode else {
        panic!("config viewer not open");
    };
    let memory = v
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.dotted() == "incus.memory")
        .unwrap();
    assert_eq!(memory.value, Some(toml::Value::String("8GiB".into())));
    assert_eq!(
        memory.restart,
        Some(Some(toml::Value::String("4GiB".into())))
    );
    let max = v
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.dotted() == "max_agents")
        .unwrap();
    assert!(v.flashing(max));

    write_repo_config(
        &h,
        "max_agents = 4\npoll_secs = 7\n[claude]\nmodel = \"opus\"",
    );
    h.app.on_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('r'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        h.app.status,
        "config reloaded: 2 changed (poll_secs 3600→7, pr.poll_secs 30→120)"
    );
    assert!(h.app.reload.restart.is_empty());
    assert_eq!(h.app.poll_every.get(), Duration::from_secs(7));
    let Mode::Config(v) = &h.app.mode else {
        panic!("r closed the viewer");
    };
    assert_eq!(v.flash.as_deref(), Some(h.app.status.as_str()));

    h.app.command("reload");
    assert_eq!(h.app.status, "config reloaded: no changes");
}

#[test]
fn reload_keeps_runtime_overrides_whose_file_value_did_not_change() {
    let mut h = Harness::new("reload-runtime", &[]);
    h.app.command("max 6");
    write_repo_config(&h, "[stall]\nminutes = 9");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 6);
    assert_eq!(h.app.cfg.stall.minutes, 9);
    write_repo_config(&h, "max_agents = 1\n[stall]\nminutes = 9");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 1);
}

#[test]
fn invalid_config_leaves_the_running_config_untouched() {
    let mut h = Harness::new("reload-invalid", &[]);
    write_repo_config(&h, "max_agents = 4\n");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 4);
    write_repo_config(&h, "max_agents = 5\n[claude\n");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 4);
    assert!(
        h.app.status.starts_with(crate::config::reload::FAILED),
        "{}",
        h.app.status
    );
    assert!(
        h.app.status.contains("config.toml line 2"),
        "{}",
        h.app.status
    );
    assert!(h.app.reload.error.is_some());
    assert_eq!(h.app.layers.config().unwrap().max_agents, 4);
    write_repo_config(&h, "max_agents = 5\n");
    h.app.command("reload");
    assert_eq!(h.app.cfg.max_agents, 5);
    assert!(h.app.reload.error.is_none());
}

#[test]
fn editing_the_config_file_reloads_automatically() {
    let mut h = Harness::new("reload-auto", &[]);
    h.app.handle(Event::Tick);
    write_repo_config(&h, "max_agents = 7\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.app.cfg.max_agents != 7 && Instant::now() < deadline {
        h.app.handle(Event::Tick);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(h.app.cfg.max_agents, 7);
    assert_eq!(h.app.status, "config reloaded: 1 changed (max_agents 2→7)");
}
