use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::Path;

use color_eyre::Result;
use ratatui::style::Color;

use super::effective::{Effective, Entry, Layers, Source};
use crate::ui::theme;

const TAG_COLUMN_MAX: usize = 60;
const ACCENTS: [Color; 6] = [
    theme::BLUE,
    theme::MAUVE,
    theme::TEAL,
    theme::PINK,
    theme::LAVENDER,
    theme::SKY,
];

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub raw: bool,
    pub no_defaults: bool,
    pub color: bool,
}

pub fn source_color(s: Source) -> Color {
    match s {
        Source::Default => theme::MUTED,
        Source::Global => theme::BLUE,
        Source::Repo => theme::MAUVE,
        Source::Flag => theme::PEACH,
        Source::Runtime => theme::YELLOW,
    }
}

pub fn value_color(v: &toml::Value) -> Color {
    match v {
        toml::Value::String(_) => theme::GREEN,
        toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Datetime(_) => theme::PEACH,
        toml::Value::Boolean(_) => theme::MAUVE,
        toml::Value::Array(_) | toml::Value::Table(_) => theme::SKY,
    }
}

pub fn color_enabled(stream: &impl IsTerminal) -> bool {
    stream.is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub fn run(repo: &Path, flags: toml::Table, raw: bool, no_defaults: bool) -> Result<()> {
    let opts = Options {
        raw,
        no_defaults,
        color: !raw && color_enabled(&std::io::stdout()),
    };
    let out = Layers::load(repo, flags).and_then(|l| Ok(render(&l, &l.effective()?, opts)));
    let out = match out {
        Ok(out) => out,
        Err(e) => {
            let paint = Paint(color_enabled(&std::io::stderr()));
            eprintln!("{} {e:#}", paint.style(theme::RED, true, "error:"));
            std::process::exit(1);
        }
    };
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(out.as_bytes())
        .and_then(|_| stdout.flush())
    {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => Ok(r?),
    }
}

pub fn render(layers: &Layers, eff: &Effective, opts: Options) -> String {
    let mut rows = Vec::new();
    if !opts.raw {
        header(layers, &mut rows);
    }
    body(eff, opts, &mut rows);
    if !opts.raw && !eff.unknown.is_empty() {
        unknown(eff, &mut rows);
    }
    finish(&rows, &Paint(opts.color && !opts.raw))
}

struct Span {
    text: String,
    color: Option<Color>,
    bold: bool,
}

fn span(text: impl Into<String>, color: Color) -> Span {
    Span {
        text: text.into(),
        color: Some(color),
        bold: false,
    }
}

fn bold(text: impl Into<String>, color: Color) -> Span {
    Span {
        bold: true,
        ..span(text, color)
    }
}

#[derive(Default)]
struct Row {
    body: Vec<Span>,
    tag: Vec<Span>,
}

impl Row {
    fn new(body: Vec<Span>) -> Self {
        Self {
            body,
            tag: Vec::new(),
        }
    }

    fn width(&self) -> usize {
        self.body.iter().map(|s| s.text.chars().count()).sum()
    }
}

struct Paint(bool);

impl Paint {
    fn style(&self, color: Color, bold: bool, s: &str) -> String {
        match (self.0, color) {
            (true, Color::Rgb(r, g, b)) => {
                let weight = if bold { "1;" } else { "" };
                format!("\x1b[{weight}38;2;{r};{g};{b}m{s}\x1b[0m")
            }
            _ => s.to_string(),
        }
    }

    fn spans(&self, spans: &[Span]) -> String {
        spans
            .iter()
            .map(|s| match s.color {
                Some(c) => self.style(c, s.bold, &s.text),
                None => s.text.clone(),
            })
            .collect()
    }
}

fn finish(rows: &[Row], paint: &Paint) -> String {
    let col = rows
        .iter()
        .filter(|r| !r.tag.is_empty())
        .map(Row::width)
        .filter(|&w| w <= TAG_COLUMN_MAX)
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for row in rows {
        out.push_str(&paint.spans(&row.body));
        if !row.tag.is_empty() {
            let pad = col.saturating_sub(row.width()) + 2;
            out.push_str(&" ".repeat(pad));
            out.push_str(&paint.spans(&row.tag));
        }
        out.push('\n');
    }
    out
}

fn blank(rows: &mut Vec<Row>) {
    if rows.last().is_some_and(|r| !r.body.is_empty()) {
        rows.push(Row::default());
    }
}

fn header(layers: &Layers, rows: &mut Vec<Row>) {
    let files: Vec<(Source, String, Option<bool>)> = match &layers.global {
        Some(_) => Vec::new(),
        None => vec![(Source::Global, "no config directory".into(), None)],
    }
    .into_iter()
    .chain(
        layers
            .files()
            .map(|(s, l)| (s, display_path(&l.path), Some(l.exists()))),
    )
    .collect();
    let width = files.iter().map(|f| f.1.chars().count()).max().unwrap_or(0);
    for (source, path, exists) in files {
        let mut body = vec![
            span("# ", theme::MUTED),
            span(format!("{:<8}", source.name()), source_color(source)),
            span(format!("{path:<width$}"), theme::TEXT),
        ];
        match exists {
            Some(true) => body.push(span("  exists", theme::GREEN)),
            Some(false) => body.push(span("  missing", theme::OVERLAY)),
            None => {}
        }
        rows.push(Row::new(body));
    }
    for (source, table) in [
        (Source::Flag, &layers.flags),
        (Source::Runtime, &layers.runtime),
    ] {
        for (k, v) in table {
            rows.push(Row::new(vec![
                span("# ", theme::MUTED),
                span(format!("{:<8}", source.name()), source_color(source)),
                span(key(k), theme::TEXT),
                span(" = ", theme::OVERLAY),
                span(v.to_string(), value_color(v)),
            ]));
        }
    }
    blank(rows);
}

fn body(eff: &Effective, opts: Options, rows: &mut Vec<Row>) {
    let mut sections: BTreeMap<&[String], Vec<&Entry>> = BTreeMap::new();
    for e in &eff.entries {
        if opts.no_defaults && e.source == Source::Default {
            continue;
        }
        sections
            .entry(&e.path[..e.path.len() - 1])
            .or_default()
            .push(e);
    }
    let mut accents: Vec<&str> = Vec::new();
    for (table, entries) in sections {
        if let Some(top) = table.first() {
            if !accents.contains(&top.as_str()) {
                accents.push(top);
            }
            let accent = ACCENTS[(accents.len() - 1) % ACCENTS.len()];
            blank(rows);
            rows.push(Row::new(vec![bold(format!("[{}]", dotted(table)), accent)]));
        }
        for e in entries {
            let tag = (!opts.raw).then(|| {
                vec![
                    span("# ", theme::MUTED),
                    span(e.source.name(), source_color(e.source)),
                ]
            });
            value_rows(
                vec![
                    span(key(&e.path[e.path.len() - 1]), theme::TEXT),
                    span(" = ", theme::OVERLAY),
                ],
                "",
                &e.value,
                value_color(&e.value),
                tag.unwrap_or_default(),
                rows,
            );
        }
    }
}

fn unknown(eff: &Effective, rows: &mut Vec<Row>) {
    blank(rows);
    rows.push(Row::new(vec![bold("# unknown keys", theme::RED)]));
    for u in &eff.unknown {
        value_rows(
            vec![span(format!("# {} = ", dotted(&u.path)), theme::RED)],
            "# ",
            &u.value,
            theme::RED,
            vec![
                span("# ", theme::MUTED),
                span(u.source.name(), source_color(u.source)),
                span(format!(" {}", display_path(&u.file)), theme::OVERLAY),
            ],
            rows,
        );
    }
}

fn value_rows(
    lead: Vec<Span>,
    prefix: &str,
    value: &toml::Value,
    color: Color,
    tag: Vec<Span>,
    rows: &mut Vec<Row>,
) {
    let text = value.to_string();
    let mut lines = text.split('\n');
    let mut body = lead;
    body.push(span(lines.next().unwrap_or_default(), color));
    rows.push(Row::new(body));
    for line in lines {
        let mut body = Vec::new();
        if !prefix.is_empty() {
            body.push(span(prefix, color));
        }
        body.push(span(line, color));
        rows.push(Row::new(body));
    }
    if let Some(last) = rows.last_mut() {
        last.tag = tag;
    }
}

fn dotted(path: &[String]) -> String {
    path.iter().map(|k| key(k)).collect::<Vec<_>>().join(".")
}

fn key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        return k.to_string();
    }
    let mut out = String::from("\"");
    for c in k.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn display_path(p: &Path) -> String {
    directories::BaseDirs::new()
        .and_then(|b| {
            p.strip_prefix(b.home_dir())
                .ok()
                .map(|r| format!("~/{}", r.display()))
        })
        .unwrap_or_else(|| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn layers(tag: &str, global: &str, repo: &str, flags: &str) -> (Layers, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("sulla-show-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let g = d.join("global.toml");
        let r = d.join("repo.toml");
        std::fs::write(&g, global).unwrap();
        std::fs::write(&r, repo).unwrap();
        let l = Layers::load_from(Some(&g), &r, toml::from_str(flags).unwrap()).unwrap();
        (l, d)
    }

    fn serialized(c: &Config) -> toml::Table {
        toml::Table::try_from(c).unwrap()
    }

    const GLOBAL: &str = r#"
max_agents = 7
system_prompt = """
Use "pnpm", not npm.
Paths look like C:\\tmp and ''' quotes ''' and \"\"\" too, ending in "quote\"
"""
[claude]
model = "opus"
extra_args = ["--x", "it's"]
[presets]
perf = "measure"
"odd key.with dot" = "y"
[incus]
init = """
apt-get update
"""
env = { FOO = "bar\tbaz" }
"#;

    const REPO: &str = r#"
sandbox = "incus"
[incus.browse]
ports = [8000, 8001]
url = "http://{ip}:8000"
[notify]
on = ["review"]
"#;

    #[test]
    fn raw_round_trips_to_the_same_config() {
        let (l, d) = layers("raw", GLOBAL, REPO, "base_branch = \"main\"");
        let eff = l.effective().unwrap();
        for no_defaults in [false, true] {
            let out = render(
                &l,
                &eff,
                Options {
                    raw: true,
                    no_defaults,
                    color: true,
                },
            );
            assert!(!out.contains('\x1b'), "{out}");
            assert!(!out.contains("# default"), "{out}");
            let back: Config = toml::from_str(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
            assert_eq!(serialized(&back), serialized(&l.config().unwrap()), "{out}");
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn pretty_output_tags_sources_and_stays_toml() {
        let (l, d) = layers("pretty", GLOBAL, REPO, "max_agents = 5");
        let eff = l.effective().unwrap();
        let out = render(&l, &eff, Options::default());
        let line = |prefix: &str| {
            out.lines()
                .find(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("no {prefix} in\n{out}"))
                .to_string()
        };
        assert!(line("max_agents = 5").ends_with("# flag"));
        assert!(line("sandbox = \"incus\"").ends_with("# repo"));
        assert!(line("model = \"opus\"").ends_with("# global"));
        assert!(line("poll_secs = 2").ends_with("# default"));
        assert!(line("ports = [8000, 8001]").ends_with("# repo"));
        assert!(line("# flag").contains("max_agents = 5"));
        assert!(line("# repo").contains("exists"));
        let tag_col = |prefix: &str| line(prefix).rfind('#').unwrap();
        assert_eq!(tag_col("max_agents = 5"), tag_col("poll_secs = 2"));
        assert!(out.contains("system_prompt = \"\"\"\n"));
        let tables: Vec<&str> = out.lines().filter(|l| l.starts_with('[')).collect();
        assert_eq!(tables.first(), Some(&"[ai_review]"));
        let incus = tables.iter().position(|t| *t == "[incus]").unwrap();
        assert_eq!(tables[incus + 1], "[incus.browse]");
        assert!(out.find("max_agents = 5").unwrap() < out.find("[ai_review]").unwrap());
        assert!(!out.contains('\x1b'));
        let back: Config = toml::from_str(&out).unwrap();
        assert_eq!(serialized(&back), serialized(&l.config().unwrap()));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn no_defaults_hides_default_keys() {
        let (l, d) = layers(
            "nodefaults",
            "max_agents = 7",
            "[claude]\nmodel = \"m\"",
            "",
        );
        let eff = l.effective().unwrap();
        let out = render(
            &l,
            &eff,
            Options {
                no_defaults: true,
                ..Options::default()
            },
        );
        assert!(out.contains("max_agents = 7"));
        assert!(out.contains("[claude]\nmodel = \"m\""));
        assert!(!out.contains("# default"), "{out}");
        assert!(!out.contains("[incus]"), "{out}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn unknown_keys_print_in_a_trailing_section() {
        let (l, d) = layers("unknown", "max_agent = 4", "[incus]\nbowse = 1", "");
        let eff = l.effective().unwrap();
        let out = render(&l, &eff, Options::default());
        let tail = &out[out.find("# unknown keys").expect(&out)..];
        assert!(tail.contains("# max_agent = 4"), "{out}");
        assert!(tail.contains("# incus.bowse = 1"), "{out}");
        assert!(tail.contains("global.toml"), "{out}");
        assert!(tail.contains("repo.toml"), "{out}");
        let raw = render(
            &l,
            &eff,
            Options {
                raw: true,
                ..Options::default()
            },
        );
        assert!(!raw.contains("unknown"), "{raw}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn colour_uses_theme_rgb() {
        let (l, d) = layers("color", "", "", "");
        let eff = l.effective().unwrap();
        let out = render(
            &l,
            &eff,
            Options {
                color: true,
                ..Options::default()
            },
        );
        let Color::Rgb(r, g, b) = theme::MUTED else {
            unreachable!()
        };
        assert!(out.contains(&format!("\x1b[38;2;{r};{g};{b}mdefault\x1b[0m")));
        assert!(out.contains("\x1b[1;38;2;"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn keys_quote_when_not_bare() {
        assert_eq!(key("max_agents"), "max_agents");
        assert_eq!(key("a.b"), "\"a.b\"");
        assert_eq!(key("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(key(""), "\"\"");
    }
}
