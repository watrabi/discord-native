//! Networking: one background thread running a single-threaded tokio runtime
//! that owns the gateway websocket and all REST calls.
//!
//! The UI never blocks on the network: it sends [`Command`]s and drains
//! [`NetEvent`]s, and the network thread wakes the UI with
//! `request_repaint()` only when something actually arrived. When nothing
//! happens the process sleeps (no polling, no timers other than heartbeats).

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use flate2::{Decompress, FlushDecompress};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

use crate::model::*;

const GATEWAY_QUERY: &str = "/?v=10&encoding=json&compress=zlib-stream";

/// REST base; override with `DISCORD_API_BASE` (e.g. for Discord-compatible servers or testing).
static API_BASE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    std::env::var("DISCORD_API_BASE").unwrap_or_else(|_| "https://discord.com/api/v10".into())
});
/// Gateway base; override with `DISCORD_GATEWAY`.
static GATEWAY_BASE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| std::env::var("DISCORD_GATEWAY").unwrap_or_else(|_| "wss://gateway.discord.gg".into()));
const USER_AGENT: &str =
    concat!("DiscordBot (https://github.com/watrabi/discord-native, ", env!("CARGO_PKG_VERSION"), ")");

#[derive(Clone)]
pub struct Credentials {
    pub token: String,
    pub bot: bool,
}

impl Credentials {
    fn auth_header(&self) -> String {
        if self.bot { format!("Bot {}", self.token) } else { self.token.clone() }
    }
}

/// Requests from the UI thread.
#[derive(Debug)]
pub enum Command {
    FetchHistory { channel_id: Id, before: Option<Id> },
    Send { channel_id: Id, content: String, reply_to: Option<Id>, files: Vec<PathBuf> },
    Edit { channel_id: Id, message_id: Id, content: String },
    Delete { channel_id: Id, message_id: Id },
    React { channel_id: Id, message_id: Id, emoji: String, add: bool },
    Typing { channel_id: Id },
    Ack { channel_id: Id, message_id: Id },
}

/// Results / events delivered to the UI thread.
#[derive(Debug)]
pub enum NetEvent {
    Status(String),
    AuthFailed(String),
    Ready(Box<Ready>),
    GuildCreate(Box<Guild>),
    GuildUpdate(Box<Guild>),
    GuildDelete(UnavailableGuild),
    ChannelUpsert(Box<Channel>),
    ChannelDelete(Box<Channel>),
    Role(GuildRoleEvent, bool /* deleted */),
    MemberUpdate(GuildMemberUpdate),
    MessageCreate(Box<Message>),
    MessageUpdate(Box<MessageUpdate>),
    MessageDelete(MessageDelete),
    Reaction(ReactionEvent, bool /* added */),
    Typing(TypingStart),
    Ack(MessageAck),
    History { channel_id: Id, before: Option<Id>, messages: Vec<Message> },
    HistoryFailed { channel_id: Id, status: u16 },
    Error(String),
}

pub struct NetHandle {
    pub tx: mpsc::UnboundedSender<Command>,
    pub rx: std_mpsc::Receiver<NetEvent>,
}

impl NetHandle {
    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
}

/// Sender that wakes the UI after every event.
#[derive(Clone)]
struct Out {
    tx: std_mpsc::Sender<NetEvent>,
    ctx: eframe::egui::Context,
}

impl Out {
    fn send(&self, e: NetEvent) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }
}

pub fn spawn(creds: Credentials, ctx: eframe::egui::Context) -> NetHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = std_mpsc::channel();
    let out = Out { tx: ev_tx, ctx };
    std::thread::Builder::new()
        .name("net".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(2)
                .build()
                .expect("tokio runtime");
            rt.block_on(run(creds, out, cmd_rx));
        })
        .expect("spawn net thread");
    NetHandle { tx: cmd_tx, rx: ev_rx }
}

