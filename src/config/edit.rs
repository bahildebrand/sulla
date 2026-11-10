use std::path::{Path, PathBuf};

use color_eyre::{
    Result,
    eyre::{WrapErr, eyre},
};
use serde::Deserialize;
use serde::de::{self, Visitor};
use toml_edit::{DocumentMut, Item, Table, TableLike};

use super::effective::{Layer, Layers, Source};
use super::reload;
use super::show::dotted;
use super::{AgentKind, PrTool, SandboxKind, Workflow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Str,
    Int,
    Bool,
    Choice(&'static [&'static str]),
    Guess,
}

impl Kind {
    pub fn of(path: &[String], value: Option<&toml::Value>) -> Option<Kind> {
        if let Some(c) = choices(path) {
            return Some(Kind::Choice(c));
        }
        match value {
            None => Some(Kind::Guess),
            Some(toml::Value::String(s)) if s.trim_end_matches('\n').contains('\n') => None,
            Some(toml::Value::String(_)) => Some(Kind::Str),
            Some(toml::Value::Integer(_)) => Some(Kind::Int),
            Some(toml::Value::Boolean(_)) => Some(Kind::Bool),
            Some(toml::Value::Float(_)) => Some(Kind::Guess),
            Some(_) => None,
        }
    }

    pub fn options(self) -> &'static [&'static str] {
        match self {
            Kind::Bool => &["true", "false"],
            Kind::Choice(c) => c,
            _ => &[],
        }
    }

    pub fn cycle(self, current: &str, forward: bool) -> Option<&'static str> {
        let opts = self.options();
        if opts.is_empty() {
            return None;
        }
        let n = opts.len();
        let next = match opts.iter().position(|o| *o == current.trim()) {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None if forward => 0,
            None => n - 1,
        };
        Some(opts[next])
    }

    fn candidates(self, input: &str) -> Result<Vec<toml::Value>> {
        let t = input.trim();
        Ok(match self {
            Kind::Str => vec![toml::Value::String(input.to_string())],
            Kind::Int => vec![toml::Value::Integer(
                t.parse()
                    .map_err(|_| eyre!("{t:?} is not a whole number"))?,
            )],
            Kind::Bool => vec![toml::Value::Boolean(
                t.parse().map_err(|_| eyre!("expected true or false"))?,
            )],
            Kind::Choice(c) => {
                if !c.contains(&t) {
                    return Err(eyre!("expected one of {}", c.join(", ")));
                }
                vec![toml::Value::String(t.to_string())]
            }
            Kind::Guess => {
                let mut out = Vec::new();
                if let Ok(i) = t.parse::<i64>() {
                    out.push(toml::Value::Integer(i));
                } else if let Ok(f) = t.parse::<f64>() {
                    out.push(toml::Value::Float(f));
                }
                if let Ok(b) = t.parse::<bool>() {
                    out.push(toml::Value::Boolean(b));
                }
                out.push(toml::Value::String(input.to_string()));
                out
            }
        })
    }
}

fn choices(path: &[String]) -> Option<&'static [&'static str]> {
    match dotted(path).as_str() {
        "agent" => variants::<AgentKind>(),
        "sandbox" => variants::<SandboxKind>(),
        "workflow" => variants::<Workflow>(),
        "pr.tool" => variants::<PrTool>(),
        _ => None,
    }
}

struct Probe<'a>(&'a mut Option<&'static [&'static str]>);

impl<'de> de::Deserializer<'de> for Probe<'_> {
    type Error = de::value::Error;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("not an enum"))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        variants: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        *self.0 = Some(variants);
        Err(de::Error::custom("probed"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

pub fn variants<'de, T: Deserialize<'de>>() -> Option<&'static [&'static str]> {
    let mut out = None;
    let _ = T::deserialize(Probe(&mut out));
    out
}

pub fn apply(text: &str, key: &[String], value: Option<&toml::Value>) -> Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    let (last, parents) = key.split_last().ok_or_else(|| eyre!("empty key"))?;
    match value {
        Some(v) => {
            let (table, inline) = descend(doc.as_table_mut(), parents, false)?;
            let mut new: toml_edit::Value = v.to_string().parse()?;
            match table.get_mut(last) {
                Some(Item::Value(old)) => {
                    *new.decor_mut() = old.decor().clone();
                    *old = new;
                }
                Some(Item::None) | None => {
                    new.decor_mut().clear();
                    table.insert(last, Item::Value(new));
                    if inline {
                        table.fmt();
                    }
                }
                Some(_) => return Err(eyre!("{} is a table", dotted(key))),
            }
        }
        None => {
            if let Some(table) = find(doc.as_table_mut(), parents) {
                remove_keeping_comments(table, last);
            }
        }
    }
    Ok(doc.to_string())
}

