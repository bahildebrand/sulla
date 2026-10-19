use std::collections::BTreeMap;

use crate::beads::Bead;

pub const LABEL: &str = "preset";

const BUILTIN: &[(&str, &str)] = &[
    (
        "bugfix",
        "This is a bug fix. First reproduce the bug, ideally with a failing test. Find the root \
         cause before changing code, then make the smallest fix that addresses it. Keep the \
         regression test and do not refactor unrelated code.",
    ),
    (
        "feature",
        "This is a new feature. Follow the existing architecture and conventions, keep the \
         change focused on the requested behavior, add focused tests for it, and update any \
         directly related docs or help text.",
    ),
    (
        "refactor",
        "This is a refactor. Preserve existing behavior exactly. Make sure the relevant tests \
         pass before and after, prefer small mechanical steps, and do not add features or fix \
         unrelated issues along the way.",
    ),
    (
        "tests",
        "This is about tests. Add or improve tests for the described behavior without changing \
         production code unless a test exposes a real bug. Cover edge cases and keep tests \
         readable and deterministic.",
    ),
    (
        "docs",
        "This is a documentation change. Keep it accurate to the current code, concise, and \
         consistent with existing docs. Do not change code behavior.",
    ),
];

pub fn names(custom: &BTreeMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = BUILTIN.iter().map(|(n, _)| n.to_string()).collect();
    out.extend(
        custom
            .keys()
            .filter(|k| !BUILTIN.iter().any(|(n, _)| n == k))
            .cloned(),
    );
    out
}

pub fn text<'a>(custom: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    custom
        .get(name)
        .map(String::as_str)
        .or_else(|| BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, t)| *t))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

pub fn for_bead<'a>(
    custom: &'a BTreeMap<String, String>,
    bead: &'a Bead,
) -> Option<(&'a str, &'a str)> {
    let name = bead.label(LABEL)?;
    match text(custom, name) {
        Some(t) => Some((name, t)),
        None => {
            tracing::warn!("{}: unknown preset {name}", bead.id);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_presets_override_and_extend_builtins() {
        let mut custom = BTreeMap::new();
        custom.insert("bugfix".to_string(), "mine".to_string());
        custom.insert("perf".to_string(), "go fast".to_string());
        let n = names(&custom);
        assert_eq!(n.iter().filter(|x| *x == "bugfix").count(), 1);
        assert_eq!(n.last().map(String::as_str), Some("perf"));
        assert_eq!(text(&custom, "bugfix"), Some("mine"));
        assert_eq!(text(&custom, "perf"), Some("go fast"));
        assert!(text(&custom, "refactor").is_some());
        assert_eq!(text(&custom, "nope"), None);
    }

    #[test]
    fn bead_preset_comes_from_label() {
        let b: Bead = serde_json::from_str(
            r#"{"id":"a","title":"t","status":"open","labels":["x","preset:refactor"]}"#,
        )
        .unwrap();
        let custom = BTreeMap::new();
        assert_eq!(for_bead(&custom, &b).map(|(n, _)| n), Some("refactor"));
        let b: Bead = serde_json::from_str(
            r#"{"id":"a","title":"t","status":"open","labels":["preset:zz"]}"#,
        )
        .unwrap();
        assert!(for_bead(&custom, &b).is_none());
    }
}