async fn run(creds: Credentials, out: Out, mut cmds: mpsc::UnboundedReceiver<Command>) {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        creds.auth_header().parse().expect("token contains invalid characters"),
    );
    let http = match reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .pool_max_idle_per_host(2)
        .pool_idle_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            out.send(NetEvent::AuthFailed(format!("HTTP client: {e}")));
            return;
        }
    };
    let rest = Rest { http, bot: creds.bot };

    tokio::spawn(gateway_loop(creds, out.clone()));

    while let Some(cmd) = cmds.recv().await {
        let rest = rest.clone();
        let out = out.clone();
        tokio::spawn(async move { rest.handle(cmd, out).await });
    }
}

// ---------------------------------------------------------------- REST ----

#[derive(Clone)]
struct Rest {
    http: reqwest::Client,
    bot: bool,
}

enum Body {
    None,
    Json(serde_json::Value),
    Multipart(serde_json::Value, Vec<PathBuf>),
}

impl Rest {
    async fn handle(&self, cmd: Command, out: Out) {
        use reqwest::Method;
        let api = API_BASE.as_str();
        match cmd {
            Command::FetchHistory { channel_id, before } => {
                let mut url = format!("{api}/channels/{channel_id}/messages?limit=50");
                if let Some(b) = before {
                    url.push_str(&format!("&before={b}"));
                }
                match self.request(Method::GET, &url, Body::None).await {
                    Ok(bytes) => match serde_json::from_slice::<Vec<Message>>(&bytes) {
                        Ok(messages) => out.send(NetEvent::History { channel_id, before, messages }),
                        Err(e) => {
                            out.send(NetEvent::HistoryFailed { channel_id, status: 0 });
                            out.send(NetEvent::Error(format!("Bad history payload: {e}")));
                        }
                    },
                    Err((status, msg)) => {
                        out.send(NetEvent::HistoryFailed { channel_id, status });
                        if status != 403 {
                            out.send(NetEvent::Error(msg));
                        }
                    }
                }
            }
            Command::Send { channel_id, content, reply_to, files } => {
                let mut payload = json!({ "content": content });
                if let Some(r) = reply_to {
                    payload["message_reference"] = json!({ "message_id": r.to_string(), "fail_if_not_exists": false });
                }
                if !self.bot {
                    // Lets the server de-duplicate retries.
                    payload["nonce"] = json!(nonce().to_string());
                }
                let url = format!("{api}/channels/{channel_id}/messages");
                let body = if files.is_empty() { Body::Json(payload) } else { Body::Multipart(payload, files) };
                if let Err((_, e)) = self.request(Method::POST, &url, body).await {
                    out.send(NetEvent::Error(format!("Send failed: {e}")));
                }
            }
            Command::Edit { channel_id, message_id, content } => {
                let url = format!("{api}/channels/{channel_id}/messages/{message_id}");
                if let Err((_, e)) = self.request(Method::PATCH, &url, Body::Json(json!({ "content": content }))).await
                {
                    out.send(NetEvent::Error(format!("Edit failed: {e}")));
                }
            }
            Command::Delete { channel_id, message_id } => {
                let url = format!("{api}/channels/{channel_id}/messages/{message_id}");
                if let Err((_, e)) = self.request(Method::DELETE, &url, Body::None).await {
                    out.send(NetEvent::Error(format!("Delete failed: {e}")));
                }
            }
            Command::React { channel_id, message_id, emoji, add } => {
                let url = format!(
                    "{api}/channels/{channel_id}/messages/{message_id}/reactions/{}/@me",
                    percent_encode(&emoji)
                );
                let m = if add { Method::PUT } else { Method::DELETE };
                if let Err((_, e)) = self.request(m, &url, Body::None).await {
                    out.send(NetEvent::Error(format!("Reaction failed: {e}")));
                }
            }
            Command::Typing { channel_id } => {
                let _ = self.request(Method::POST, &format!("{api}/channels/{channel_id}/typing"), Body::None).await;
            }
            Command::Ack { channel_id, message_id } => {
                if !self.bot {
                    let url = format!("{api}/channels/{channel_id}/messages/{message_id}/ack");
                    let _ = self.request(Method::POST, &url, Body::Json(json!({ "token": null }))).await;
                }
            }
        }
    }

