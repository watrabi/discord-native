//! Discord-flavoured markdown → egui `LayoutJob`.
//!
//! The result is cached per message so parsing happens once, not per frame.
//! Links are recorded as char ranges so clicks can be resolved with
//! `Galley::cursor_from_pos` without any extra widgets.

use std::ops::Range;

use eframe::egui::{Color32, FontFamily, FontId, Stroke, text::LayoutJob, text::TextFormat};

use crate::model::Id;

#[derive(Clone)]
pub struct Theme {
    pub size: f32,
    pub text: Color32,
    pub strong: Color32,
    pub weak: Color32,
    pub link: Color32,
    pub code_bg: Color32,
    pub mention_fg: Color32,
    pub mention_bg: Color32,
    pub spoiler: Color32,
}

pub struct Rendered {
    pub job: LayoutJob,
    /// (char range, url)
    pub links: Vec<(Range<usize>, String)>,
}

impl Rendered {
    pub fn link_at(&self, char_idx: usize) -> Option<&str> {
        self.links.iter().find(|(r, _)| r.contains(&char_idx)).map(|(_, u)| u.as_str())
    }
}

pub enum Ref {
    User(Id),
    Channel(Id),
    Role(Id),
}

#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    spoiler: bool,
    quote: bool,
}

struct Builder<'a> {
    job: LayoutJob,
    chars: usize,
    links: Vec<(Range<usize>, String)>,
    theme: &'a Theme,
}

impl Builder<'_> {
    fn fmt(&self, s: Style) -> TextFormat {
        let t = self.theme;
        let mut color = if s.bold { t.strong } else { t.text };
        if s.quote {
            color = color.gamma_multiply(0.85);
        }
        TextFormat {
            font_id: FontId::new(t.size, FontFamily::Proportional),
            color,
            background: if s.spoiler { t.spoiler } else { Color32::TRANSPARENT },
            italics: s.italic,
            underline: if s.underline { Stroke::new(1.0, color) } else { Stroke::NONE },
            strikethrough: if s.strike { Stroke::new(1.0, color) } else { Stroke::NONE },
            ..Default::default()
        }
    }

    fn push(&mut self, text: &str, f: TextFormat) {
        if text.is_empty() {
            return;
        }
        self.chars += text.chars().count();
        self.job.append(text, 0.0, f);
    }

    fn text(&mut self, text: &str, s: Style) {
        let f = self.fmt(s);
        self.push(text, f);
    }

    fn code(&mut self, text: &str) {
        let t = self.theme;
        let f = TextFormat {
            font_id: FontId::new(t.size * 0.95, FontFamily::Monospace),
            color: t.text,
            background: t.code_bg,
            ..Default::default()
        };
        self.push(text, f);
    }

    fn mention(&mut self, text: &str) {
        let t = self.theme;
        let f = TextFormat {
            font_id: FontId::new(t.size, FontFamily::Proportional),
            color: t.mention_fg,
            background: t.mention_bg,
            ..Default::default()
        };
        self.push(text, f);
    }

    fn link(&mut self, text: &str, url: &str, s: Style) {
        let mut f = self.fmt(s);
        f.color = self.theme.link;
        f.underline = Stroke::new(1.0, self.theme.link);
        let start = self.chars;
        self.push(text, f);
        self.links.push((start..self.chars, url.to_owned()));
    }
}

pub fn render(content: &str, theme: &Theme, resolve: &dyn Fn(Ref) -> Option<String>) -> Rendered {
    let mut b = Builder { job: LayoutJob::default(), chars: 0, links: Vec::new(), theme };
    b.job.wrap.max_width = f32::INFINITY;

    // Split out fenced code blocks first; they are not parsed further.
    let mut rest = content;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("```") else { break };
        block(&mut b, &rest[..start], resolve);
        let mut code = &after[..end];
        // Drop language tag on the opening line.
        if let Some(nl) = code.find('\n') {
            let first = &code[..nl];
            if !first.is_empty() && !first.contains(' ') {
                code = &code[nl + 1..];
            }
        }
        let code = code.trim_end_matches('\n');
        if !b.job.text.is_empty() && !b.job.text.ends_with('\n') {
            b.text("\n", Style::default());
        }
        b.code(code);
        rest = &after[end + 3..];
        if !rest.is_empty() && !rest.starts_with('\n') {
            b.text("\n", Style::default());
        }
    }
    block(&mut b, rest, resolve);

    // Trim a trailing newline produced by block splitting.
    Rendered { job: b.job, links: b.links }
}

