use super::*;

fn chain(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn publish_chain_takes_unpublished_review_parents() {
    let review = Some(State::Review);
    let h = Harness::new("chain", &[("a", review), ("b", review), ("c", review)]);
    h.stack("b", "a");
    h.stack("c", "b");
    assert_eq!(h.app.publish_chain("c"), chain(&["a", "b", "c"]));
    assert_eq!(h.app.publish_chain("a"), chain(&["a"]));
}

#[test]
fn publish_chain_stops_at_awaiting_or_unreviewed_parent() {
    let h = Harness::new(
        "chain-stop",
        &[
            ("a", Some(State::Pr)),
            ("b", Some(State::Review)),
            ("c", Some(State::Review)),
            ("x", Some(State::Running)),
            ("y", Some(State::Review)),
        ],
    );
    h.stack("b", "a");
    h.stack("c", "b");
    h.stack("y", "x");
    assert_eq!(h.app.publish_chain("c"), chain(&["b", "c"]));
    assert_eq!(h.app.pr_base("b"), "sulla/a");
    assert_eq!(h.app.publish_chain("y"), chain(&["y"]));
}

#[test]
fn confirm_publish_offers_the_whole_stack() {
    let review = Some(State::Review);
    let mut h = Harness::new("chain-confirm", &[("a", review), ("b", review)]);
    h.stack("b", "a");
    h.app.view = View::Progress("b".into());
    let blockers = Ok(vec![vec![], vec!["a".to_string()]]);
    h.app
        .confirm_publish("b".into(), chain(&["a", "b"]), blockers);
    let Mode::Confirm {
        msg,
        action: Action::Publish(c),
    } = &h.app.mode
    else {
        panic!("no confirm, status: {}", h.app.status);
    };
    assert_eq!(c, &chain(&["a", "b"]));
    assert!(msg.contains("sulla/a, sulla/b"), "{msg}");
}

#[test]
fn confirm_publish_names_blockers_below_in_the_stack() {
    let review = Some(State::Review);
    let mut h = Harness::new("chain-blocked", &[("a", review), ("b", review)]);
    h.stack("b", "a");
    h.app.view = View::Progress("b".into());
    let blockers = Ok(vec![vec!["q".to_string()], vec!["a".to_string()]]);
    h.app
        .confirm_publish("b".into(), chain(&["a", "b"]), blockers);
    assert!(matches!(h.app.mode, Mode::Normal));
    assert_eq!(
        h.app.status,
        "cannot publish b: a below it is blocked by open q"
    );
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    assert_eq!(h.app.status, "cannot publish b: blocked by open a");
}

#[test]
fn confirm_publish_stacks_onto_a_parent_revising_its_pr() {
    let mut h = Harness::new(
        "chain-revising",
        &[
            ("a", Some(State::Revise)),
            ("b", Some(State::Review)),
            ("c", Some(State::Review)),
        ],
    );
    h.app.all[0].labels.push("pr:7".into());
    h.stack("b", "a");
    h.stack("c", "b");
    h.app.view = View::Progress("c".into());
    assert_eq!(h.app.publish_chain("c"), chain(&["b", "c"]));
    assert_eq!(h.app.pr_base("b"), "sulla/a");
    let blockers = Ok(vec![vec!["a".to_string()], vec!["b".to_string()]]);
    h.app
        .confirm_publish("c".into(), chain(&["b", "c"]), blockers);
    let Mode::Confirm {
        msg,
        action: Action::Publish(c),
    } = &h.app.mode
    else {
        panic!("no confirm, status: {}", h.app.status);
    };
    assert_eq!(c, &chain(&["b", "c"]));
    assert!(msg.contains("onto sulla/a"), "{msg}");
}

fn commit_on(dir: &Path, parent: &str, msg: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["commit-tree", "HEAD^{tree}", "-p", parent, "-m", msg])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "commit-tree {parent}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn confirmed_publish(h: &Harness) -> (String, Vec<String>) {
    let Mode::Confirm {
        msg,
        action: Action::Publish(c),
    } = &h.app.mode
    else {
        panic!("no confirm, status: {}", h.app.status);
    };
    (msg.clone(), c.clone())
}

#[test]
fn confirm_publish_restacks_onto_a_pr_blocker_that_lost_its_stack_parent() {
    let mut h = Harness::new(
        "adopt-stale",
        &[("a", Some(State::Pr)), ("b", Some(State::Review))],
    );
    h.app.all[0].labels.push("pr:7".into());
    let old = commit_on(&h.dir, "main", "a v1");
    let child = commit_on(&h.dir, &old, "b");
    let amended = commit_on(&h.dir, "main", "a v2");
    git_in(&h.dir, &["branch", "sulla/a", &amended]);
    git_in(&h.dir, &["branch", "sulla/b", &child]);
    git_in(&h.dir, &["update-ref", "refs/sulla/stack/b", &old]);
    assert_eq!(h.app.git.stack_parent("b"), None);
    h.app.view = View::Progress("b".into());
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    let (msg, c) = confirmed_publish(&h);
    assert_eq!(c, chain(&["b"]));
    assert!(msg.contains("against sulla/a"), "{msg}");
    assert_eq!(h.app.git.stack_parent("b").as_deref(), Some("a"));
    assert_eq!(git_in(&h.dir, &["rev-parse", "refs/sulla/stack/b"]), old);
}

