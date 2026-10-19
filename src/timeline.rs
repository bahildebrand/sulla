use std::fmt::Write;

use chrono::{DateTime, Duration, Local, Utc};

use crate::beads::{Bead, Comment, STATE_EVENT_PREFIX};
use crate::ui::review::{self, Verdict};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub at: DateTime<Utc>,
    pub state: String,
    pub reason: String,
    pub dur: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiVerdict {
    pub at: DateTime<Utc>,
    pub verdict: Verdict,
    pub comments: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    pub steps: Vec<Step>,
    pub verdicts: Vec<AiVerdict>,
    pub totals: Vec<(String, Duration)>,
    pub rounds: usize,
    pub closed: Option<DateTime<Utc>>,
}

fn seq(id: &str) -> u64 {
    id.rsplit('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn reason(description: &str) -> String {
    description
        .lines()
        .find_map(|l| l.strip_prefix("Reason:"))
        .map(str::trim)
        .filter(|r| *r != "sulla")
        .unwrap_or("")
        .to_string()
}

pub fn build(bead: &Bead, events: &[Bead], comments: &[Comment], now: DateTime<Utc>) -> Timeline {
    let mut changes: Vec<(&Bead, DateTime<Utc>, &str)> = events
        .iter()
        .filter(|e| e.issue_type == "event" && e.parent.as_deref() == Some(bead.id.as_str()))
        .filter_map(|e| {
            let state = e.title.strip_prefix(STATE_EVENT_PREFIX)?.trim();
            Some((e, e.created_at?, state))
        })
        .collect();
    changes.sort_by_key(|(e, at, _)| (*at, seq(&e.id)));

    let mut steps: Vec<Step> = Vec::new();
    if let Some(created) = bead.created_at
        && changes.first().is_none_or(|(_, at, _)| created < *at)
    {
        steps.push(Step {
            at: created,
            state: "pending".into(),
            reason: "created".into(),
            dur: Duration::zero(),
        });
    }
    steps.extend(changes.iter().map(|(e, at, state)| Step {
        at: *at,
        state: state.to_string(),
        reason: reason(&e.description),
        dur: Duration::zero(),
    }));

    let closed = (bead.status == "closed")
        .then(|| bead.closed_at.or(bead.updated_at))
        .flatten();
    let end = closed.unwrap_or(now);
    let ends: Vec<DateTime<Utc>> = steps
        .iter()
        .skip(1)
        .map(|s| s.at)
        .chain([end.max(steps.last().map_or(end, |s| s.at))])
        .collect();
    for (s, e) in steps.iter_mut().zip(ends) {
        s.dur = (e - s.at).max(Duration::zero());
    }

    let mut totals: Vec<(String, Duration)> = Vec::new();
    for s in &steps {
        match totals.iter_mut().find(|(k, _)| *k == s.state) {
            Some((_, d)) => *d += s.dur,
            None => totals.push((s.state.clone(), s.dur)),
        }
    }

    let rounds = steps
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            s.state == "review"
                && !i
                    .checked_sub(1)
                    .is_some_and(|p| matches!(steps[p].state.as_str(), "review" | "reviewing"))
        })
        .count();

    let mut verdicts: Vec<AiVerdict> = comments
        .iter()
        .filter_map(|c| {
            let out = review::parse_ai_review(&c.text)?;
            Some(AiVerdict {
                at: c.created_at?,
                verdict: out.verdict,
                comments: out.comments.len(),
            })
        })
        .collect();
    verdicts.sort_by_key(|v| v.at);

    Timeline {
        steps,
        verdicts,
        totals,
        rounds,
        closed,
    }
}

pub fn fmt_span(d: Duration) -> String {
    let s = d.num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{:02}h", s / 86400, s % 86400 / 3600),
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%m-%d %H:%M:%S")
        .to_string()
}

fn verdict_text(v: &AiVerdict) -> String {
    let word = match v.verdict {
        Verdict::Approve => "approve",
        Verdict::Changes => "changes",
    };
    format!("{word}, {} comment(s)", v.comments)
}

