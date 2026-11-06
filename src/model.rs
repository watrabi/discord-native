//! Minimal, lenient Discord API types.
//!
//! Only the fields the client actually uses are deserialized; everything else
//! is skipped by serde without allocating. Every field that Discord may omit
//! has a default so that a schema change in one object never breaks a whole
//! payload (which matters most for the multi-megabyte READY event).

use serde::{Deserialize, Deserializer};

pub type Id = u64;

/// Discord epoch (2015-01-01T00:00:00Z) in unix milliseconds.
pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

pub fn snowflake_ms(id: Id) -> u64 {
    (id >> 22) + DISCORD_EPOCH_MS
}

/// Snowflakes arrive as strings ("123") but some legacy fields use numbers.
pub mod sf {
    use super::*;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        String(String),
        Num(u64),
    }

    fn conv(r: Raw) -> Result<u64, String> {
        match r {
            Raw::String(s) => s.parse().map_err(|_| format!("bad snowflake {s:?}")),
            Raw::Num(n) => Ok(n),
        }
    }

    pub fn de<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        conv(Raw::deserialize(d)?).map_err(serde::de::Error::custom)
    }

    pub fn opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        match Option::<Raw>::deserialize(d)? {
            None => Ok(None),
            Some(r) => Ok(conv(r).ok().filter(|&v| v != 0)),
        }
    }

    pub fn vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u64>, D::Error> {
        let v = Option::<Vec<Raw>>::deserialize(d)?.unwrap_or_default();
        Ok(v.into_iter().filter_map(|r| conv(r).ok()).collect())
    }
}

/// Permission bitfields are serialized as decimal strings.
fn perm_str<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        S(String),
        N(u64),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        Some(Raw::S(s)) => s.parse().unwrap_or(0),
        Some(Raw::N(n)) => n,
        None => 0,
    })
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.username)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub user: Option<User>,
    /// Present in `merged_members` (user accounts) instead of `user`.
    #[serde(default, deserialize_with = "sf::opt")]
    pub user_id: Option<Id>,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default, deserialize_with = "sf::vec")]
    pub roles: Vec<Id>,
}

impl Member {
    pub fn id(&self) -> Option<Id> {
        self.user.as_ref().map(|u| u.id).or(self.user_id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Role {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "perm_str")]
    pub permissions: u64,
    #[serde(default)]
    pub position: i32,
    #[serde(default)]
    pub color: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Overwrite {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    /// 0 = role, 1 = member
    #[serde(rename = "type", default, deserialize_with = "overwrite_kind")]
    pub kind: u8,
    #[serde(default, deserialize_with = "perm_str")]
    pub allow: u64,
    #[serde(default, deserialize_with = "perm_str")]
    pub deny: u64,
}

fn overwrite_kind<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        N(u8),
        S(String),
    }
    Ok(match Raw::deserialize(d)? {
        Raw::N(n) => n,
        Raw::S(s) => u8::from(s == "member"),
    })
}

pub mod channel_kind {
    pub const TEXT: u8 = 0;
    pub const DM: u8 = 1;
    pub const VOICE: u8 = 2;
    pub const GROUP_DM: u8 = 3;
    pub const CATEGORY: u8 = 4;
    pub const ANNOUNCEMENT: u8 = 5;
    pub const ANNOUNCEMENT_THREAD: u8 = 10;
    pub const PUBLIC_THREAD: u8 = 11;
    pub const PRIVATE_THREAD: u8 = 12;
    pub const STAGE: u8 = 13;
    pub const FORUM: u8 = 15;
    pub const MEDIA: u8 = 16;
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default, deserialize_with = "sf::opt")]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub position: i32,
    #[serde(default, deserialize_with = "sf::opt")]
    pub parent_id: Option<Id>,
    #[serde(default, deserialize_with = "sf::opt")]
    pub last_message_id: Option<Id>,
    #[serde(default, deserialize_with = "null_default")]
    pub recipients: Vec<User>,
    #[serde(default, deserialize_with = "sf::vec")]
    pub recipient_ids: Vec<Id>,
    #[serde(default, deserialize_with = "null_default")]
    pub permission_overwrites: Vec<Overwrite>,
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GuildProperties {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "sf::opt")]
    pub owner_id: Option<Id>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Guild {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "sf::opt")]
    pub owner_id: Option<Id>,
    /// Newer user-account READY payloads nest name/owner here.
    #[serde(default)]
    pub properties: Option<GuildProperties>,
    #[serde(default, deserialize_with = "null_default")]
    pub channels: Vec<Channel>,
    #[serde(default, deserialize_with = "null_default")]
    pub roles: Vec<Role>,
    #[serde(default, deserialize_with = "null_default")]
    pub members: Vec<Member>,
    #[serde(default)]
    pub unavailable: bool,
}