#[test]
fn confirm_publish_stacks_onto_a_pr_blocker_from_the_fork_point() {
    let mut h = Harness::new(
        "adopt-fresh",
        &[("a", Some(State::Revise)), ("b", Some(State::Review))],
    );
    h.app.all[0].labels.push("pr:7".into());
    let a = commit_on(&h.dir, "main", "a");
    let b = commit_on(&h.dir, "main", "b");
    git_in(&h.dir, &["branch", "sulla/a", &a]);
    git_in(&h.dir, &["branch", "sulla/b", &b]);
    h.app.view = View::Progress("b".into());
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    let (msg, _) = confirmed_publish(&h);
    assert!(msg.contains("against sulla/a"), "{msg}");
    assert_eq!(h.app.git.stack_parent("b").as_deref(), Some("a"));
    let main = git_in(&h.dir, &["rev-parse", "main"]);
    assert_eq!(git_in(&h.dir, &["rev-parse", "refs/sulla/stack/b"]), main);
}

#[test]
fn confirm_publish_keeps_blocking_on_a_blocker_without_a_pr() {
    let mut h = Harness::new(
        "adopt-none",
        &[("a", Some(State::Running)), ("b", Some(State::Review))],
    );
    let a = commit_on(&h.dir, "main", "a");
    git_in(&h.dir, &["branch", "sulla/a", &a]);
    git_in(&h.dir, &["branch", "sulla/b", "main"]);
    h.app.view = View::Progress("b".into());
    h.app
        .confirm_publish("b".into(), chain(&["b"]), Ok(vec![vec!["a".to_string()]]));
    assert!(matches!(h.app.mode, Mode::Normal));
    assert_eq!(h.app.status, "cannot publish b: blocked by open a");
    assert_eq!(h.app.git.stack_parent("b"), None);
}

#[test]
fn failed_parent_publish_skips_the_rest_of_the_stack() {
    let review = Some(State::Review);
    let mut h = Harness::new("chain-fail", &[("a", review), ("b", review)]);
    h.stack("b", "a");
    h.app.publish(chain(&["a", "b"]));
    assert!(h.app.publishing.contains("a") && h.app.publishing.contains("b"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !h.app.publishing.is_empty() {
        let wait = deadline.saturating_duration_since(Instant::now());
        let ev = h.rx.recv_timeout(wait).expect("publish did not finish");
        if matches!(ev, Event::Finished(_)) {
            h.app.dispatch(ev);
        }
    }
    assert!(
        h.app.status.ends_with(", did not publish b"),
        "{}",
        h.app.status
    );
    assert_eq!(h.state("a"), review);
    assert_eq!(h.state("b"), review);
}

#[test]
fn publish_conflict_hands_off_to_the_agent_then_republishes() {
    let review = Some(State::Review);
    let mut h = Harness::new("publish-conflict", &[("a", review), ("b", review)]);
    h.app.cfg.workflow = Workflow::Pr;
    h.app.cfg.pr.tool = PrTool::Graphite;
    h.stack("b", "a");
    h.app.git.ensure_worktree("b", "sulla/a").unwrap();
    let conflict = git::RebaseConflict {
        base: "sulla/a".into(),
        detail: "could not apply".into(),
    };
    h.app.publishing.insert("a".into());
    h.app
        .pushed("a".into(), Err(conflict.into()), Some("b".into()));
    h.pump();
    assert!(!h.app.publishing.contains("a"));
    assert_eq!(h.app.republishing.get("a").map(String::as_str), Some("b"));
    assert_eq!(h.state("a"), Some(State::Running));
    h.app.republish_resolved(&[]);
    assert!(h.app.publishing.is_empty());
    h.app.edit_local("a", |b| b.set_state_label(review));
    h.app.republish_resolved(&["a".to_string()]);
    assert!(h.app.republishing.is_empty());
    assert!(h.app.publishing.contains("a") && h.app.publishing.contains("b"));
}

#[test]
fn publish_conflict_is_reported_without_a_republish_target() {
    let review = Some(State::Review);
    let mut h = Harness::new("publish-conflict-off", &[("a", review)]);
    let conflict = git::RebaseConflict {
        base: "main".into(),
        detail: "could not apply".into(),
    };
    h.app.publishing.insert("a".into());
    h.app.pushed("a".into(), Err(conflict.into()), None);
    assert!(h.app.republishing.is_empty());
    assert_eq!(h.state("a"), review);
    assert!(
        h.app.status.contains("rebase onto main failed"),
        "{}",
        h.app.status
    );
}

#[test]
fn open_pr_needs_a_valid_pr_label() {
    let mut h = Harness::new("open-pr", &[("a", Some(State::Pr)), ("b", Some(State::Pr))]);
    h.app.all[1].labels.push("pr:x".into());
    h.app.open_pr("a");
    assert_eq!(h.app.status, "a has no PR");
    h.app.open_pr("b");
    assert_eq!(h.app.status, "b has an invalid PR label pr:x");
}

#[test]
fn pr_feedback_notifies_without_queueing_a_revision() {
    let mut h = Harness::new("pr-feedback", &[("a", Some(State::Pr))]);
    h.app.all[0].labels.push("pr:7".into());
    let comment = crate::pr::PrComment {
        id: "c1".into(),
        author: "rev".into(),
        body: "rename this".into(),
        path: None,
        line: None,
        created_at: None,
        approval: false,
        bot: false,
    };
    let status = crate::pr::PrStatus {
        state: crate::pr::PrState::Open,
        comments: vec![comment],
        checks: crate::pr::Checks::None,
        head: None,
        viewer: None,
    };
    h.app.on_pr("a", 7, Ok(status));
    assert_eq!(h.state("a"), Some(State::Pr));
    assert_eq!(h.app.status, "PR #7 for a: 1 new comment, U to revise");
    assert_eq!(h.app.pr_watch.beads["a"].pending.count(), 1);
}

#[test]
fn merged_pr_restacks_children_once_the_fetch_finishes() {
    let mut h = Harness::new(
        "merge-fetch",
        &[("p", Some(State::Pr)), ("c", Some(State::Review))],
    );
    h.app.cfg.workflow = Workflow::Pr;
    h.app.all[0].labels.push("pr:7".into());
    worked(&h, "p", "main");
    h.app.git.ensure_stacked_worktree("c", "p").unwrap();
    worked(&h, "c", "sulla/p");
    git_in(&h.dir, &["merge", "-q", "--ff-only", "sulla/p"]);
    h.app
        .on_pr("p", 7, Ok(pr_status(crate::pr::PrState::Merged, &[])));
    assert!(h.app.status.starts_with("PR #7 merged"), "{}", h.app.status);
    assert!(!h.app.status.contains("restacked"), "{}", h.app.status);
    assert_eq!(h.app.background_pending, 1);
    assert_eq!(h.app.git.stack_parent("c").as_deref(), Some("p"));
    h.pump();
    assert!(h.app.status.starts_with("PR #7 merged"), "{}", h.app.status);
    assert!(h.app.status.ends_with(", restacked c"), "{}", h.app.status);
    assert_eq!(h.app.git.stack_parent("c"), None);
}

fn queue_revision(h: &mut Harness) -> Bead {
    h.app.cfg.workflow = Workflow::Pr;
    h.app.all[0].labels.push("pr:7".into());
    worked(h, "a", "main");
    h.app
        .on_pr("a", 7, Ok(pr_status(crate::pr::PrState::Open, &["c1"])));
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Revise)));
    h.bead("a")
}