pub fn render(bead: &Bead, t: &Timeline) -> String {
    let mut out = format!("{}: {}\n\nTIMELINE\n", bead.id, bead.title);
    if t.steps.is_empty() {
        out.push_str("  no state changes recorded\n");
    }
    let mut verdicts = t.verdicts.iter().peekable();
    let mut push_verdicts = |out: &mut String, until: Option<DateTime<Utc>>| {
        while let Some(v) = verdicts.next_if(|v| until.is_none_or(|u| v.at <= u)) {
            let _ = writeln!(
                out,
                "  {}  {:<10} {:>7}  {}",
                stamp(v.at),
                "ai review",
                "",
                verdict_text(v)
            );
        }
    };
    for s in &t.steps {
        push_verdicts(&mut out, Some(s.at));
        let _ = writeln!(
            out,
            "  {}  {:<10} {:>7}  {}",
            stamp(s.at),
            s.state,
            fmt_span(s.dur),
            s.reason
        );
    }
    push_verdicts(&mut out, None);
    if let Some(c) = t.closed {
        let _ = writeln!(out, "  {}  {:<10}", stamp(c), "closed");
    }

    out.push_str("\nTIME IN STATE\n");
    let max = t
        .totals
        .iter()
        .map(|(_, d)| d.num_seconds())
        .max()
        .unwrap_or(0)
        .max(1);
    for (state, d) in &t.totals {
        let bar = (d.num_seconds() * 40 / max) as usize;
        let _ = writeln!(
            out,
            "  {state:<10} {:>7}  {}",
            fmt_span(*d),
            "█".repeat(bar.max(usize::from(d.num_seconds() > 0)))
        );
    }
    let total = t.totals.iter().map(|(_, d)| *d).sum::<Duration>();
    let _ = writeln!(out, "  {:<10} {:>7}", "total", fmt_span(total));

    let approved = t
        .verdicts
        .iter()
        .filter(|v| v.verdict == Verdict::Approve)
        .count();
    let _ = writeln!(out, "\nREVIEW ROUNDS  {}", t.rounds);
    let _ = writeln!(
        out,
        "AI REVIEWS     {} ({approved} approve, {} changes)",
        t.verdicts.len(),
        t.verdicts.len() - approved
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead(json: &str) -> Bead {
        serde_json::from_str(json).unwrap()
    }

    fn event(id: &str, state: &str, at: &str, reason: &str) -> Bead {
        let parent = id.split('.').next().unwrap();
        bead(&format!(
            r#"{{"id":"{id}","title":"State change: sulla → {state}","status":"closed","issue_type":"event","parent":"{parent}","created_at":"{at}","description":"Changed sulla\n\nReason: {reason}"}}"#
        ))
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn builds_durations_rounds_and_verdicts() {
        let b = bead(
            r#"{"id":"a","title":"t","status":"closed","issue_type":"task","created_at":"2026-10-19T00:59:00Z","closed_at":"2026-10-19T02:00:00Z"}"#,
        );
        let events = vec![
            event("a.5", "review", "2026-10-19T01:30:00Z", "ready for review"),
            event("a.1", "running", "2026-10-19T01:00:00Z", "sulla"),
            event("a.2", "review", "2026-10-19T01:10:00Z", "ready for review"),
            event("a.3", "reviewing", "2026-10-19T01:15:00Z", "sulla"),
            event("a.4", "review", "2026-10-19T01:20:00Z", "ai review done"),
            event("a.6", "running", "2026-10-19T01:20:00Z", "sulla"),
            event("x.1", "running", "2026-10-19T01:00:00Z", "sulla"),
        ];
        let comments = vec![
            Comment {
                text: r#"{"verdict":"changes","comments":[{"file":"a","line":1,"comment":"x"}]}"#
                    .into(),
                created_at: Some(at("2026-10-19T01:19:00Z")),
            },
            Comment {
                text: "lgtm".into(),
                created_at: Some(at("2026-10-19T01:19:30Z")),
            },
        ];
        let t = build(&b, &events, &comments, at("2026-10-20T00:00:00Z"));
        let states: Vec<&str> = t.steps.iter().map(|s| s.state.as_str()).collect();
        assert_eq!(
            states,
            [
                "pending",
                "running",
                "review",
                "reviewing",
                "review",
                "running",
                "review"
            ]
        );
        assert_eq!(t.steps[1].reason, "");
        assert_eq!(t.steps[2].reason, "ready for review");
        assert_eq!(t.steps[5].dur, Duration::minutes(10));
        assert_eq!(t.steps[6].dur, Duration::minutes(30));
        assert_eq!(t.rounds, 2);
        let total = |s: &str| t.totals.iter().find(|(k, _)| k == s).unwrap().1;
        assert_eq!(total("pending"), Duration::minutes(1));
        assert_eq!(total("running"), Duration::minutes(20));
        assert_eq!(total("review"), Duration::minutes(35));
        assert_eq!(total("reviewing"), Duration::minutes(5));
        assert_eq!(t.verdicts.len(), 1);
        assert_eq!(t.verdicts[0].verdict, Verdict::Changes);
        assert_eq!(t.verdicts[0].comments, 1);
        assert_eq!(t.closed, Some(at("2026-10-19T02:00:00Z")));

        let text = render(&b, &t);
        assert!(text.contains("REVIEW ROUNDS  2"));
        assert!(text.contains("AI REVIEWS     1 (0 approve, 1 changes)"));
        assert!(text.contains("changes, 1 comment(s)"));
    }

    #[test]
    fn open_bead_runs_until_now() {
        let b = bead(
            r#"{"id":"a","title":"t","status":"in_progress","created_at":"2026-10-19T01:00:00Z"}"#,
        );
        let events = vec![event("a.1", "running", "2026-10-19T01:00:00Z", "sulla")];
        let t = build(&b, &events, &[], at("2026-10-19T03:30:00Z"));
        assert_eq!(t.steps.len(), 1);
        assert_eq!(t.steps[0].dur, Duration::minutes(150));
        assert_eq!(t.rounds, 0);
        assert_eq!(t.closed, None);
    }

    #[test]
    fn formats_spans() {
        assert_eq!(fmt_span(Duration::seconds(42)), "42s");
        assert_eq!(fmt_span(Duration::seconds(185)), "3m05s");
        assert_eq!(fmt_span(Duration::minutes(125)), "2h05m");
        assert_eq!(fmt_span(Duration::hours(50)), "2d02h");
        assert_eq!(fmt_span(Duration::seconds(-5)), "0s");
    }
}
