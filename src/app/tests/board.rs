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
