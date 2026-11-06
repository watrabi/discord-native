//! Token persistence in a user-only readable file.

use std::path::PathBuf;

use crate::net::Credentials;

fn dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(x).join("discord-native"));
    }
    #[cfg(windows)]
    if let Some(a) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(a).join("discord-native"));
    }
    let home = std::env::var_os("HOME")?;
    #[cfg(target_os = "macos")]
    return Some(PathBuf::from(home).join("Library/Application Support/discord-native"));
    #[cfg(not(target_os = "macos"))]
    Some(PathBuf::from(home).join(".config/discord-native"))
}

fn token_path() -> Option<PathBuf> {
    dir().map(|d| d.join("token"))
}

pub fn parse_token(raw: &str, bot_hint: bool) -> Credentials {
    let raw = raw.trim().trim_matches('"');
    match raw.strip_prefix("Bot ") {
        Some(t) => Credentials { token: t.trim().to_owned(), bot: true },
        None => Credentials { token: raw.to_owned(), bot: bot_hint },
    }
}

/// Environment (`DISCORD_TOKEN`, `DISCORD_BOT=1`) wins over the saved file.
pub fn load() -> Option<Credentials> {
    if let Ok(t) = std::env::var("DISCORD_TOKEN")
        && !t.trim().is_empty()
    {
        let bot = std::env::var("DISCORD_BOT").is_ok_and(|v| v == "1" || v == "true");
        return Some(parse_token(&t, bot));
    }
    let text = std::fs::read_to_string(token_path()?).ok()?;
    let mut lines = text.lines();
    let kind = lines.next()?.trim();
    let token = lines.next()?.trim();
    (!token.is_empty()).then(|| Credentials { token: token.to_owned(), bot: kind == "bot" })
}

pub fn save(c: &Credentials) -> std::io::Result<()> {
    let path = token_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let body = format!("{}\n{}\n", if c.bot { "bot" } else { "user" }, c.token);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&path)?.write_all(body.as_bytes())
}

pub fn forget() {
    if let Some(p) = token_path() {
        let _ = std::fs::remove_file(p);
    }
}
