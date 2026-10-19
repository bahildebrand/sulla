use crate::beads::Bead;

#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub query: String,
    terms: Vec<String>,
    labels: Vec<String>,
    priorities: Vec<(u8, u8)>,
}

fn lower(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

fn find(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

fn priorities(spec: &str) -> Option<Vec<(u8, u8)>> {
    let digit = |s: &str| s.trim_start_matches(['p', 'P']).parse::<u8>().ok();
    spec.split(',')
        .map(|part| match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (digit(a)?, digit(b)?);
                Some((a.min(b), a.max(b)))
            }
            None => digit(part).map(|p| (p, p)),
        })
        .collect()
}

impl Filter {
    pub fn parse(query: &str) -> Self {
        let mut f = Filter {
            query: query.to_string(),
            ..Default::default()
        };
        for tok in query.split_whitespace() {
            let (key, val) = tok.split_once(':').unwrap_or(("", tok));
            match key.to_lowercase().as_str() {
                "label" | "l" if !val.is_empty() => f.labels.push(val.to_lowercase()),
                "p" | "prio" | "priority" if !val.is_empty() => match priorities(val) {
                    Some(p) => f.priorities.extend(p),
                    None => f.terms.push(tok.to_lowercase()),
                },
                _ => f.terms.push(tok.to_lowercase()),
            }
        }
        f
    }

    pub fn is_active(&self) -> bool {
        !(self.terms.is_empty() && self.labels.is_empty() && self.priorities.is_empty())
    }

    pub fn matches(&self, b: &Bead) -> bool {
        let title = lower(&b.title);
        let id = lower(&b.id);
        self.terms.iter().all(|t| {
            let t: Vec<char> = t.chars().collect();
            find(&title, &t).is_some() || find(&id, &t).is_some()
        }) && self.labels.iter().all(|l| {
            b.labels
                .iter()
                .any(|bl| bl.to_lowercase().contains(l.as_str()))
        }) && (self.priorities.is_empty()
            || self
                .priorities
                .iter()
                .any(|(lo, hi)| (*lo..=*hi).contains(&b.priority)))
    }

    pub fn highlights(&self, text: &str) -> Vec<bool> {
        let hay = lower(text);
        let mut marks = vec![false; hay.len()];
        for t in &self.terms {
            let t: Vec<char> = t.chars().collect();
            let mut start = 0;
            while let Some(i) = find(&hay[start..], &t) {
                marks[start + i..start + i + t.len()].fill(true);
                start += i + t.len();
            }
        }
        marks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead(id: &str, title: &str, priority: u8, labels: &[&str]) -> Bead {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": title,
            "status": "open",
            "priority": priority,
            "labels": labels,
        }))
        .unwrap()
    }

    #[test]
    fn matches_terms_labels_and_priorities() {
        let b = bead("sulla-12", "Board Search UI", 1, &["ui", "sulla:running"]);
        assert!(!Filter::parse("  ").is_active());
        assert!(Filter::parse("search board").matches(&b));
        assert!(Filter::parse("SULLA-12").matches(&b));
        assert!(!Filter::parse("search missing").matches(&b));
        assert!(Filter::parse("label:UI").matches(&b));
        assert!(Filter::parse("l:sulla:run").matches(&b));
        assert!(!Filter::parse("label:backend").matches(&b));
        assert!(Filter::parse("p:1").matches(&b));
        assert!(Filter::parse("p:0-2").matches(&b));
        assert!(Filter::parse("p:3,P1").matches(&b));
        assert!(!Filter::parse("p:2-4").matches(&b));
        assert!(!Filter::parse("p:2 search").matches(&b));
        assert!(Filter::parse("p:x").is_active());
        assert!(!Filter::parse("p:x").matches(&b));
    }

    #[test]
    fn highlights_every_occurrence_case_insensitively() {
        let marks = Filter::parse("ab").highlights("xAbab");
        assert_eq!(marks, [false, true, true, true, true]);
        assert!(Filter::parse("p:1").highlights("p1").iter().all(|m| !m));
    }
}