fn block(b: &mut Builder, text: &str, resolve: &dyn Fn(Ref) -> Option<String>) {
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            b.text("\n", Style::default());
        }
        first = false;
        let mut style = Style::default();
        let mut line = line;
        if let Some(q) = line.strip_prefix("> ").or_else(|| (line == ">").then_some("")) {
            let weak = b.theme.weak;
            let mut f = b.fmt(style);
            f.color = weak;
            f.font_id = FontId::new(b.theme.size, FontFamily::Monospace);
            b.push("│ ", f);
            style.quote = true;
            line = q;
        } else if let Some(h) = line.strip_prefix("# ").or(line.strip_prefix("## ")).or(line.strip_prefix("### ")) {
            style.bold = true;
            line = h;
        } else if let Some(item) = line.strip_prefix("- ").or(line.strip_prefix("* ")) {
            b.text("  • ", style);
            line = item;
        }
        inline(b, line, style, resolve);
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric())
}

fn inline(b: &mut Builder, s: &str, mut style: Style, resolve: &dyn Fn(Ref) -> Option<String>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut start = 0; // start of pending plain text
    macro_rules! flush {
        () => {
            if start < i {
                b.text(&s[start..i], style);
            }
        };
    }
    while i < bytes.len() {
        let rest = &s[i..];
        let prev = s[..i].chars().next_back();

        // Escapes.
        if bytes[i] == b'\\' && i + 1 < bytes.len() && !bytes[i + 1].is_ascii_alphanumeric() {
            flush!();
            let c = rest[1..].chars().next().unwrap();
            i += 1;
            start = i;
            i += c.len_utf8();
            continue;
        }

        // Inline code.
        if bytes[i] == b'`'
            && let Some(end) = rest[1..].find('`')
        {
            flush!();
            b.code(&rest[1..1 + end]);
            i += end + 2;
            start = i;
            continue;
        }

        // Paired markers.
        let mut toggled = false;
        for (marker, field) in [("**", 0u8), ("__", 1), ("~~", 2), ("||", 3)] {
            if rest.starts_with(marker) {
                let active = match field {
                    0 => style.bold,
                    1 => style.underline,
                    2 => style.strike,
                    _ => style.spoiler,
                };
                if active || rest[2..].contains(marker) {
                    flush!();
                    match field {
                        0 => style.bold = !style.bold,
                        1 => style.underline = !style.underline,
                        2 => style.strike = !style.strike,
                        _ => style.spoiler = !style.spoiler,
                    }
                    i += 2;
                    start = i;
                    toggled = true;
                }
                break;
            }
        }
        if toggled {
            continue;
        }
        if rest.starts_with("**") || rest.starts_with("__") {
            // Unpaired double marker: literal text.
            i += 2;
            continue;
        }
        if bytes[i] == b'*' || (bytes[i] == b'_' && (style.italic || !is_word(prev))) {
            let m = bytes[i] as char;
            let next = rest[1..].chars().next();
            let closing_ok = style.italic && !(m == '_' && is_word(next));
            let opening_ok = !style.italic && next.is_some_and(|c| !c.is_whitespace()) && rest[1..].contains(m);
            if closing_ok || opening_ok {
                flush!();
                style.italic = !style.italic;
                i += 1;
                start = i;
                continue;
            }
        }

        // Mentions, channels, roles, custom emoji, timestamps, <url>.
        if bytes[i] == b'<'
            && let Some(end) = rest.find('>')
        {
            let inner = &rest[1..end];
            if let Some((text, kind)) = parse_angle(inner, resolve) {
                flush!();
                match kind {
                    AngleKind::Mention => b.mention(&text),
                    AngleKind::Plain => b.text(&text, style),
                    AngleKind::Url => b.link(&text, &text, style),
                }
                i += end + 1;
                start = i;
                continue;
            }
        }
        if (rest.starts_with("@everyone") || rest.starts_with("@here")) && !is_word(prev) {
            flush!();
            let n = if rest.starts_with("@here") { 5 } else { 9 };
            b.mention(&rest[..n]);
            i += n;
            start = i;
            continue;
        }

        // Bare URLs.
        if (rest.starts_with("https://") || rest.starts_with("http://")) && !is_word(prev) {
            let mut end = rest.find(|c: char| c.is_whitespace() || c == '<').unwrap_or(rest.len());
            // Trailing punctuation is usually not part of the URL.
            while end > 0
                && matches!(rest.as_bytes()[end - 1], b'.' | b',' | b')' | b'!' | b'?' | b':' | b';' | b'"' | b'\'')
            {
                if rest.as_bytes()[end - 1] == b')' && rest[..end].contains('(') {
                    break;
                }
                end -= 1;
            }
            flush!();
            b.link(&rest[..end], &rest[..end], style);
            i += end;
            start = i;
            continue;
        }

        i += rest.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    flush!();
}

enum AngleKind {
    Mention,
    Plain,
    Url,
}

fn parse_angle(inner: &str, resolve: &dyn Fn(Ref) -> Option<String>) -> Option<(String, AngleKind)> {
    let id = |s: &str| s.parse::<Id>().ok();
    if let Some(r) = inner.strip_prefix("@&") {
        let n = resolve(Ref::Role(id(r)?)).unwrap_or_else(|| "deleted-role".into());
        return Some((format!("@{n}"), AngleKind::Mention));
    }
    if let Some(u) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        let n = resolve(Ref::User(id(u)?)).unwrap_or_else(|| "unknown-user".into());
        return Some((format!("@{n}"), AngleKind::Mention));
    }
    if let Some(c) = inner.strip_prefix('#') {
        let n = resolve(Ref::Channel(id(c)?)).unwrap_or_else(|| "unknown".into());
        return Some((format!("#{n}"), AngleKind::Mention));
    }
    if let Some(e) = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':')) {
        let (name, eid) = e.rsplit_once(':')?;
        id(eid)?;
        return Some((format!(":{name}:"), AngleKind::Plain));
    }
    if let Some(t) = inner.strip_prefix("t:") {
        let (ts, fmt) = t.split_once(':').unwrap_or((t, "f"));
        let ts: i64 = ts.parse().ok()?;
        return Some((format_timestamp(ts, fmt), AngleKind::Mention));
    }
    if inner.starts_with("https://") || inner.starts_with("http://") {
        return Some((inner.to_owned(), AngleKind::Url));
    }
    None
}