fn remove_keeping_comments(table: &mut dyn TableLike, key: &str) {
    let prefix = table
        .key(key)
        .and_then(|k| k.leaf_decor().prefix())
        .and_then(|p| p.as_str())
        .filter(|p| p.contains('#'))
        .map(String::from);
    let next = table
        .iter()
        .map(|(k, _)| k.to_string())
        .skip_while(|k| k != key)
        .nth(1);
    table.remove(key);
    if let (Some(prefix), Some(next)) = (prefix, next)
        && let Some(mut k) = table.key_mut(&next)
    {
        let rest = k
            .leaf_decor()
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_string();
        k.leaf_decor_mut().set_prefix(format!("{prefix}{rest}"));
    }
}

fn descend<'a>(
    table: &'a mut dyn TableLike,
    path: &[String],
    inline: bool,
) -> Result<(&'a mut dyn TableLike, bool)> {
    let Some((first, rest)) = path.split_first() else {
        return Ok((table, inline));
    };
    if table.get(first).is_none_or(Item::is_none) {
        let mut t = Table::new();
        t.set_implicit(true);
        table.insert(first, Item::Table(t));
    }
    let item = table
        .get_mut(first)
        .ok_or_else(|| eyre!("{first} is missing"))?;
    let inline = inline || item.is_inline_table();
    let next = item
        .as_table_like_mut()
        .ok_or_else(|| eyre!("{first} is not a table"))?;
    descend(next, rest, inline)
}

fn find<'a>(table: &'a mut dyn TableLike, path: &[String]) -> Option<&'a mut dyn TableLike> {
    match path.split_first() {
        None => Some(table),
        Some((first, rest)) => find(table.get_mut(first)?.as_table_like_mut()?, rest),
    }
}

fn layer(layers: &Layers, source: Source) -> Result<&Layer> {
    layers.layer(source).ok_or_else(|| match source {
        Source::Global => eyre!("no global config directory"),
        s => eyre!("can't write to the {} layer", s.name()),
    })
}

pub fn path(layers: &Layers, source: Source) -> Result<PathBuf> {
    Ok(layer(layers, source)?.path.clone())
}

fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).wrap_err_with(|| path.display().to_string()),
    }
}

fn check(layers: &Layers, source: Source, text: &str) -> Result<Layers> {
    let mut next = layers.clone();
    let slot = next
        .layer_mut(source)
        .ok_or_else(|| eyre!("no {} layer", source.name()))?;
    *slot = Layer::parse(&slot.path, text)?;
    next.config()?;
    Ok(next)
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).wrap_err_with(|| dir.display().to_string())?;
    }
    std::fs::write(path, text).wrap_err_with(|| path.display().to_string())
}

fn edit(path: &Path, text: &str, key: &[String], value: Option<&toml::Value>) -> Result<String> {
    apply(text, key, value).wrap_err_with(|| format!("can't edit {}", path.display()))
}

