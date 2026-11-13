use super::*;

#[test]
fn browse_runs_the_app_lifecycle_off_the_ui_thread() {
    let mut h = Harness::new("browse", &[("t-1", Some(State::Running))]);
    h.app.cfg.sandbox = crate::config::SandboxKind::Incus;
    h.app.cfg.incus.browse = crate::config::BrowseConfig {
        ports: vec![8000],
        up: Some("echo booting".into()),
        ready: Some("echo not yet; false".into()),
        ready_timeout_secs: 1,
        ..Default::default()
    };
    h.app.browse("t-1");
    assert_eq!(
        h.app.status,
        "browse: t-1 has no slot or worktree, press A to start it"
    );
    assert_eq!(h.app.slots.acquire("t-1", 2), Some(0));
    std::fs::create_dir_all(h.app.git.worktree_path("t-1")).unwrap();
    let called = Instant::now();
    h.app.browse("t-1");
    assert!(called.elapsed() < Duration::from_millis(500));
    assert_eq!(h.app.status, "browse t-1: checking the app…");
    h.app.browse("t-1");
    assert_eq!(h.app.status, "browse t-1: already in progress");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while h.app.browsing.contains("t-1") {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("browse did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
            seen.push(h.app.status.clone());
        }
    }
    assert!(
        seen.contains(&"browse t-1: starting app…".to_string()),
        "{seen:?}"
    );
    let last = seen.last().unwrap();
    assert!(
        last.starts_with("browse t-1: app was not ready within 1s"),
        "{last}"
    );
    assert!(last.contains(" | booting"), "{last}");
    assert!(last.contains(" | not yet"), "{last}");
    assert!(!last.contains("--- "), "{last}");
    assert!(h.dir.join(".sulla/logs/t-1.app.log").exists());
    h.app.browse("stop t-1");
    assert_eq!(
        h.app.status,
        "browse: browse stop needs [incus.browse] down"
    );
}

fn browse_cfg(h: &mut Harness, browse: crate::config::BrowseConfig) {
    h.app.cfg.sandbox = crate::config::SandboxKind::Incus;
    h.app.cfg.incus.browse = browse;
}

fn drain_browse(h: &mut Harness, label: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    while h.app.browsing.contains(label) {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("browse did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
            seen.push(h.app.status.clone());
        }
    }
    seen
}

#[test]
fn browse_records_the_app_owner_on_up_and_clears_it_on_down_or_reset() {
    let mut h = Harness::new("app-owner", &[("t-1", Some(State::Running))]);
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            up: Some("true".into()),
            down: Some("true".into()),
            ready: Some("false".into()),
            ready_timeout_secs: 1,
            ..Default::default()
        },
    );
    assert_eq!(h.app.slots.acquire("t-1", 2), Some(0));
    std::fs::create_dir_all(h.app.git.worktree_path("t-1")).unwrap();
    h.app.browse("t-1");
    drain_browse(&mut h, "t-1");
    assert_eq!(h.app.apps.owner(0), Some("t-1"));
    h.app.publish_slots();
    assert_eq!(browse::read_own_apps(&h.dir).owner(0), Some("t-1"));

    h.app.browse("stop t-1");
    drain_browse(&mut h, "t-1");
    assert_eq!(h.app.apps.owner(0), None);
    h.app.publish_slots();
    assert!(browse::read_own_apps(&h.dir).is_empty());

    h.app.apps.started(0, "t-1");
    h.app.slots.release("t-1");
    h.app.finish(Some(0), "t-1", Outcome::Exited);
    assert_eq!(h.app.apps.owner(0), Some("t-1"));
    h.app.finish(Some(0), "t-1", Outcome::Killed);
    assert_eq!(h.app.apps.owner(0), None);
    h.app.apps.started(0, "t-1");
    h.app.finish(Some(0), "t-1", Outcome::Discarded);
    assert_eq!(h.app.apps.owner(0), None);
}

#[test]
fn publishing_slots_keeps_app_owners_another_process_recorded() {
    let mut h = Harness::new("app-merge", &[("t-1", Some(State::Running))]);
    h.app.slots.acquire("t-1", 2);
    h.app.apps.started(0, "t-1");
    h.app.publish_slots();
    let mut theirs = browse::read_own_apps(&h.dir);
    theirs.started(1, "t-2");
    browse::write_slots(&h.dir, &h.app.slots.snapshot(), &theirs).unwrap();
    h.app.slots.acquire("t-3", 2);
    h.app.apps.stopped(0);
    h.app.publish_slots();
    let file = browse::read_own_apps(&h.dir);
    assert_eq!(file.owner(0), None);
    assert_eq!(file.owner(1), Some("t-2"));
    assert_eq!(h.app.apps, file);
}

#[test]
fn browse_asks_before_replacing_another_beads_app() {
    let mut h = Harness::new("app-stale", &[("t-1", Some(State::Running))]);
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            up: Some("true".into()),
            ready: Some("false".into()),
            ready_timeout_secs: 1,
            ..Default::default()
        },
    );
    assert_eq!(h.app.slots.acquire("t-1", 2), Some(0));
    std::fs::create_dir_all(h.app.git.worktree_path("t-1")).unwrap();
    h.app.apps.started(0, "o-9");
    h.app.browse("t-1");
    let Mode::Confirm { msg, action } = std::mem::replace(&mut h.app.mode, Mode::Normal) else {
        panic!("no confirm");
    };
    assert_eq!(msg, "slot 0 is serving o-9; restart app for t-1? y/n");
    assert!(!h.app.browsing.contains("t-1"));
    assert!(matches!(&action, Action::RestartApp(t) if t == "t-1"));
    h.app.confirm(action);
    assert!(h.app.browsing.contains("t-1"));
    let seen = drain_browse(&mut h, "t-1");
    assert!(
        seen.contains(&"browse t-1: starting app…".to_string()),
        "{seen:?}"
    );
    assert_eq!(h.app.apps.owner(0), Some("t-1"));
}

