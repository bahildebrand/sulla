use super::*;

#[test]
fn review_launch_marks_reviewing_when_it_starts() {
    let mut h = Harness::new("review", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_review(&bead, record);
    let rid = agent::review_id("t-1");
    assert!(h.app.is_starting(&rid));
    assert_eq!(h.app.agent_key("t-1"), rid);
    assert_eq!(h.state("t-1"), Some(State::Review));
    drop(held);
    h.pump();
    assert_eq!(*done.lock().unwrap(), Some(true));
    assert!(h.app.agents.contains_key(&rid));
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
}

#[test]
fn each_review_round_gets_a_fresh_session() {
    let mut h = Harness::new("review-again", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let rid = agent::review_id("t-1");
    h.app.start_review("t-1");
    h.pump();
    assert!(h.app.agents.contains_key(&rid));
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
    h.app
        .edit_local("t-1", |b| b.set_state_label(Some(State::Review)));
    h.app.reconcile();
    assert!(!h.app.agents.contains_key(&rid));
    h.app.start_review("t-1");
    h.pump();
    assert!(h.app.agents.contains_key(&rid));
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
    let sessions = h.sessions(&rid);
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert_ne!(sessions[0], sessions[1]);
    assert_eq!(h.app.review_sessions["t-1"], sessions);
}

#[test]
fn reviewer_that_dies_at_startup_returns_the_bead_to_review() {
    let mut h = Harness::new("review-dies", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let rid = agent::review_id("t-1");
    h.app.start_review("t-1");
    h.pump();
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
    assert!(h.app.slots.get(&rid).is_some());
    h.dispatch_exit(&rid);
    while h.state("t-1") != Some(State::Review) {
        let ev = h.rx.recv_timeout(Duration::from_secs(20)).unwrap();
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
        }
    }
    let status = h.app.status.clone();
    h.pump();
    assert!(!h.app.agents.contains_key(&rid));
    assert_eq!(h.app.slots.get(&rid), None);
    assert_eq!(h.app.free_slots(), 2);
    assert!(
        status.starts_with("ai review of t-1 failed: reviewer exited with code 0 after "),
        "{status}"
    );
    assert!(status.ends_with(", left in review"), "{status}");
    h.app.start_review("t-1");
    h.pump();
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
}

#[test]
fn review_launch_is_dropped_when_bead_leaves_review() {
    let mut h = Harness::new("review-left", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let gate = h.gate.clone();
    let held = gate.lock().unwrap();
    let (done, record) = recorder();
    let bead = h.bead("t-1");
    h.app.launch_review(&bead, record);
    h.app
        .edit_local("t-1", |b| b.set_state_label(Some(State::Running)));
    drop(held);
    h.pump();
    let rid = agent::review_id("t-1");
    assert_eq!(*done.lock().unwrap(), Some(false));
    assert!(!h.app.agents.contains_key(&rid));
    assert_eq!(h.app.slots.get(&rid), None);
}

#[test]
fn auto_review_starts_a_reviewer_when_the_implementer_finishes() {
    let mut h = Harness::new("auto-review", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    let rid = agent::review_id("t-1");
    h.app.ready_for_review("t-1", true);
    assert!(!h.app.is_starting(&rid));
    h.app.cfg.ai_review.auto = true;
    h.app.ready_for_review("t-1", false);
    assert!(!h.app.is_starting(&rid));
    h.app.ready_for_review("t-1", true);
    assert!(h.app.is_starting(&rid));
    assert_eq!(
        h.app.status,
        "starting auto ai review of t-1 (round 1/3)..."
    );
    h.pump();
    assert!(h.app.agents.contains_key(&rid));
    assert_eq!(h.state("t-1"), Some(State::Reviewing));
    assert_eq!(h.app.auto_rounds.get("t-1"), Some(&1));
}

#[test]
fn auto_review_stops_after_max_rounds_and_resets() {
    let mut h = Harness::new("auto-review-max", &[("t-1", Some(State::Review))]);
    h.app.git.ensure_worktree("t-1", "main").unwrap();
    h.app.cfg.ai_review.auto = true;
    h.app.cfg.ai_review.max_rounds = 2;
    h.app.auto_rounds.insert("t-1".into(), 2);
    let rid = agent::review_id("t-1");
    h.app.ready_for_review("t-1", true);
    assert!(!h.app.is_starting(&rid));
    assert!(!h.app.auto_rounds.contains_key("t-1"));
    assert_eq!(
        h.app.status,
        "auto ai review of t-1: stopped after 2 round(s), left in review"
    );
    h.app.ready_for_review("t-1", true);
    assert!(h.app.is_starting(&rid));
    h.pump();
    assert_eq!(h.app.auto_rounds.get("t-1"), Some(&1));
}

#[test]
fn auto_review_skips_released_beads_and_counts_nothing_when_it_cannot_start() {
    let mut h = Harness::new(
        "auto-review-skip",
        &[("a", Some(State::Released)), ("b", Some(State::Review))],
    );
    h.app.cfg.ai_review.auto = true;
    h.app.ready_for_review("a", true);
    assert!(!h.app.is_starting(&agent::review_id("a")));
    assert!(!h.app.auto_rounds.contains_key("a"));
    h.app.ready_for_review("b", true);
    h.pump();
    assert!(!h.app.agents.contains_key(&agent::review_id("b")));
    assert!(!h.app.auto_rounds.contains_key("b"));
    assert_eq!(h.state("b"), Some(State::Review));
}

fn reviewable(name: &str) -> Harness {
    let mut h = Harness::new(name, &[("t-1", Some(State::Review))]);
    let wt = h.app.git.ensure_worktree("t-1", "main").unwrap();
    std::fs::write(wt.join("a.txt"), "a\nb\n").unwrap();
    git_in(&wt, &["commit", "-qam", "add b"]);
    h.app.open_review("t-1".into());
    h
}

fn press(h: &mut Harness, code: crossterm::event::KeyCode) {
    use crossterm::event::{KeyEvent, KeyModifiers};
    h.app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn type_text(h: &mut Harness, text: &str) {
    use crossterm::event::KeyCode;
    for c in text.chars() {
        press(h, KeyCode::Char(c));
    }
    press(h, KeyCode::Enter);
}

fn comment_and_reject(h: &mut Harness) {
    use crossterm::event::KeyCode;
    press(h, KeyCode::Tab);
    press(h, KeyCode::Char('G'));
    press(h, KeyCode::Char('c'));
    type_text(h, "rename b");
    press(h, KeyCode::Char('n'));
    let d = open_diff(h);
    assert_eq!(d.comments.len(), 1, "{}", h.app.status);
    assert_eq!(d.rejected_hunks(), 1);
}

fn open_diff(h: &Harness) -> &crate::ui::review::Diff {
    match &h.app.view {
        View::Review(d) => d,
        _ => panic!("review not open"),
    }
}

#[test]
fn review_comments_survive_leaving_and_reopening() {
    use crossterm::event::KeyCode;
    let mut h = reviewable("review-reopen");
    comment_and_reject(&mut h);
    press(&mut h, KeyCode::Char('q'));
    assert!(matches!(h.app.view, View::Board));
    h.app.open_review("t-1".into());
    let d = open_diff(&h);
    assert_eq!(d.comments.len(), 1);
    assert_eq!(d.comments[0].text, "rename b");
    assert_eq!(d.rejected_hunks(), 1);
    assert_eq!(d.files[d.sel].name, "a.txt");
    press(&mut h, KeyCode::Esc);
    assert!(h.app.saved_reviews.contains_key("t-1"));
    h.app.discard("t-1");
    assert!(h.app.saved_reviews.is_empty());
}

#[test]
fn review_reload_keeps_comments() {
    use crossterm::event::KeyCode;
    let mut h = reviewable("review-reload");
    comment_and_reject(&mut h);
    let cursor = open_diff(&h).files[1].cursor;
    press(&mut h, KeyCode::Char('g'));
    press(&mut h, KeyCode::Char('r'));
    let d = open_diff(&h);
    assert_eq!(d.comments.len(), 1);
    assert_eq!(d.rejected_hunks(), 1);
    assert_eq!(d.sel, 1);
    assert_eq!(d.files[1].cursor, cursor);
}

#[test]
fn review_x_deletes_one_comment_per_press() {
    use crossterm::event::KeyCode;
    let mut h = reviewable("review-x");
    press(&mut h, KeyCode::Tab);
    press(&mut h, KeyCode::Char('G'));
    for text in ["one", "two"] {
        press(&mut h, KeyCode::Char('c'));
        type_text(&mut h, text);
    }
    assert_eq!(open_diff(&h).comments.len(), 2, "{}", h.app.status);
    press(&mut h, KeyCode::Char('x'));
    assert_eq!(h.app.status, "deleted comment 2/2 on this line");
    assert_eq!(open_diff(&h).comments[0].text, "one");
    press(&mut h, KeyCode::Char('x'));
    assert_eq!(h.app.status, "deleted comment 1/1 on this line");
    press(&mut h, KeyCode::Char('x'));
    assert_eq!(h.app.status, "no comments here");
}

#[test]
fn review_capital_r_does_not_reload() {
    use crossterm::event::KeyCode;
    let mut h = reviewable("review-no-capital-r");
    h.app.status.clear();
    let wt = h.app.git.ensure_worktree("t-1", "main").unwrap();
    std::fs::write(wt.join("b.txt"), "new\n").unwrap();
    git_in(&wt, &["add", "b.txt"]);
    git_in(&wt, &["commit", "-qm", "add c"]);
    let files = open_diff(&h).files.len();
    press(&mut h, KeyCode::Char('R'));
    assert_eq!(open_diff(&h).files.len(), files);
    assert_eq!(h.app.status, "");
    assert_eq!(h.state("t-1"), Some(State::Review));
    press(&mut h, KeyCode::Char('g'));
    press(&mut h, KeyCode::Char('r'));
    assert!(open_diff(&h).files.len() > files);
}

#[test]
fn review_note_carries_pending_inline_comments() {
    use crossterm::event::KeyCode;
    let mut h = reviewable("review-note");
    comment_and_reject(&mut h);
    let json = open_diff(&h).feedback().unwrap();
    press(&mut h, KeyCode::Char('s'));
    type_text(&mut h, "also add tests");
    assert!(matches!(&h.app.view, View::Progress(id) if id == "t-1"));
    assert_eq!(h.app.status, "feedback and 1 inline comment(s) sent to t-1");
    h.pump();
    let prompts = h.prompts("t-1");
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let prompt = prompts[0].clone().unwrap();
    assert!(
        prompt.contains(&format!("{json}\n\nalso add tests")),
        "{prompt}"
    );
    h.app.open_review("t-1".into());
    let d = open_diff(&h);
    assert!(d.comments.is_empty());
    assert_eq!(d.rejected_hunks(), 1);
}
