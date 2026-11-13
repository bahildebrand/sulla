use super::*;

fn idle_agent(h: &mut Harness, id: &str) {
    let mut cmd = CommandBuilder::new("sleep");
    cmd.arg("30");
    let a = Agent::spawn(cmd, id, 24, 80, None, h.app.tx.clone()).unwrap();
    h.app.agents.insert(id.to_string(), a);
}

fn fired(h: &Harness) -> Vec<(Trigger, String)> {
    h.app.notifier.fired.clone()
}

#[test]
fn reviewer_and_chat_bells_do_not_notify() {
    let mut h = Harness::new("bell-quiet", &[("a", Some(State::Reviewing))]);
    let rid = agent::review_id("a");
    idle_agent(&mut h, &rid);
    h.app.on_bell(&rid);
    h.app.on_bell(CHAT_ID);
    h.app.confirm_waits(Instant::now() + WAIT_CONFIRM * 2);
    assert!(fired(&h).is_empty());
    h.app.cfg.notify.on.push(Trigger::Chat);
    h.app.notifier.reconfigure(h.app.cfg.notify.clone());
    h.app.on_bell(CHAT_ID);
    assert_eq!(fired(&h), [(Trigger::Chat, CHAT_ID.to_string())]);
}

#[test]
fn bell_then_review_notifies_review_once() {
    let mut h = Harness::new("bell-review", &[("a", Some(State::Running))]);
    idle_agent(&mut h, "a");
    let t0 = Instant::now();
    h.app.on_bell("a");
    h.app.confirm_waits(t0 + Duration::from_secs(5));
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Review)));
    h.app.ready_for_review("a", true);
    h.app.ready_for_review("a", false);
    h.app.confirm_waits(t0 + WAIT_CONFIRM * 2);
    assert_eq!(fired(&h), [(Trigger::Review, "a".to_string())]);
    assert!(h.app.waits.is_empty());
    h.app.notifier.clear("a");
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Running)));
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Review)));
    h.app.ready_for_review("a", false);
    assert_eq!(fired(&h).len(), 2);
}

#[test]
fn review_does_not_notify_while_an_ai_review_starts() {
    let mut h = Harness::new("review-auto", &[("a", Some(State::Review))]);
    h.app.git.ensure_worktree("a", "main").unwrap();
    h.app.cfg.ai_review.auto = true;
    h.app.ready_for_review("a", true);
    assert!(h.app.is_starting(&agent::review_id("a")));
    h.app.notify_review("a");
    h.pump();
    assert_eq!(h.state("a"), Some(State::Reviewing));
    h.app.notify_review("a");
    assert!(fired(&h).is_empty());
}

#[test]
fn bell_with_an_idle_agent_notifies_waiting_after_confirmation() {
    let mut h = Harness::new(
        "bell-waiting",
        &[("a", Some(State::Running)), ("b", Some(State::Running))],
    );
    idle_agent(&mut h, "a");
    idle_agent(&mut h, "b");
    let t0 = Instant::now();
    h.app.on_bell("a");
    h.app.on_bell("b");
    h.app.confirm_waits(t0 + WAIT_CONFIRM / 2);
    assert!(fired(&h).is_empty());
    h.app.agents.get_mut("b").unwrap().kill();
    h.app.confirm_waits(t0 + WAIT_CONFIRM * 2);
    assert_eq!(fired(&h), [(Trigger::Waiting, "a".to_string())]);
    assert!(h.app.waits.is_empty());
}

#[test]
fn dead_agent_notifies_dead() {
    let mut h = Harness::new("dead", &[("a", Some(State::Running))]);
    h.app.failed.insert("a".into());
    h.app.check_stalls();
    assert_eq!(h.app.stalls.get("a"), Some(&Stall::Dead));
    assert_eq!(fired(&h), [(Trigger::Dead, "a".to_string())]);
}

fn pr_comment(id: &str, author: &str) -> crate::pr::PrComment {
    crate::pr::PrComment {
        id: id.into(),
        author: author.into(),
        body: "rename this".into(),
        path: None,
        line: None,
        created_at: None,
        approval: false,
        bot: author.ends_with("[bot]"),
    }
}

