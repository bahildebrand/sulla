use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn press(h: &mut Harness, c: char) {
    h.app
        .on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
}

fn cells(h: &Harness) -> Vec<Vec<String>> {
    let (w, ht) = h.app.term;
    let mut t = Terminal::new(TestBackend::new(w, ht)).unwrap();
    t.draw(|f| crate::ui::draw(f, &h.app)).unwrap();
    let buf = t.backend().buffer();
    (0..ht)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
        .collect()
}

fn screen(h: &Harness) -> Vec<String> {
    cells(h).into_iter().map(|r| r.concat()).collect()
}

fn first_help_row(h: &Harness) -> String {
    let rows = cells(h);
    let (title, at) = rows
        .iter()
        .enumerate()
        .find_map(|(y, r)| {
            let x = r.windows(5).position(|w| w.concat() == "sulla")?;
            Some((y, x))
        })
        .expect("help popup drawn");
    let left = rows[title][..at].iter().rposition(|c| c == "╭").unwrap();
    let right = at + rows[title][at..].iter().position(|c| c == "╮").unwrap();
    rows[title + 1..]
        .iter()
        .map(|r| r[left + 1..right].concat().trim().to_string())
        .find(|r| !r.is_empty())
        .unwrap_or_default()
}

fn review_diff() -> View {
    let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    View::Review(crate::ui::review::Diff::parse("r-1", diff))
}

#[test]
fn help_opens_at_the_section_for_the_current_view() {
    let mut h = Harness::new("help-section", &[("r-1", Some(State::Review))]);
    for (term, view, section) in [
        ((120, 40), View::Board, "Board"),
        ((120, 40), View::Progress("r-1".into()), "Progress"),
        ((120, 40), review_diff(), "Review"),
        ((80, 24), review_diff(), "Review"),
    ] {
        h.app.term = term;
        h.app.view = view;
        h.app.mode = Mode::Normal;
        press(&mut h, '?');
        assert!(matches!(h.app.mode, Mode::Help { .. }));
        let top = first_help_row(&h);
        assert!(
            top.starts_with(&format!("{section} ─")),
            "{section} at {term:?}: {top}"
        );
    }
}

#[test]
fn help_from_config_starts_at_config_and_returns_to_it() {
    let mut h = Harness::new("help-config", &[]);
    h.app.term = (120, 40);
    h.app.open_config();
    assert!(matches!(h.app.mode, Mode::Config(_)));
    press(&mut h, '?');
    assert!(matches!(
        h.app.mode,
        Mode::Help {
            config: Some(_),
            ..
        }
    ));
    assert!(first_help_row(&h).starts_with("Config ─"));
    press(&mut h, 'j');
    assert!(matches!(h.app.mode, Mode::Help { .. }));
    press(&mut h, 'q');
    assert!(matches!(h.app.mode, Mode::Config(_)));
}

#[test]
fn every_parser_command_is_listed_in_help() {
    let listed: Vec<&str> = crate::ui::HELP
        .iter()
        .flat_map(|(k, d)| k.split_whitespace().chain(d.split_whitespace()))
        .map(|w| w.trim_matches(|c| c == '(' || c == ')' || c == ','))
        .filter(|w| w.starts_with(':'))
        .collect();
    for (names, _) in super::super::input::COMMANDS {
        let name = format!(":{}", names[0]);
        assert!(listed.contains(&name.as_str()), "{name} missing from HELP");
    }
    assert!(listed.contains(&":browse"));
    let help: String = crate::ui::HELP
        .iter()
        .map(|(k, d)| format!("{k} {d}\n"))
        .collect();
    assert!(help.contains(":browse restart"));
}

#[test]
fn commands_dispatch_from_the_table_and_report_usage() {
    let mut h = Harness::new("help-commands", &[]);
    h.app.command("nope");
    assert_eq!(h.app.status, "unknown command: nope");
    h.app.command("new");
    assert_eq!(h.app.status, "usage: :new <title>");
    h.app.command("max 4");
    assert_eq!(h.app.cfg.max_agents, 4);
    h.app.command("pause");
    assert!(h.app.paused);
    h.app.command("resume");
    assert!(!h.app.paused);
}

#[test]
fn idle_normal_status_line_shows_key_hints() {
    let mut h = Harness::new("help-hints", &[]);
    h.app.term = (120, 10);
    h.app.status.clear();
    let bottom = screen(&h).pop().unwrap();
    assert!(bottom.contains("command"), "{bottom}");
    assert!(bottom.contains("filter"), "{bottom}");
    h.app.status = "something happened".into();
    let bottom = screen(&h).pop().unwrap();
    assert!(bottom.contains("something happened"), "{bottom}");
    assert!(!bottom.contains("filter"), "{bottom}");
}