#[test]
fn probing_runs_off_the_ui_thread_and_skips_without_ports() {
    let mut h = Harness::new("app-probe", &[("t-1", Some(State::Running))]);
    h.app.slots.acquire("t-1", 2);
    h.app.probe_apps();
    assert!(!h.app.probing);
    assert!(h.app.app_live.is_empty());
    let held = std::net::TcpListener::bind("127.0.100.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    h.app.cfg.incus.browse.ports = vec![port];
    h.app.probed = None;
    let called = Instant::now();
    h.app.probe_apps();
    assert!(called.elapsed() < Duration::from_millis(100));
    assert!(h.app.probing);
    h.app.probe_apps();
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.app.probing {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("probe did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
        }
    }
    assert_eq!(h.app.app_live.get(&0), Some(&browse::Live::Up));
    assert_eq!(h.app.app_status("t-1"), Some((browse::Live::Up, None)));
    assert_eq!(h.app.app_status(&agent::review_id("t-1")), None);
    drop(held);
}

#[test]
fn app_status_shows_on_cards_titles_and_review_header() {
    use crate::ui::theme;
    let mut h = Harness::new("app-badge", &[("t-1", Some(State::Running))]);
    h.app.slots.acquire("t-1", 2);
    assert_eq!(find(&screen(&h.app, 200, 40), "app"), None);
    h.app.app_live.insert(0, browse::Live::Up);
    h.app.col = Column::ALL
        .iter()
        .position(|c| *c == Column::InProgress)
        .unwrap();
    for zoom in [false, true] {
        h.app.zoom = zoom;
        assert_eq!(find(&screen(&h.app, 200, 40), "▲ app"), Some(theme::GREEN));
    }
    h.app.app_live.insert(0, browse::Live::Partial);
    assert_eq!(find(&screen(&h.app, 200, 40), "◐ app"), Some(theme::YELLOW));
    h.app.app_live.insert(0, browse::Live::Down);
    assert_eq!(find(&screen(&h.app, 200, 40), "▼ app"), Some(theme::MUTED));
    h.app.app_live.insert(0, browse::Live::Up);
    h.app.apps.started(0, "o-9");
    assert_eq!(
        find(&screen(&h.app, 200, 40), "▲ app (o-9)"),
        Some(theme::PEACH)
    );
    h.app.view = View::Progress("t-1".into());
    assert_eq!(
        find(&screen(&h.app, 200, 40), "▲ app (o-9)"),
        Some(theme::PEACH)
    );
    h.app.apps.started(0, "t-1");
    let buf = screen(&h.app, 200, 40);
    assert_eq!(find(&buf, "▲ app"), Some(theme::GREEN));
    assert_eq!(find(&buf, "▲ app ("), None);
    let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    h.app.view = View::Review(crate::ui::review::Diff::parse("t-1", diff));
    assert_eq!(find(&screen(&h.app, 200, 40), "▲ app"), Some(theme::GREEN));
}

#[test]
fn browse_resumes_an_evicted_bead_and_protects_it_from_eviction() {
    let review = Some(State::Review);
    let mut h = Harness::new(
        "browse-resume",
        &[
            ("r-1", review),
            ("r-2", review),
            ("r-3", review),
            ("p-1", None),
        ],
    );
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            up: Some("true".into()),
            ready: Some("false".into()),
            ready_timeout_secs: 1,
            ..Default::default()
        },
    );
    park(&mut h, &["r-1", "r-2"]);
    h.app.git.ensure_worktree("r-3", "main").unwrap();
    h.app.browse("r-3");
    assert_eq!(h.finished(), [(0, "r-1".to_string(), Outcome::Evicted)]);
    assert!(h.app.is_starting("r-3"));
    h.pump();
    assert_eq!(h.app.slots.get("r-3"), Some(0));
    assert!(h.app.browsing.contains("r-3"));
    let seen = drain_browse(&mut h, "r-3");
    assert!(
        seen.contains(&"browse r-3: starting app…".to_string()),
        "{seen:?}"
    );
    assert_eq!(h.app.apps.owner(0), Some("r-3"));
    assert_eq!(h.state("r-3"), review);

    h.app.ready = vec![h.bead("p-1")];
    h.app.schedule();
    assert_eq!(
        h.finished().last(),
        Some(&(1, "r-2".to_string(), Outcome::Evicted))
    );
    assert!(h.app.agents.contains_key("r-3"));
}

#[test]
fn browse_without_a_free_slot_says_why() {
    let review = Some(State::Review);
    let mut h = Harness::new(
        "browse-full",
        &[("r-1", review), ("r-2", review), ("r-3", review)],
    );
    browse_cfg(
        &mut h,
        crate::config::BrowseConfig {
            ports: vec![8000],
            ..Default::default()
        },
    );
    park(&mut h, &["r-1", "r-2"]);
    for id in ["r-1", "r-2"] {
        h.app
            .edit_local(id, |b| b.set_state_label(Some(State::Running)));
    }
    h.app.git.ensure_worktree("r-3", "main").unwrap();
    h.app.browse("r-3");
    assert!(
        h.app.status.starts_with("no free agent slot for r-3"),
        "{}",
        h.app.status
    );
    assert!(!h.app.is_starting("r-3"));
    assert!(h.finished().is_empty());
    h.app.browse("stop r-3");
    assert_eq!(h.app.status, "browse: r-3 has no slot, so no app to stop");
    h.app.browse("x-9");
    assert_eq!(h.app.status, "browse: no bead x-9");
}