impl Guild {
    pub fn name(&self) -> &str {
        self.name
            .as_deref()
            .or_else(|| self.properties.as_ref().and_then(|p| p.name.as_deref()))
            .unwrap_or("Unknown server")
    }
    pub fn owner(&self) -> Option<Id> {
        self.owner_id.or_else(|| self.properties.as_ref().and_then(|p| p.owner_id))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Embed {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Emoji {
    #[serde(default, deserialize_with = "sf::opt")]
    pub id: Option<Id>,
    #[serde(default)]
    pub name: Option<String>,
}

impl Emoji {
    /// Text shown in the UI.
    pub fn display(&self) -> String {
        match (&self.id, &self.name) {
            (Some(_), Some(n)) => format!(":{n}:"),
            (None, Some(n)) => n.clone(),
            _ => "?".into(),
        }
    }
    /// Identifier used in reaction REST routes (`name` or `name:id`).
    pub fn route(&self) -> String {
        match (&self.id, &self.name) {
            (Some(id), Some(n)) => format!("{n}:{id}"),
            (Some(id), None) => format!("_:{id}"),
            (None, Some(n)) => n.clone(),
            _ => String::new(),
        }
    }
    pub fn same(&self, other: &Emoji) -> bool {
        match (self.id, other.id) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.name == other.name,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reaction {
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub me: bool,
    pub emoji: Emoji,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(deserialize_with = "sf::de")]
    pub channel_id: Id,
    #[serde(default, deserialize_with = "sf::opt")]
    pub guild_id: Option<Id>,
    pub author: User,
    #[serde(default)]
    pub member: Option<Member>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub attachments: Vec<Attachment>,
    #[serde(default, deserialize_with = "null_default")]
    pub embeds: Vec<Embed>,
    #[serde(default, deserialize_with = "null_default")]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default, deserialize_with = "sf::vec")]
    pub mention_roles: Vec<Id>,
    #[serde(default, deserialize_with = "null_default")]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub referenced_message: Option<Box<Message>>,
    #[serde(rename = "type", default)]
    pub kind: u8,
}

/// MESSAGE_UPDATE is a partial message.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageUpdate {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(deserialize_with = "sf::de")]
    pub channel_id: Id,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
    #[serde(default)]
    pub embeds: Option<Vec<Embed>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageDelete {
    #[serde(default, deserialize_with = "sf::opt")]
    pub id: Option<Id>,
    #[serde(default, deserialize_with = "sf::vec")]
    pub ids: Vec<Id>,
    #[serde(deserialize_with = "sf::de")]
    pub channel_id: Id,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReactionEvent {
    #[serde(deserialize_with = "sf::de")]
    pub user_id: Id,
    #[serde(deserialize_with = "sf::de")]
    pub channel_id: Id,
    #[serde(deserialize_with = "sf::de")]
    pub message_id: Id,
    pub emoji: Emoji,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TypingStart {
    #[serde(deserialize_with = "sf::de")]
    pub channel_id: Id,
    #[serde(deserialize_with = "sf::de")]
    pub user_id: Id,
    #[serde(default)]
    pub member: Option<Member>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadStateEntry {
    /// Channel id.
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(default, deserialize_with = "sf::opt")]
    pub last_message_id: Option<Id>,
    #[serde(default, deserialize_with = "null_default")]
    pub mention_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelOverride {
    #[serde(deserialize_with = "sf::de")]
    pub channel_id: Id,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildSettings {
    #[serde(default, deserialize_with = "sf::opt")]
    pub guild_id: Option<Id>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default, deserialize_with = "null_default")]
    pub channel_overrides: Vec<ChannelOverride>,
}

/// Several READY fields are either a bare array or `{ "entries": [...] }`
/// depending on client capabilities.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Entries<T> {
    List(Vec<T>),
    Wrapped {
        #[serde(default = "Vec::new")]
        entries: Vec<T>,
    },
}

impl<T> Default for Entries<T> {
    fn default() -> Self {
        Entries::List(Vec::new())
    }
}

impl<T> Entries<T> {
    pub fn into_vec(self) -> Vec<T> {
        match self {
            Entries::List(v) | Entries::Wrapped { entries: v } => v,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ready {
    pub session_id: String,
    #[serde(default)]
    pub resume_gateway_url: Option<String>,
    pub user: User,
    #[serde(default, deserialize_with = "null_default")]
    pub guilds: Vec<Guild>,
    #[serde(default, deserialize_with = "null_default")]
    pub private_channels: Vec<Channel>,
    #[serde(default, deserialize_with = "null_default")]
    pub users: Vec<User>,
    #[serde(default, deserialize_with = "null_default")]
    pub read_state: Entries<ReadStateEntry>,
    #[serde(default, deserialize_with = "null_default")]
    pub user_guild_settings: Entries<GuildSettings>,
    /// User accounts: own member object per guild, same order as `guilds`.
    #[serde(default, deserialize_with = "null_default")]
    pub merged_members: Vec<Vec<Member>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildRoleEvent {
    #[serde(deserialize_with = "sf::de")]
    pub guild_id: Id,
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default, deserialize_with = "sf::opt")]
    pub role_id: Option<Id>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildMemberUpdate {
    #[serde(deserialize_with = "sf::de")]
    pub guild_id: Id,
    pub user: User,
    #[serde(default, deserialize_with = "sf::vec")]
    pub roles: Vec<Id>,
    #[serde(default)]
    pub nick: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageAck {
    #[serde(deserialize_with = "sf::de")]
    pub channel_id: Id,
    #[serde(deserialize_with = "sf::de")]
    pub message_id: Id,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnavailableGuild {
    #[serde(deserialize_with = "sf::de")]
    pub id: Id,
    #[serde(default)]
    pub unavailable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lenient_message() {
        let m: Message = serde_json::from_str(
            r#"{"id":"1100000000000000000","channel_id":"5","author":{"id":"7","username":"a","global_name":null},
                "content":"hi","attachments":null,"mentions":[],"mention_roles":["9"],"type":0,
                "reactions":[{"count":2,"me":true,"emoji":{"id":null,"name":"👍"}}],
                "unknown_field":{"deep":[1,2,3]}}"#,
        )
        .unwrap();
        assert_eq!(m.channel_id, 5);
        assert_eq!(m.mention_roles, vec![9]);
        assert!(m.attachments.is_empty());
        assert_eq!(m.reactions[0].emoji.route(), "👍");
    }

    #[test]
    fn parses_ready_variants() {
        let r: Ready = serde_json::from_str(
            r#"{"session_id":"s","user":{"id":"1","username":"me"},
                "guilds":[{"id":"2","properties":{"name":"G","owner_id":"1"},"channels":[
                    {"id":"3","type":0,"name":"general","position":null,"permission_overwrites":[{"id":"2","type":"role","allow":"0","deny":"1024"}]}]}],
                "read_state":{"entries":[{"id":"3","last_message_id":0,"mention_count":1}],"partial":false},
                "user_guild_settings":[{"guild_id":null,"muted":false,"channel_overrides":[]}],
                "private_channels":[{"id":"4","type":1,"recipient_ids":["8"],"last_message_id":"10"}]}"#,
        )
        .unwrap();
        assert_eq!(r.guilds[0].name(), "G");
        assert_eq!(r.guilds[0].owner(), Some(1));
        assert_eq!(r.guilds[0].channels[0].permission_overwrites[0].deny, 1024);
        let rs = r.read_state.into_vec();
        assert_eq!(rs[0].last_message_id, None);
        assert_eq!(r.private_channels[0].recipient_ids, vec![8]);
    }
}