#[test]
fn revision_starts_once_the_fetch_and_rebase_finish() {
    let mut h = Harness::new("revise-fetch", &[("a", Some(State::Pr))]);
    let a = queue_revision(&mut h);
    h.app.revise(&a);
    assert!(h.app.is_starting("a"));
    assert_eq!(h.app.background_pending, 1);
    assert_eq!(h.state("a"), Some(State::Revise));
    assert_eq!(h.app.pr_watch.beads["a"].pending.count(), 1);
    h.pump();
    assert_eq!(h.state("a"), Some(State::Running));
    assert_eq!(h.app.pr_watch.beads["a"].pending.count(), 0);
    assert!(h.app.pr_watch.beads["a"].revising.is_some());
    assert!(!git_in(&h.dir, &["rev-parse", "refs/sulla/revise/a"]).is_empty());
    assert!(h.app.agents.contains_key("a"));
}

#[test]
fn revision_is_dropped_when_the_bead_leaves_revise_during_the_fetch() {
    let mut h = Harness::new("revise-left", &[("a", Some(State::Pr))]);
    let a = queue_revision(&mut h);
    h.app.revise(&a);
    h.app
        .edit_local("a", |b| b.set_state_label(Some(State::Pr)));
    h.pump();
    assert!(!h.app.is_starting("a"));
    assert_eq!(h.state("a"), Some(State::Pr));
    assert_eq!(h.app.pr_watch.beads["a"].pending.count(), 1);
    assert!(git_in(&h.dir, &["for-each-ref", "refs/sulla/revise"]).is_empty());
    assert!(!h.app.agents.contains_key("a"));
}

#[test]
fn published_revision_replies_on_the_pr_in_the_background() {
    let mut h = Harness::new("reply-fetch", &[("a", Some(State::Pr))]);
    h.app.cfg.pr.reply = true;
    queue_revision(&mut h);
    h.pump();
    h.app.pr_watch.beads.get_mut("a").unwrap().take();
    let a = h.bead("a");
    h.app.published(
        "a",
        a,
        Ok(crate::pr::Pr {
            number: 7,
            url: "u".into(),
        }),
    );
    assert!(h.app.pr_watch.beads["a"].revising.is_none());
    assert_eq!(h.app.background_pending, 1);
    assert_eq!(h.app.status, "published a as PR #7 u");
    h.pump();
    assert_eq!(h.app.background_pending, 0);
    assert_eq!(h.app.status, "published a as PR #7 u");
}