fn format_timestamp(ts: i64, fmt: &str) -> String {
    use chrono::{Local, TimeZone};
    let Some(t) = Local.timestamp_opt(ts, 0).single() else { return ts.to_string() };
    let pattern = match fmt {
        "t" => "%H:%M",
        "T" => "%H:%M:%S",
        "d" => "%Y-%m-%d",
        "D" => "%B %-d, %Y",
        "F" => "%A, %B %-d, %Y %H:%M",
        "R" => {
            let secs = ts - Local::now().timestamp();
            let (n, unit) = match secs.abs() {
                s if s < 60 => (s, "second"),
                s if s < 3600 => (s / 60, "minute"),
                s if s < 86400 => (s / 3600, "hour"),
                s if s < 86400 * 30 => (s / 86400, "day"),
                s if s < 86400 * 365 => (s / (86400 * 30), "month"),
                s => (s / (86400 * 365), "year"),
            };
            let pl = if n == 1 { "" } else { "s" };
            return if secs >= 0 { format!("in {n} {unit}{pl}") } else { format!("{n} {unit}{pl} ago") };
        }
        _ => "%B %-d, %Y %H:%M",
    };
    t.format(pattern).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme {
            size: 14.0,
            text: Color32::WHITE,
            strong: Color32::WHITE,
            weak: Color32::GRAY,
            link: Color32::BLUE,
            code_bg: Color32::BLACK,
            mention_fg: Color32::WHITE,
            mention_bg: Color32::DARK_BLUE,
            spoiler: Color32::BLACK,
        }
    }

    fn r(s: &str) -> Rendered {
        render(s, &theme(), &|r| match r {
            Ref::User(1) => Some("alice".into()),
            Ref::Channel(2) => Some("general".into()),
            _ => None,
        })
    }

    #[test]
    fn strips_markers_and_resolves() {
        let out = r("**hi** <@1> in <#2> `x*y` snake_case_name *it*");
        assert_eq!(out.job.text, "hi @alice in #general x*y snake_case_name it");
        assert!(out.job.sections.iter().any(|s| s.format.italics));
    }

    #[test]
    fn links_have_char_ranges() {
        let out = r("é see https://example.com/a_(b). ok");
        assert_eq!(out.job.text, "é see https://example.com/a_(b). ok");
        let (range, url) = &out.links[0];
        assert_eq!(url, "https://example.com/a_(b)");
        assert_eq!(range.start, 6);
        assert_eq!(out.link_at(10), Some("https://example.com/a_(b)"));
        assert_eq!(out.link_at(2), None);
    }

    #[test]
    fn code_blocks() {
        let out = r("before\n```rust\nfn main() {}\n```\nafter");
        assert_eq!(out.job.text, "before\nfn main() {}\nafter");
    }

    #[test]
    fn unmatched_markers_are_literal() {
        assert_eq!(r("2 * 3 = 6").job.text, "2 * 3 = 6");
        assert_eq!(r("a ** b").job.text, "a ** b");
        assert_eq!(r("<notamention>").job.text, "<notamention>");
    }
}