#[test]
fn pr_comments_from_bots_or_the_user_do_not_notify() {
    let mut h = Harness::new("pr-bots", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let mut s = pr_status(crate::pr::PrState::Open, &[]);
    s.viewer = Some("me".into());
    s.comments = vec![pr_comment("c1", "me"), pr_comment("c2", "ci[bot]")];
    h.app.on_pr("a", 7, Ok(s.clone()));
    assert_eq!(h.app.status, "PR #7 for a: 2 new comments, U to revise");
    assert_eq!(h.app.pr_watch.beads["a"].comments.len(), 2);
    assert!(fired(&h).is_empty());
    s.comments.push(pr_comment("c3", "alice"));
    h.app.on_pr("a", 7, Ok(s));
    assert_eq!(fired(&h), [(Trigger::Pr, "a".to_string())]);
}

#[test]
fn failing_checks_notify_once_per_head() {
    let mut h = Harness::new("pr-checks", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let poll = |h: &mut Harness, head: &str, checks: crate::pr::Checks| {
        let mut s = pr_status(crate::pr::PrState::Open, &[]);
        s.head = Some(head.into());
        s.checks = checks;
        h.app.on_pr("a", 7, Ok(s));
        h.app.notifier.clear("a");
        fired(h).len()
    };
    let failing =
        |names: &[&str]| crate::pr::Checks::Failing(names.iter().map(|n| n.to_string()).collect());
    assert_eq!(poll(&mut h, "h1", failing(&["test"])), 1);
    assert_eq!(poll(&mut h, "h1", failing(&["lint", "test"])), 1);
    assert_eq!(poll(&mut h, "h1", crate::pr::Checks::Pending), 1);
    assert_eq!(poll(&mut h, "h1", failing(&["test"])), 1);
    assert_eq!(poll(&mut h, "h2", crate::pr::Checks::Pending), 1);
    assert_eq!(poll(&mut h, "h2", failing(&["test"])), 2);
    assert_eq!(poll(&mut h, "h2", crate::pr::Checks::Passing), 2);
}

#[test]
fn pr_approved_or_closed_notifies() {
    let mut h = Harness::new("pr-approved", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let mut s = pr_status(crate::pr::PrState::Open, &[]);
    let mut approve = pr_comment("r1", "bob");
    approve.approval = true;
    s.comments.push(approve);
    h.app.on_pr("a", 7, Ok(s.clone()));
    assert_eq!(
        h.app.status,
        "PR #7 for a: 1 new comment, approved, ready to merge"
    );
    assert_eq!(fired(&h).len(), 1);
    h.app.notifier.clear("a");
    s.state = crate::pr::PrState::Closed;
    h.app.on_pr("a", 7, Ok(s));
    assert_eq!(h.app.status, "PR #7 for a: closed without merging");
    assert_eq!(fired(&h).len(), 2);
}

#[test]
fn present_user_gets_a_status_chime_instead_of_a_bell() {
    let mut h = Harness::new("notify-present", &[("a", Some(State::Review))]);
    h.app.notify(Trigger::Review, "a", "A");
    assert_eq!(fired(&h), [(Trigger::Review, "a".to_string())]);
    assert_eq!(h.app.chime(), Some("🔔 a ready for review"));
    h.app
        .notifier
        .flush(Instant::now() + Duration::from_secs(60));
    assert!(h.app.notifier.sent.is_empty());
}

#[test]
fn away_user_is_notified_and_greeted_with_a_summary() {
    let mut h = Harness::new(
        "notify-away",
        &[("a", Some(State::Review)), ("b", Some(State::Running))],
    );
    let now = Instant::now();
    h.app.last_input = now - Duration::from_secs(300);
    h.app.notify(Trigger::Review, "a", "A");
    h.app.notify(Trigger::Waiting, "b", "B");
    assert_eq!(h.app.chime(), None);
    h.app.notifier.flush(now + Duration::from_secs(10));
    assert_eq!(h.app.notifier.sent.len(), 1);
    assert_eq!(h.app.notifier.sent[0].0, "sulla: 2 need you");
    h.app.on_input(now);
    assert_eq!(
        h.app.status,
        "while you were away: 1 waiting, 1 ready for review"
    );
    h.app.status.clear();
    h.app.on_input(now);
    assert!(h.app.status.is_empty());
}

#[test]
fn losing_focus_counts_as_away_and_regaining_it_returns() {
    let mut h = Harness::new("notify-focus", &[("a", Some(State::Review))]);
    let now = Instant::now();
    h.app.on_input(now);
    assert_eq!(h.app.focused, None);
    h.app.on_focus(false, now);
    h.app.notify(Trigger::Review, "a", "A");
    assert_eq!(h.app.chime(), None);
    h.app.on_input(now);
    assert_eq!(h.app.focused, Some(true));
    assert_eq!(h.app.status, "while you were away: 1 ready for review");
    h.app.on_focus(false, now);
    h.app.status.clear();
    h.app.on_focus(true, now);
    assert!(h.app.status.is_empty());
    assert!(h.app.notifier.sent.is_empty());
}