pub fn set(
    layers: &Layers,
    source: Source,
    key: &[String],
    kind: Kind,
    input: &str,
) -> Result<toml::Value> {
    let file = path(layers, source)?;
    let text = read(&file)?;
    let mut first = None;
    for value in kind.candidates(input)? {
        let new = edit(&file, &text, key, Some(&value))?;
        match check(layers, source, &new) {
            Ok(_) => {
                write(&file, &new)?;
                return Ok(value);
            }
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    Err(first.unwrap_or_else(|| eyre!("nothing to write")))
}

pub fn remove(layers: &Layers, source: Source, key: &[String]) -> Result<()> {
    let file = path(layers, source)?;
    let text = read(&file)?;
    let new = edit(&file, &text, key, None)?;
    check(layers, source, &new)?;
    write(&file, &new)
}

pub fn fallback(layers: &Layers, source: Source, key: &[String]) -> Result<Option<toml::Value>> {
    let mut next = layers.clone();
    if let Some(t) = next.layer_mut(source).and_then(|l| l.table.as_mut()) {
        unset(t, key);
    }
    Ok(reload::leaves(&next.config()?)?.remove(key))
}

fn unset(table: &mut toml::Table, key: &[String]) {
    match key {
        [] => {}
        [last] => {
            table.remove(last);
        }
        [first, rest @ ..] => {
            if let Some(toml::Value::Table(t)) = table.get_mut(first) {
                unset(t, rest);
            }
        }
    }
}

pub fn ensure(path: &Path) -> Result<()> {
    if !path.exists() {
        write(path, "")?;
    }
    Ok(())
}

pub fn message(e: &color_eyre::Report) -> String {
    match e.chain().find_map(|c| c.downcast_ref::<toml::de::Error>()) {
        Some(t) => t.message().trim().to_string(),
        None => format!("{e:#}")
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Vec<String> {
        s.split('.').map(String::from).collect()
    }

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("sulla-edit-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }

        fn global(&self) -> PathBuf {
            self.0.join("global").join("config.toml")
        }

        fn repo(&self) -> PathBuf {
            self.0.join("repo").join(".sulla").join("config.toml")
        }

        fn layers(&self) -> Layers {
            Layers::load_from(Some(&self.global()), &self.repo(), toml::Table::new()).unwrap()
        }

        fn read(&self, p: &Path) -> String {
            std::fs::read_to_string(p).unwrap()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const COMMENTED: &str = "# sulla settings\nmax_agents = 3 # keep it small\n\n# which agent\nagent = \"copilot\"\nkache  =  true\n\n[claude]\n# the model\nmodel = \"opus\"\nbin = \"claude\"\n";

    #[test]
    fn variants_come_from_serde() {
        assert_eq!(variants::<AgentKind>(), Some(&["copilot", "claude"][..]));
        assert_eq!(variants::<SandboxKind>(), Some(&["worktree", "incus"][..]));
        assert_eq!(variants::<Workflow>(), Some(&["local", "pr"][..]));
        assert_eq!(variants::<PrTool>(), Some(&["gh", "graphite"][..]));
        assert_eq!(variants::<String>(), None);
    }

    #[test]
    fn kinds_follow_the_value_and_enum_fields() {
        let s = |v: &str| toml::Value::String(v.into());
        assert_eq!(
            Kind::of(&key("agent"), Some(&s("copilot"))),
            Some(Kind::Choice(&["copilot", "claude"]))
        );
        assert_eq!(Kind::of(&key("claude.bin"), Some(&s("x"))), Some(Kind::Str));
        assert_eq!(Kind::of(&key("system_prompt"), Some(&s("a\nb"))), None);
        assert_eq!(Kind::of(&key("check"), Some(&s("a\n"))), Some(Kind::Str));
        assert_eq!(
            Kind::of(&key("max_agents"), Some(&toml::Value::Integer(3))),
            Some(Kind::Int)
        );
        assert_eq!(
            Kind::of(&key("kache"), Some(&toml::Value::Boolean(true))),
            Some(Kind::Bool)
        );
        assert_eq!(Kind::of(&key("incus.cpus"), None), Some(Kind::Guess));
        assert_eq!(
            Kind::of(&key("incus.mounts"), Some(&toml::Value::Array(vec![]))),
            None
        );
        let k = Kind::Choice(&["a", "b", "c"]);
        assert_eq!(k.cycle("a", true), Some("b"));
        assert_eq!(k.cycle("c", true), Some("a"));
        assert_eq!(k.cycle("a", false), Some("c"));
        assert_eq!(k.cycle("zzz", true), Some("a"));
        assert_eq!(Kind::Bool.cycle("true", true), Some("false"));
        assert_eq!(Kind::Str.cycle("x", true), None);
    }

    #[test]
    fn apply_rewrites_only_the_key_and_keeps_comments() {
        let out = apply(
            COMMENTED,
            &key("max_agents"),
            Some(&toml::Value::Integer(5)),
        )
        .unwrap();
        assert_eq!(out, COMMENTED.replace("max_agents = 3", "max_agents = 5"));
        let out = apply(COMMENTED, &key("agent"), Some(&"claude".into())).unwrap();
        assert_eq!(out, COMMENTED.replace("\"copilot\"", "\"claude\""));
        let out = apply(COMMENTED, &key("kache"), Some(&toml::Value::Boolean(false))).unwrap();
        assert_eq!(out, COMMENTED.replace("true", "false"));
        let out = apply(COMMENTED, &key("claude.model"), Some(&"son\"net".into())).unwrap();
        assert_eq!(
            out,
            COMMENTED.replace("model = \"opus\"", "model = 'son\"net'")
        );
        let out = apply(COMMENTED, &key("claude.model"), None).unwrap();
        assert_eq!(out, COMMENTED.replace("model = \"opus\"\n", ""));
    }

    #[test]
    fn apply_creates_nested_tables_and_new_keys() {
        let out = apply(
            COMMENTED,
            &key("incus.browse.url"),
            Some(&"http://x".into()),
        )
        .unwrap();
        assert!(out.starts_with(COMMENTED), "{out}");
        assert!(
            out.ends_with("\n[incus.browse]\nurl = \"http://x\"\n"),
            "{out}"
        );
        assert!(!out.contains("[incus]\n"), "{out}");
        let cfg: super::super::Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.incus.browse.url.as_deref(), Some("http://x"));
        let out = apply(COMMENTED, &key("poll_secs"), Some(&toml::Value::Integer(4))).unwrap();
        assert!(out.contains("kache  =  true\npoll_secs = 4\n"), "{out}");
        let out = apply(COMMENTED, &key("claude.extra"), Some(&"y".into())).unwrap();
        assert!(out.contains("bin = \"claude\"\nextra = \"y\"\n"), "{out}");
        let out = apply("", &key("pr.draft"), Some(&toml::Value::Boolean(true))).unwrap();
        assert_eq!(out, "[pr]\ndraft = true\n");
        let inline = "[incus]\nbrowse = { ports = [1] }\n";
        let out = apply(inline, &key("incus.browse.url"), Some(&"u".into())).unwrap();
        assert_eq!(out, "[incus]\nbrowse = { ports = [1], url = \"u\" }\n");
        assert!(apply("max_agents = 3\n", &key("max_agents.x"), Some(&"u".into())).is_err());
        assert!(apply("[claude\n", &key("max_agents"), Some(&"u".into())).is_err());
    }

    #[test]
    fn set_writes_the_right_file_and_creates_it() {
        let d = Dir::new("set");
        let layers = d.layers();
        let v = set(
            &layers,
            Source::Repo,
            &key("incus.browse.url"),
            Kind::Guess,
            "http://a",
        )
        .unwrap();
        assert_eq!(v, toml::Value::String("http://a".into()));
        assert_eq!(d.read(&d.repo()), "[incus.browse]\nurl = \"http://a\"\n");
        assert!(!d.global().exists());
        let layers = d.layers();
        set(
            &layers,
            Source::Global,
            &key("max_agents"),
            Kind::Int,
            " 6 ",
        )
        .unwrap();
        assert_eq!(d.read(&d.global()), "max_agents = 6\n");
        assert_eq!(d.read(&d.repo()), "[incus.browse]\nurl = \"http://a\"\n");
        let cfg = d.layers().config().unwrap();
        assert_eq!(cfg.max_agents, 6);
        assert_eq!(cfg.incus.browse.url.as_deref(), Some("http://a"));
    }

    #[test]
    fn guesses_pick_the_first_type_the_config_accepts() {
        let d = Dir::new("guess");
        let layers = d.layers();
        assert_eq!(
            set(&layers, Source::Repo, &key("incus.cpus"), Kind::Guess, "4").unwrap(),
            toml::Value::Integer(4)
        );
        assert_eq!(
            set(
                &layers,
                Source::Repo,
                &key("claude.model"),
                Kind::Guess,
                "4"
            )
            .unwrap(),
            toml::Value::String("4".into())
        );
        assert_eq!(
            d.read(&d.repo()),
            "[incus]\ncpus = 4\n\n[claude]\nmodel = \"4\"\n"
        );
    }

    #[test]
    fn invalid_values_leave_the_file_alone() {
        let d = Dir::new("invalid");
        std::fs::create_dir_all(d.repo().parent().unwrap()).unwrap();
        std::fs::write(d.repo(), COMMENTED).unwrap();
        let layers = d.layers();
        let e = set(&layers, Source::Repo, &key("max_agents"), Kind::Int, "lots").unwrap_err();
        assert_eq!(message(&e), "\"lots\" is not a whole number");
        let e = set(&layers, Source::Repo, &key("max_agents"), Kind::Int, "-1").unwrap_err();
        assert!(message(&e).contains("invalid value"), "{}", message(&e));
        let e = set(
            &layers,
            Source::Repo,
            &key("agent"),
            Kind::Choice(&["copilot", "claude"]),
            "gpt",
        )
        .unwrap_err();
        assert_eq!(message(&e), "expected one of copilot, claude");
        let e = set(
            &layers,
            Source::Repo,
            &key("incus.cpus"),
            Kind::Guess,
            "four",
        )
        .unwrap_err();
        assert!(message(&e).contains("expected u32"), "{}", message(&e));
        assert_eq!(d.read(&d.repo()), COMMENTED);
        std::fs::write(d.repo(), "[claude\n").unwrap();
        let e = set(&layers, Source::Repo, &key("max_agents"), Kind::Int, "2").unwrap_err();
        assert!(format!("{e:#}").contains("can't edit"), "{e:#}");
        assert_eq!(d.read(&d.repo()), "[claude\n");
        assert!(!d.global().exists());
    }

    #[test]
    fn set_round_trips_each_kind_through_a_commented_file() {
        let d = Dir::new("roundtrip");
        std::fs::create_dir_all(d.repo().parent().unwrap()).unwrap();
        std::fs::write(d.repo(), COMMENTED).unwrap();
        let layers = d.layers();
        let agent = Kind::of(&key("agent"), None).unwrap();
        set(&layers, Source::Repo, &key("max_agents"), Kind::Int, "8").unwrap();
        set(
            &layers,
            Source::Repo,
            &key("claude.model"),
            Kind::Str,
            "sonnet",
        )
        .unwrap();
        set(&layers, Source::Repo, &key("kache"), Kind::Bool, "false").unwrap();
        set(&layers, Source::Repo, &key("agent"), agent, "claude").unwrap();
        let want = COMMENTED
            .replace("max_agents = 3", "max_agents = 8")
            .replace("\"opus\"", "\"sonnet\"")
            .replace("true", "false")
            .replace("\"copilot\"", "\"claude\"");
        assert_eq!(d.read(&d.repo()), want);
        let cfg = d.layers().config().unwrap();
        assert_eq!(cfg.max_agents, 8);
        assert_eq!(cfg.agent, AgentKind::Claude);
        assert!(!cfg.kache);
    }

    #[test]
    fn remove_falls_back_to_the_lower_layer() {
        let d = Dir::new("remove");
        std::fs::create_dir_all(d.repo().parent().unwrap()).unwrap();
        std::fs::create_dir_all(d.global().parent().unwrap()).unwrap();
        std::fs::write(d.global(), "max_agents = 7\n").unwrap();
        std::fs::write(d.repo(), "# mine\nmax_agents = 2\npoll_secs = 9\n").unwrap();
        let layers = d.layers();
        assert_eq!(
            fallback(&layers, Source::Repo, &key("max_agents")).unwrap(),
            Some(toml::Value::Integer(7))
        );
        assert_eq!(
            fallback(&layers, Source::Repo, &key("poll_secs")).unwrap(),
            Some(toml::Value::Integer(2))
        );
        assert_eq!(
            fallback(&layers, Source::Repo, &key("check")).unwrap(),
            None
        );
        remove(&layers, Source::Repo, &key("max_agents")).unwrap();
        assert_eq!(d.read(&d.repo()), "# mine\npoll_secs = 9\n");
        assert_eq!(d.layers().config().unwrap().max_agents, 7);
        remove(&d.layers(), Source::Global, &key("max_agents")).unwrap();
        assert_eq!(d.layers().config().unwrap().max_agents, 3);
    }

    #[test]
    fn global_layer_is_required_for_global_writes() {
        let d = Dir::new("noglobal");
        let layers = Layers::load_from(None, &d.repo(), toml::Table::new()).unwrap();
        let e = set(&layers, Source::Global, &key("max_agents"), Kind::Int, "2").unwrap_err();
        assert_eq!(message(&e), "no global config directory");
        assert!(set(&layers, Source::Flag, &key("max_agents"), Kind::Int, "2").is_err());
    }

    #[test]
    fn ensure_creates_an_empty_file_and_its_directory() {
        let d = Dir::new("ensure");
        ensure(&d.repo()).unwrap();
        assert_eq!(d.read(&d.repo()), "");
        std::fs::write(d.repo(), "max_agents = 1\n").unwrap();
        ensure(&d.repo()).unwrap();
        assert_eq!(d.read(&d.repo()), "max_agents = 1\n");
    }
}
