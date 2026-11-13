use super::*;

#[test]
fn new_bead_form_steps_through_acceptance_before_priority() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut h = Harness::new("new-bead-acceptance", &[]);
    h.app.mode = Mode::NewBead(NewBeadForm::new(Vec::new()));
    let key = |h: &mut Harness, code: KeyCode| {
        h.app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    };
    key(&mut h, KeyCode::Tab);
    key(&mut h, KeyCode::Char('d'));
    key(&mut h, KeyCode::Enter);
    key(&mut h, KeyCode::Char('a'));
    h.app.on_paste("b\r\nc");
    key(&mut h, KeyCode::Enter);
    key(&mut h, KeyCode::Char('0'));
    let Mode::NewBead(form) = &h.app.mode else {
        panic!("form closed");
    };
    assert_eq!(form.description, "d");
    assert_eq!(form.acceptance, "ab\nc");
    assert_eq!(form.priority, 0);
    assert_eq!(form.field, 3);
}

#[test]
fn every_view_renders_at_any_terminal_size() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut h = Harness::new(
        "render",
        &[
            ("r-1", Some(State::Backlog)),
            ("r-2", None),
            ("r-3", Some(State::Running)),
            ("r-4", Some(State::Review)),
            ("r-5", Some(State::Held)),
        ],
    );
    for b in &mut h.app.all {
        b.description = "a fairly long description line that will need to be cut".into();
    }
    let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n ctx\n";
    let mut d = crate::ui::review::Diff::parse("r-4", diff);
    d.move_cursor(5);
    d.add_comment("please rename", 1);
    let views = [View::Board, View::Progress("r-3".into()), View::Review(d)];
    let modes = || {
        vec![
            Mode::Normal,
            Mode::Insert,
            Mode::Help { scroll: 3 },
            Mode::Health,
            Mode::Config(Box::new(
                crate::ui::config::ConfigView::new(&h.app.layers, &h.app.cfg, &h.app.reload)
                    .unwrap(),
            )),
            Mode::NewBead(NewBeadForm::new(vec!["rust".into()])),
            Mode::Inspect {
                id: "r-1".into(),
                text: "TITLE\n\nDESCRIPTION\n  body\n".into(),
                scroll: 0,
            },
            Mode::Confirm {
                action: Action::Discard("r-1".into()),
                msg: "discard r-1? y/n".into(),
            },
            Mode::Input {
                kind: InputKind::Command,
                buf: "max 3".into(),
            },
            Mode::Input {
                kind: InputKind::Comment(2),
                buf: "a comment".into(),
            },
        ]
    };
    for view in views {
        h.app.view = view;
        for zoom in [false, true] {
            h.app.zoom = zoom;
            for mode in modes() {
                h.app.mode = mode;
                for (w, ht) in [(200, 50), (80, 24), (24, 8), (6, 3), (1, 1)] {
                    let mut t = Terminal::new(TestBackend::new(w, ht)).unwrap();
                    t.draw(|f| crate::ui::draw(f, &h.app)).unwrap();
                }
            }
        }
    }
}

fn rows(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

fn column_of(c: Column) -> usize {
    Column::ALL.iter().position(|x| *x == c).unwrap()
}

#[test]
fn local_workflow_hides_an_empty_awaiting_pr_column() {
    let mut h = Harness::new("hide-awaiting-pr", &[("w-1", Some(State::Review))]);
    assert_eq!(h.app.cfg.workflow, Workflow::Local);
    let top = rows(&screen(&h.app, 120, 40)).concat();
    assert!(!top.contains("Awaiting PR"));
    for c in Column::ALL.iter().filter(|c| **c != Column::AwaitingPr) {
        assert!(top.contains(c.title()), "{} missing", c.title());
    }
    assert_eq!(h.app.visible_columns().len(), 5);

    h.app.all[0].labels.push("pr:7".into());
    assert!(
        rows(&screen(&h.app, 120, 40))
            .concat()
            .contains("Awaiting PR")
    );
    assert_eq!(h.app.visible_columns().len(), 6);

    h.app.all[0].labels.retain(|l| l != "pr:7");
    h.app.cfg.workflow = Workflow::Pr;
    assert!(
        rows(&screen(&h.app, 120, 40))
            .concat()
            .contains("Awaiting PR")
    );
}

#[test]
fn column_navigation_skips_the_hidden_awaiting_pr_column() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut h = Harness::new("nav-awaiting-pr", &[("n-1", Some(State::Review))]);
    let key = |h: &mut Harness, c: char| {
        h.app
            .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    };
    h.app.col = column_of(Column::Review);
    key(&mut h, 'l');
    assert_eq!(h.app.col, column_of(Column::Done));
    key(&mut h, 'l');
    assert_eq!(h.app.col, column_of(Column::Done));
    key(&mut h, 'h');
    assert_eq!(h.app.col, column_of(Column::Review));
    h.app.col = 0;
    key(&mut h, 'h');
    assert_eq!(h.app.col, 0);

    h.app.col = column_of(Column::AwaitingPr);
    h.app.clamp();
    assert_eq!(h.app.col, column_of(Column::Review));

    h.app.zoom = true;
    let nav = rows(&screen(&h.app, 120, 40)).concat();
    assert!(nav.contains("Ready for Review"));
    assert!(!nav.contains(crate::ui::theme::column_icon(Column::AwaitingPr)));
}

#[test]
fn long_titles_end_in_an_ellipsis_and_keep_the_bead_id() {
    let mut h = Harness::new("long-title", &[("lt-1", None)]);
    h.app.all[0].title = "a very long title that cannot possibly fit in one column".into();
    for zoom in [false, true] {
        h.app.zoom = zoom;
        h.app.col = column_of(Column::InProgress);
        let screen = rows(&screen(&h.app, 120, 40));
        let line = screen
            .iter()
            .find(|l| l.contains("a very"))
            .expect("card headline");
        if zoom {
            assert!(line.contains(&h.app.all[0].title));
        } else {
            assert!(line.contains("…"), "{line}");
            assert!(line.contains("… lt-1"), "{line}");
            assert!(!line.contains("one column"), "{line}");
        }
        assert!(line.contains("lt-1"), "{line}");
    }
}

#[test]
fn pr_phase_text_moves_to_the_activity_line() {
    let mut h = Harness::new("pr-phase", &[("pp-1", Some(State::Review))]);
    h.app.all[0].labels.push("pr:7".into());
    h.app
        .pr_watch
        .beads
        .insert("pp-1".into(), crate::pr::Seen::default());
    let screen = rows(&screen(&h.app, 200, 40));
    let head = screen
        .iter()
        .position(|l| l.contains("pp-1"))
        .expect("headline");
    assert!(!screen[head].contains("revised"), "{}", screen[head]);
    assert!(
        screen[head + 1].contains("revised, Enter to review"),
        "{}",
        screen[head + 1]
    );
}