    /// Performs a request, transparently waiting out rate limits.
    async fn request(&self, method: reqwest::Method, url: &str, body: Body) -> Result<bytes::Bytes, (u16, String)> {
        for _attempt in 0..4 {
            let mut req = self.http.request(method.clone(), url);
            req = match &body {
                Body::None => req,
                Body::Json(v) => req.json(v),
                Body::Multipart(payload, files) => {
                    let mut form = reqwest::multipart::Form::new().text("payload_json", payload.to_string());
                    for (i, path) in files.iter().enumerate() {
                        let p = path.clone();
                        let data = tokio::task::spawn_blocking(move || std::fs::read(&p))
                            .await
                            .map_err(|e| (0, e.to_string()))?
                            .map_err(|e| (0, format!("{}: {e}", path.display())))?;
                        let name =
                            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                        form = form.part(format!("files[{i}]"), reqwest::multipart::Part::bytes(data).file_name(name));
                    }
                    req.multipart(form)
                }
            };
            let resp = req.send().await.map_err(|e| (0, e.to_string()))?;
            let status = resp.status().as_u16();
            if status == 429 {
                #[derive(Deserialize)]
                struct Limited {
                    retry_after: f64,
                }
                let wait = resp.json::<Limited>().await.map(|l| l.retry_after).unwrap_or(1.0).clamp(0.05, 30.0);
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            let bytes = resp.bytes().await.map_err(|e| (status, e.to_string()))?;
            if (200..300).contains(&status) {
                return Ok(bytes);
            }
            #[derive(Deserialize)]
            struct ApiError {
                #[serde(default)]
                message: String,
            }
            let msg = serde_json::from_slice::<ApiError>(&bytes)
                .map(|e| e.message)
                .unwrap_or_else(|_| String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned());
            return Err((status, format!("HTTP {status}: {msg}")));
        }
        Err((429, "rate limited".into()))
    }
}

fn nonce() -> u64 {
    let ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    ms.saturating_sub(DISCORD_EPOCH_MS) << 22
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ------------------------------------------------------------- Gateway ----

/// Streaming zlib inflater for `compress=zlib-stream`: one shared context for
/// the whole connection, messages end with the Z_SYNC_FLUSH suffix.
pub struct Inflater {
    z: Decompress,
    input: Vec<u8>,
    output: Vec<u8>,
}

impl Inflater {
    pub fn new() -> Self {
        Self { z: Decompress::new(true), input: Vec::new(), output: Vec::new() }
    }

    pub fn push(&mut self, data: &[u8]) -> Result<Option<&[u8]>, flate2::DecompressError> {
        self.input.extend_from_slice(data);
        if !self.input.ends_with(&[0, 0, 0xff, 0xff]) {
            return Ok(None);
        }
        self.output.clear();
        let mut offset = 0;
        loop {
            if self.output.capacity() - self.output.len() < 16 * 1024 {
                self.output.reserve((self.input.len() * 4).max(32 * 1024));
            }
            let before = self.z.total_in();
            self.z.decompress_vec(&self.input[offset..], &mut self.output, FlushDecompress::Sync)?;
            offset += (self.z.total_in() - before) as usize;
            if offset >= self.input.len() && self.output.len() < self.output.capacity() {
                break;
            }
        }
        self.input.clear();
        Ok(Some(&self.output))
    }

    /// Release memory after a large payload (READY can be many MB).
    pub fn trim(&mut self) {
        const KEEP: usize = 256 * 1024;
        if self.output.capacity() > KEEP {
            self.output = Vec::with_capacity(32 * 1024);
        }
        if self.input.capacity() > KEEP {
            self.input = Vec::new();
        }
    }
}

#[derive(Deserialize)]
struct Payload<'a> {
    op: u8,
    #[serde(borrow, default)]
    d: Option<&'a RawValue>,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<&'a str>,
}

#[derive(Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

struct Session {
    id: String,
    resume_url: String,
    seq: Option<u64>,
}

enum Exit {
    /// Reconnect, resuming if we still have a session.
    Reconnect,
    /// Reconnect with a fresh IDENTIFY.
    Reidentify,
    /// Unrecoverable (bad token, etc.).
    Fatal(String),
    /// Bot is missing the privileged MESSAGE_CONTENT intent.
    DisallowedIntents,
}

const INTENTS_BASE: u64 = 1 // GUILDS
    | 1 << 9  // GUILD_MESSAGES
    | 1 << 10 // GUILD_MESSAGE_REACTIONS
    | 1 << 11 // GUILD_MESSAGE_TYPING
    | 1 << 12 // DIRECT_MESSAGES
    | 1 << 13 // DIRECT_MESSAGE_REACTIONS
    | 1 << 14; // DIRECT_MESSAGE_TYPING
const INTENT_MESSAGE_CONTENT: u64 = 1 << 15;

async fn gateway_loop(creds: Credentials, out: Out) {
    let mut session: Option<Session> = None;
    let mut intents = INTENTS_BASE | INTENT_MESSAGE_CONTENT;
    let mut backoff = 1u64;
    loop {
        let url = session
            .as_ref()
            .map(|s| s.resume_url.as_str())
            .unwrap_or(GATEWAY_BASE.as_str())
            .trim_end_matches('/')
            .to_string()
            + GATEWAY_QUERY;
        out.send(NetEvent::Status("Connecting…".into()));
        let started = std::time::Instant::now();
        let exit = match tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(&url)).await {
            Ok(Ok((ws, _))) => connection(ws, &creds, &out, &mut session, intents).await,
            Ok(Err(e)) => {
                out.send(NetEvent::Status(format!("Connection failed: {e}")));
                Exit::Reconnect
            }
            Err(_) => {
                out.send(NetEvent::Status("Connection timed out".into()));
                Exit::Reconnect
            }
        };
        match exit {
            Exit::Fatal(msg) => {
                out.send(NetEvent::AuthFailed(msg));
                return;
            }
            Exit::DisallowedIntents => {
                if intents & INTENT_MESSAGE_CONTENT != 0 {
                    intents &= !INTENT_MESSAGE_CONTENT;
                    out.send(NetEvent::Error(
                        "MESSAGE_CONTENT intent is not enabled for this bot; message text will be empty. \
                         Enable it in the Developer Portal."
                            .into(),
                    ));
                    session = None;
                    continue;
                }
                out.send(NetEvent::AuthFailed("Gateway rejected the requested intents".into()));
                return;
            }
            Exit::Reidentify => session = None,
            Exit::Reconnect => {}
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        out.send(NetEvent::Status(format!("Disconnected, retrying in {backoff}s")));
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

async fn connection<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    creds: &Credentials,
    out: &Out,
    session: &mut Option<Session>,
    intents: u64,
) -> Exit
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    let mut inflater = Inflater::new();
    let mut heartbeat: Option<tokio::time::Interval> = None;
    let mut acked = true;

    loop {
        let tick = async {
            match heartbeat.as_mut() {
                Some(h) => {
                    h.tick().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let frame = tokio::select! {
            _ = tick => {
                if !acked {
                    // Zombied connection: close and resume.
                    let _ = sink.send(WsMessage::Close(Some(CloseFrame { code: 4000u16.into(), reason: "zombie".into() }))).await;
                    return Exit::Reconnect;
                }
                acked = false;
                let seq = session.as_ref().and_then(|s| s.seq);
                if sink.send(WsMessage::text(json!({ "op": 1, "d": seq }).to_string())).await.is_err() {
                    return Exit::Reconnect;
                }
                continue;
            }
            f = stream.next() => f,
        };

        let data: Vec<u8> = match frame {
            None => return Exit::Reconnect,
            Some(Err(e)) => {
                out.send(NetEvent::Status(format!("Gateway error: {e}")));
                return Exit::Reconnect;
            }
            Some(Ok(WsMessage::Binary(b))) => match inflater.push(&b) {
                Ok(Some(d)) => d.to_vec(),
                Ok(None) => continue,
                Err(e) => {
                    out.send(NetEvent::Status(format!("Decompression error: {e}")));
                    return Exit::Reidentify;
                }
            },
            Some(Ok(WsMessage::Text(t))) => t.as_bytes().to_vec(),
            Some(Ok(WsMessage::Close(frame))) => {
                let code: u16 = frame.as_ref().map(|f| f.code.into()).unwrap_or(1000);
                let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                return match code {
                    4004 => Exit::Fatal("Authentication failed: invalid token".into()),
                    4014 => Exit::DisallowedIntents,
                    4010..=4013 => Exit::Fatal(format!("Gateway closed ({code}): {reason}")),
                    4007 | 4009 => Exit::Reidentify,
                    _ => Exit::Reconnect,
                };
            }
            Some(Ok(_)) => continue,
        };
        inflater.trim();

        let payload: Payload = match serde_json::from_slice(&data) {
            Ok(p) => p,
            Err(e) => {
                out.send(NetEvent::Error(format!("Bad gateway payload: {e}")));
                continue;
            }
        };
        if let (Some(s), Some(sess)) = (payload.s, session.as_mut()) {
            sess.seq = Some(s);
        }
        match payload.op {
            0 => {
                let (Some(t), Some(d)) = (payload.t, payload.d) else { continue };
                if t == "READY" {
                    match serde_json::from_str::<Ready>(d.get()) {
                        Ok(ready) => {
                            *session = Some(Session {
                                id: ready.session_id.clone(),
                                resume_url: ready.resume_gateway_url.clone().unwrap_or_else(|| GATEWAY_BASE.clone()),
                                seq: payload.s,
                            });
                            out.send(NetEvent::Status("Connected".into()));
                            out.send(NetEvent::Ready(Box::new(ready)));
                        }
                        Err(e) => out.send(NetEvent::Error(format!("Failed to parse READY: {e}"))),
                    }
                    continue;
                }
                if t == "RESUMED" {
                    out.send(NetEvent::Status("Connected".into()));
                    continue;
                }
                if let Some(ev) = dispatch(t, d.get()) {
                    out.send(ev);
                }
            }
            1 => {
                let seq = session.as_ref().and_then(|s| s.seq);
                let _ = sink.send(WsMessage::text(json!({ "op": 1, "d": seq }).to_string())).await;
            }
            7 => return Exit::Reconnect,
            9 => {
                let resumable = payload.d.map(|d| d.get() == "true").unwrap_or(false);
                tokio::time::sleep(Duration::from_millis(1000 + (nonce() >> 22) % 4000)).await;
                return if resumable { Exit::Reconnect } else { Exit::Reidentify };
            }
            10 => {
                let Some(hello) = payload.d.and_then(|d| serde_json::from_str::<Hello>(d.get()).ok()) else {
                    return Exit::Reconnect;
                };
                let period = Duration::from_millis(hello.heartbeat_interval.max(1000));
                let jitter = period.mul_f64(((nonce() >> 22) % 1000) as f64 / 1000.0);
                heartbeat = Some(tokio::time::interval_at(tokio::time::Instant::now() + jitter, period));
                let msg = match session.as_ref() {
                    Some(s) => {
                        json!({ "op": 6, "d": { "token": creds_token(creds), "session_id": s.id, "seq": s.seq } })
                    }
                    None => identify(creds, intents),
                };
                if sink.send(WsMessage::text(msg.to_string())).await.is_err() {
                    return Exit::Reconnect;
                }
            }
            11 => acked = true,
            _ => {}
        }
    }
}

fn creds_token(c: &Credentials) -> String {
    if c.bot { format!("Bot {}", c.token) } else { c.token.clone() }
}

fn identify(creds: &Credentials, intents: u64) -> serde_json::Value {
    let props = json!({ "os": std::env::consts::OS, "browser": "discord-native", "device": "discord-native" });
    if creds.bot {
        json!({ "op": 2, "d": {
            "token": creds_token(creds), "intents": intents, "properties": props,
            "compress": false, "large_threshold": 50
        }})
    } else {
        json!({ "op": 2, "d": {
            "token": creds.token, "properties": props, "capabilities": 0,
            "compress": false, "large_threshold": 50,
            "presence": { "status": "online", "since": 0, "activities": [], "afk": false }
        }})
    }
}

fn dispatch(t: &str, d: &str) -> Option<NetEvent> {
    fn p<'a, T: Deserialize<'a>>(d: &'a str) -> Option<T> {
        serde_json::from_str(d).ok()
    }
    Some(match t {
        "MESSAGE_CREATE" => NetEvent::MessageCreate(p(d)?),
        "MESSAGE_UPDATE" => NetEvent::MessageUpdate(p(d)?),
        "MESSAGE_DELETE" | "MESSAGE_DELETE_BULK" => NetEvent::MessageDelete(p(d)?),
        "MESSAGE_REACTION_ADD" => NetEvent::Reaction(p(d)?, true),
        "MESSAGE_REACTION_REMOVE" => NetEvent::Reaction(p(d)?, false),
        "TYPING_START" => NetEvent::Typing(p(d)?),
        "MESSAGE_ACK" => NetEvent::Ack(p(d)?),
        "GUILD_CREATE" => NetEvent::GuildCreate(p(d)?),
        "GUILD_UPDATE" => NetEvent::GuildUpdate(p(d)?),
        "GUILD_DELETE" => NetEvent::GuildDelete(p(d)?),
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" => NetEvent::ChannelUpsert(p(d)?),
        "CHANNEL_DELETE" => NetEvent::ChannelDelete(p(d)?),
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => NetEvent::Role(p(d)?, false),
        "GUILD_ROLE_DELETE" => NetEvent::Role(p(d)?, true),
        "GUILD_MEMBER_UPDATE" => NetEvent::MemberUpdate(p(d)?),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    #[test]
    fn inflater_handles_stream_and_split_frames() {
        let mut c = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();
        for msg in [r#"{"op":10,"d":{"heartbeat_interval":41250}}"#, &"x".repeat(200_000)] {
            let mut buf = Vec::with_capacity(msg.len() + 1024);
            c.compress_vec(msg.as_bytes(), &mut buf, FlushCompress::Sync).unwrap();
            assert!(buf.ends_with(&[0, 0, 0xff, 0xff]));
            let (a, b) = buf.split_at(buf.len() / 2);
            assert!(inf.push(a).unwrap().is_none());
            let got = inf.push(b).unwrap().unwrap();
            assert_eq!(got, msg.as_bytes());
            inf.trim();
        }
    }

    #[test]
    fn dispatch_parses_known_events() {
        let ev = dispatch("TYPING_START", r#"{"channel_id":"1","user_id":"2","timestamp":0}"#);
        assert!(matches!(ev, Some(NetEvent::Typing(t)) if t.user_id == 2));
        assert!(dispatch("PRESENCE_UPDATE", "{}").is_none());
    }

    #[test]
    fn encodes_emoji() {
        assert_eq!(percent_encode("👍"), "%F0%9F%91%8D");
        assert_eq!(percent_encode("blob:123"), "blob:123");
    }
}
