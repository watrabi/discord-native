//! Client-side state, owned by the UI thread.
//!
//! Memory policy: only channels that were opened keep message history, at
//! most [`MAX_CACHED_CHANNELS`] of them; a channel that is not on screen is
//! trimmed to [`IDLE_KEEP`] messages. Everything else (guilds, channels,
//! names, read state) is small metadata.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::markdown::Rendered;
use crate::model::{self, channel_kind as ck, *};

pub const MAX_CACHED_CHANNELS: usize = 8;
pub const IDLE_KEEP: usize = 50;
pub const MAX_IN_VIEW: usize = 1000;

pub mod perm {
    pub const ADMINISTRATOR: u64 = 1 << 3;
    pub const VIEW_CHANNEL: u64 = 1 << 10;
    pub const SEND_MESSAGES: u64 = 1 << 11;
    pub const MANAGE_MESSAGES: u64 = 1 << 13;
    pub const ATTACH_FILES: u64 = 1 << 15;
    pub const ALL: u64 = u64::MAX;
}

pub struct GuildState {
    pub name: String,
    pub owner_id: Option<Id>,
    pub roles: HashMap<Id, Role>,
    /// Own role ids; `None` if unknown (then permission checks are skipped).
    pub my_roles: Option<Vec<Id>>,
    pub nicks: HashMap<Id, String>,
    pub channels: HashSet<Id>,
    pub muted: bool,
}

pub struct ChannelState {
    pub id: Id,
    pub kind: u8,
    pub guild_id: Option<Id>,
    pub name: String,
    pub topic: Option<String>,
    pub position: i32,
    pub parent_id: Option<Id>,
    pub last_message_id: Option<Id>,
    pub overwrites: Vec<Overwrite>,
    pub recipients: Vec<Id>,
    pub read_up_to: Option<Id>,
    pub mentions: u32,
    pub muted: bool,
    /// Set when the API answered 403.
    pub forbidden: bool,
}

impl ChannelState {
    pub fn unread(&self) -> bool {
        matches!((self.last_message_id, self.read_up_to), (Some(last), Some(read)) if last > read)
    }
    pub fn is_text(&self) -> bool {
        matches!(self.kind, ck::TEXT | ck::ANNOUNCEMENT | ck::DM | ck::GROUP_DM | ck::VOICE | ck::STAGE)
    }
}

pub struct Msg {
    pub id: Id,
    pub author_id: Id,
    pub author: String,
    pub author_bot: bool,
    /// RGB of the highest coloured role, 0 = none.
    pub author_color: u32,
    pub content: String,
    pub edited: bool,
    pub attachments: Vec<Attachment>,
    pub embeds: Vec<Embed>,
    pub reactions: Vec<Reaction>,
    pub reply: Option<(String, String)>,
    pub mentions_me: bool,
    pub kind: u8,
    pub mention_users: Vec<(Id, String)>,
    /// Lazily built text layout, invalidated on edit.
    pub rendered: Option<Arc<Rendered>>,
}

#[derive(Default)]
pub struct History {
    pub msgs: VecDeque<Msg>,
    pub reached_start: bool,
    pub loading: bool,
    pub loaded_once: bool,
    pub last_used: Option<Instant>,
}

#[derive(Default)]
pub struct Store {
    pub me: Option<User>,
    pub guilds: HashMap<Id, GuildState>,
    pub guild_order: Vec<Id>,
    pub channels: HashMap<Id, ChannelState>,
    pub users: HashMap<Id, String>,
    pub history: HashMap<Id, History>,
    pub typing: HashMap<Id, Vec<(Id, Instant)>>,
    /// Bumped whenever the channel list needs to be re-sorted.
    pub layout_gen: u64,
}

impl Store {
    pub fn my_id(&self) -> Id {
        self.me.as_ref().map(|u| u.id).unwrap_or(0)
    }

    pub fn user_name(&self, guild: Option<Id>, id: Id) -> Option<&str> {
        if let Some(g) = guild.and_then(|g| self.guilds.get(&g))
            && let Some(n) = g.nicks.get(&id)
        {
            return Some(n);
        }
        self.users.get(&id).map(|s| s.as_str())
    }

    fn remember_user(&mut self, u: &User) {
        let name = u.display_name();
        match self.users.get(&u.id) {
            Some(n) if n == name => {}
            _ => {
                self.users.insert(u.id, name.to_owned());
            }
        }
    }

    pub fn on_ready(&mut self, r: Ready) {
        // A fresh READY replaces everything; the UI re-fetches the open channel.
        *self = Store::default();
        self.remember_user(&r.user);
        self.me = Some(r.user);
        for u in &r.users {
            self.remember_user(u);
        }
        let my_id = self.my_id();
        let merged = r.merged_members;
        for (i, g) in r.guilds.into_iter().enumerate() {
            let own = merged.get(i).and_then(|v| v.iter().find(|m| m.id().is_none_or(|id| id == my_id)));
            let my_roles = own.map(|m| m.roles.clone());
            self.add_guild(g, my_roles);
        }
        for c in r.private_channels {
            self.upsert_channel(c);
        }
        let reads = r.read_state.into_vec();
        let has_read_state = !reads.is_empty();
        for e in reads {
            if let Some(c) = self.channels.get_mut(&e.id) {
                c.read_up_to = Some(e.last_message_id.unwrap_or(0));
                c.mentions = e.mention_count;
            }
        }
        // Bots (and channels without read state) start fully read.
        for c in self.channels.values_mut() {
            if c.read_up_to.is_none() {
                c.read_up_to = if has_read_state && c.guild_id.is_none() { Some(0) } else { c.last_message_id };
            }
        }
        for s in r.user_guild_settings.into_vec() {
            if let Some(g) = s.guild_id.and_then(|g| self.guilds.get_mut(&g)) {
                g.muted = s.muted;
            }
            for o in s.channel_overrides {
                if let Some(c) = self.channels.get_mut(&o.channel_id) {
                    c.muted = o.muted;
                }
            }
        }
        self.layout_gen += 1;
    }

    pub fn add_guild(&mut self, g: Guild, my_roles: Option<Vec<Id>>) {
        if g.unavailable && g.channels.is_empty() {
            return;
        }
        let my_id = self.my_id();
        let name = g.name().to_owned();
        let owner_id = g.owner();
        let mut my_roles = my_roles;
        let mut nicks = HashMap::new();
        for m in &g.members {
            if let Some(u) = &m.user {
                self.remember_user(u);
            }
            if let Some(id) = m.id() {
                if id == my_id {
                    my_roles = Some(m.roles.clone());
                }
                if let Some(n) = &m.nick {
                    nicks.insert(id, n.clone());
                }
            }
        }
        let (muted, old_channels) = match self.guilds.remove(&g.id) {
            Some(old) => {
                if my_roles.is_none() {
                    my_roles = old.my_roles;
                }
                nicks.extend(old.nicks);
                (old.muted, old.channels)
            }
            None => (false, HashSet::new()),
        };
        for c in &old_channels {
            self.channels.remove(c);
        }
        let gid = g.id;
        self.guilds.insert(
            gid,
            GuildState {
                name,
                owner_id,
                roles: g.roles.into_iter().map(|r| (r.id, r)).collect(),
                my_roles,
                nicks,
                channels: HashSet::new(),
                muted,
            },
        );
        if !self.guild_order.contains(&gid) {
            self.guild_order.push(gid);
        }
        for mut c in g.channels {
            c.guild_id = Some(gid);
            self.upsert_channel(c);
        }
        self.layout_gen += 1;
    }

    pub fn remove_guild(&mut self, id: Id) {
        if let Some(g) = self.guilds.remove(&id) {
            for c in g.channels {
                self.channels.remove(&c);
                self.history.remove(&c);
            }
        }
        self.guild_order.retain(|&g| g != id);
        self.layout_gen += 1;
    }

    pub fn update_guild(&mut self, g: Guild) {
        if let Some(gs) = self.guilds.get_mut(&g.id) {
            gs.name = g.name().to_owned();
            gs.owner_id = g.owner().or(gs.owner_id);
            if !g.roles.is_empty() {
                gs.roles = g.roles.into_iter().map(|r| (r.id, r)).collect();
            }
            self.layout_gen += 1;
        }
    }

    pub fn upsert_channel(&mut self, c: Channel) {
        for u in &c.recipients {
            self.remember_user(u);
        }
        let mut recipients: Vec<Id> = c.recipients.iter().map(|u| u.id).collect();
        recipients.extend(c.recipient_ids.iter().copied());
        recipients.dedup();
        let prev = self.channels.remove(&c.id);
        let name = c.name.clone().unwrap_or_default();
        let state = ChannelState {
            id: c.id,
            kind: c.kind,
            guild_id: c.guild_id,
            name,
            topic: c.topic.filter(|t| !t.is_empty()),
            position: c.position,
            parent_id: c.parent_id,
            last_message_id: c.last_message_id.or(prev.as_ref().and_then(|p| p.last_message_id)),
            overwrites: c.permission_overwrites,
            recipients: if recipients.is_empty() {
                prev.as_ref().map(|p| p.recipients.clone()).unwrap_or_default()
            } else {
                recipients
            },
            read_up_to: prev.as_ref().and_then(|p| p.read_up_to).or(c.last_message_id),
            mentions: prev.as_ref().map(|p| p.mentions).unwrap_or(0),
            muted: prev.as_ref().map(|p| p.muted).unwrap_or(false),
            forbidden: prev.as_ref().map(|p| p.forbidden).unwrap_or(false),
        };
        if let Some(g) = c.guild_id.and_then(|g| self.guilds.get_mut(&g)) {
            g.channels.insert(c.id);
        }
        self.channels.insert(c.id, state);
        self.layout_gen += 1;
    }

    pub fn remove_channel(&mut self, id: Id) {
        if let Some(c) = self.channels.remove(&id)
            && let Some(g) = c.guild_id.and_then(|g| self.guilds.get_mut(&g))
        {
            g.channels.remove(&id);
        }
        self.history.remove(&id);
        self.layout_gen += 1;
    }

    pub fn channel_display_name(&self, c: &ChannelState) -> String {
        if c.guild_id.is_some() || !c.name.is_empty() {
            return c.name.clone();
        }
        let names: Vec<&str> =
            c.recipients.iter().map(|id| self.users.get(id).map(|s| s.as_str()).unwrap_or("unknown")).collect();
        if names.is_empty() { "Empty group".into() } else { names.join(", ") }
    }

    /// Computes own permissions in a channel (Discord's documented algorithm).
    pub fn permissions(&self, channel_id: Id) -> u64 {
        let Some(c) = self.channels.get(&channel_id) else { return 0 };
        let Some(gid) = c.guild_id else { return perm::ALL };
        let Some(g) = self.guilds.get(&gid) else { return perm::ALL };
        let me = self.my_id();
        if g.owner_id == Some(me) {
            return perm::ALL;
        }
        let Some(my_roles) = &g.my_roles else { return perm::ALL };
        let mut p = g.roles.get(&gid).map(|r| r.permissions).unwrap_or(0);
        for r in my_roles {
            if let Some(role) = g.roles.get(r) {
                p |= role.permissions;
            }
        }
        if p & perm::ADMINISTRATOR != 0 {
            return perm::ALL;
        }
        if let Some(o) = c.overwrites.iter().find(|o| o.id == gid) {
            p = (p & !o.deny) | o.allow;
        }
        let (mut allow, mut deny) = (0, 0);
        for o in c.overwrites.iter().filter(|o| o.kind == 0 && my_roles.contains(&o.id)) {
            allow |= o.allow;
            deny |= o.deny;
        }
        p = (p & !deny) | allow;
        if let Some(o) = c.overwrites.iter().find(|o| o.kind == 1 && o.id == me) {
            p = (p & !o.deny) | o.allow;
        }
        p
    }

    pub fn can_view(&self, c: &ChannelState) -> bool {
        !c.forbidden && self.permissions(c.id) & perm::VIEW_CHANNEL != 0
    }

    /// Channel tree for a guild: (category, children) in display order.
    pub fn guild_tree(&self, gid: Id) -> Vec<(Option<Id>, Vec<Id>)> {
        let Some(g) = self.guilds.get(&gid) else { return Vec::new() };
        let mut cats: Vec<&ChannelState> = Vec::new();
        let mut children: HashMap<Option<Id>, Vec<&ChannelState>> = HashMap::new();
        for c in g.channels.iter().filter_map(|id| self.channels.get(id)) {
            match c.kind {
                ck::CATEGORY => cats.push(c),
                ck::ANNOUNCEMENT_THREAD | ck::PUBLIC_THREAD | ck::PRIVATE_THREAD => {}
                _ if self.can_view(c) => children.entry(c.parent_id).or_default().push(c),
                _ => {}
            }
        }
        let key = |c: &&ChannelState| (matches!(c.kind, ck::VOICE | ck::STAGE), c.position, c.id);
        cats.sort_by_key(|c| (c.position, c.id));
        let mut out = Vec::new();
        let mut take = |k: Option<Id>| {
            let mut v = children.remove(&k).unwrap_or_default();
            v.sort_by_key(key);
            v.into_iter().map(|c| c.id).collect::<Vec<_>>()
        };
        let top = take(None);
        if !top.is_empty() {
            out.push((None, top));
        }
        for cat in cats {
            let kids = take(Some(cat.id));
            if !kids.is_empty() {
                out.push((Some(cat.id), kids));
            }
        }
        // Orphans whose parent category is missing.
        let mut rest: Vec<Id> = children.into_values().flatten().map(|c| c.id).collect();
        rest.sort();
        if !rest.is_empty() {
            out.push((None, rest));
        }
        out
    }

    pub fn dm_list(&self) -> Vec<Id> {
        let mut v: Vec<&ChannelState> = self
            .channels
            .values()
            .filter(|c| c.guild_id.is_none() && matches!(c.kind, ck::DM | ck::GROUP_DM))
            .collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.last_message_id.unwrap_or(c.id)));
        v.into_iter().map(|c| c.id).collect()
    }

    pub fn guild_unread(&self, gid: Id) -> (bool, u32) {
        let Some(g) = self.guilds.get(&gid) else { return (false, 0) };
        let mut unread = false;
        let mut mentions = 0;
        for c in g.channels.iter().filter_map(|id| self.channels.get(id)) {
            if !c.is_text() {
                continue;
            }
            mentions += c.mentions;
            if !unread && !g.muted && !c.muted && c.unread() && self.can_view(c) {
                unread = true;
            }
        }
        (unread, mentions)
    }

    pub fn dm_unread(&self) -> u32 {
        self.channels
            .values()
            .filter(|c| c.guild_id.is_none() && c.unread() && !c.muted)
            .map(|c| c.mentions.max(1))
            .sum()
    }

    fn convert_message(&mut self, m: Message) -> Msg {
        self.remember_user(&m.author);
        let me = self.my_id();
        let guild = m.guild_id.or_else(|| self.channels.get(&m.channel_id).and_then(|c| c.guild_id));
        if let (Some(g), Some(member)) = (guild, &m.member)
            && let Some(gs) = self.guilds.get_mut(&g)
        {
            match &member.nick {
                Some(n) => {
                    gs.nicks.insert(m.author.id, n.clone());
                }
                None => {
                    gs.nicks.remove(&m.author.id);
                }
            }
        }
        let my_roles = guild.and_then(|g| self.guilds.get(&g)).and_then(|g| g.my_roles.clone()).unwrap_or_default();
        let mentions_me = m.author.id != me
            && (m.mention_everyone
                || m.mentions.iter().any(|u| u.id == me)
                || m.mention_roles.iter().any(|r| my_roles.contains(r)));
        let mut mention_users = Vec::with_capacity(m.mentions.len());
        for u in &m.mentions {
            self.remember_user(u);
            mention_users.push((u.id, u.display_name().to_owned()));
        }
        let reply = m.referenced_message.map(|r| {
            let name = self.user_name(guild, r.author.id).unwrap_or(r.author.display_name()).to_owned();
            let mut snippet: String = r.content.lines().next().unwrap_or("").chars().take(100).collect();
            if snippet.is_empty() && !r.attachments.is_empty() {
                snippet = "[attachment]".into();
            }
            (name, snippet)
        });
        let author = self.user_name(guild, m.author.id).unwrap_or(m.author.display_name()).to_owned();
        let author_color = match (guild.and_then(|g| self.guilds.get(&g)), &m.member) {
            (Some(g), Some(member)) => member
                .roles
                .iter()
                .filter_map(|r| g.roles.get(r))
                .filter(|r| r.color != 0)
                .max_by_key(|r| (r.position, r.id))
                .map(|r| r.color)
                .unwrap_or(0),
            _ => 0,
        };
        Msg {
            id: m.id,
            author_id: m.author.id,
            author,
            author_bot: m.author.bot,
            author_color,
            content: m.content,
            edited: m.edited_timestamp.is_some(),
            attachments: m.attachments,
            embeds: m.embeds,
            reactions: m.reactions,
            reply,
            mentions_me,
            kind: m.kind,
            mention_users,
            rendered: None,
        }
    }

    /// Returns true if the message should notify (mention or DM, not focused).
    pub fn on_message(&mut self, m: Message, focused_channel: Option<Id>) -> bool {
        let cid = m.channel_id;
        let me = self.my_id();
        let from_me = m.author.id == me;
        let id = m.id;
        let author_id = m.author.id;
        let msg = self.convert_message(m);
        let notify_worthy =
            !from_me && (msg.mentions_me || self.channels.get(&cid).is_some_and(|c| c.guild_id.is_none()));
        let focused = focused_channel == Some(cid);
        if let Some(c) = self.channels.get_mut(&cid) {
            c.last_message_id = Some(id);
            if from_me || focused {
                c.read_up_to = Some(id);
            } else if notify_worthy {
                c.mentions += 1;
            }
            if c.guild_id.is_none() {
                self.layout_gen += 1;
            }
        }
        if let Some(h) = self.history.get_mut(&cid)
            && h.loaded_once
            && !h.msgs.iter().rev().take(20).any(|x| x.id == id)
        {
            h.msgs.push_back(msg);
            if h.msgs.len() > MAX_IN_VIEW {
                h.msgs.pop_front();
                h.reached_start = false;
            }
        }
        // Typing stops when the message lands.
        if let Some(t) = self.typing.get_mut(&cid) {
            t.retain(|(u, _)| *u != author_id);
        }
        notify_worthy && !focused
    }

    pub fn on_message_update(&mut self, u: MessageUpdate) {
        let Some(h) = self.history.get_mut(&u.channel_id) else { return };
        let Some(m) = h.msgs.iter_mut().rev().find(|m| m.id == u.id) else { return };
        if let Some(c) = u.content {
            m.content = c;
            m.rendered = None;
        }
        if u.edited_timestamp.is_some() {
            m.edited = true;
        }
        if let Some(a) = u.attachments {
            m.attachments = a;
        }
        if let Some(e) = u.embeds {
            m.embeds = e;
        }
    }

    pub fn on_message_delete(&mut self, d: MessageDelete) {
        let Some(h) = self.history.get_mut(&d.channel_id) else { return };
        let mut ids = d.ids;
        ids.extend(d.id);
        h.msgs.retain(|m| !ids.contains(&m.id));
    }

    pub fn on_reaction(&mut self, r: ReactionEvent, added: bool) {
        let me = self.my_id();
        let Some(h) = self.history.get_mut(&r.channel_id) else { return };
        let Some(m) = h.msgs.iter_mut().rev().find(|m| m.id == r.message_id) else { return };
        let mine = r.user_id == me;
        match m.reactions.iter_mut().position(|x| x.emoji.same(&r.emoji)) {
            Some(i) => {
                let x = &mut m.reactions[i];
                if added {
                    x.count += 1;
                    x.me |= mine;
                } else {
                    x.count = x.count.saturating_sub(1);
                    if mine {
                        x.me = false;
                    }
                    if x.count == 0 {
                        m.reactions.remove(i);
                    }
                }
            }
            None if added => m.reactions.push(Reaction { count: 1, me: mine, emoji: r.emoji }),
            None => {}
        }
    }

    pub fn on_typing(&mut self, t: TypingStart) {
        if t.user_id == self.my_id() {
            return;
        }
        if let Some(m) = &t.member {
            if let Some(u) = &m.user {
                self.remember_user(u);
            }
            if let (Some(n), Some(c)) = (&m.nick, self.channels.get(&t.channel_id))
                && let Some(g) = c.guild_id.and_then(|g| self.guilds.get_mut(&g))
            {
                g.nicks.insert(t.user_id, n.clone());
            }
        }
        let v = self.typing.entry(t.channel_id).or_default();
        v.retain(|(u, _)| *u != t.user_id);
        v.push((t.user_id, Instant::now()));
    }

    /// Returns names currently typing; prunes expired entries.
    pub fn typing_in(&mut self, cid: Id) -> Vec<String> {
        let Some(v) = self.typing.get_mut(&cid) else { return Vec::new() };
        v.retain(|(_, t)| t.elapsed().as_secs() < 10);
        let ids: Vec<Id> = v.iter().map(|(u, _)| *u).collect();
        if ids.is_empty() {
            self.typing.remove(&cid);
            return Vec::new();
        }
        let guild = self.channels.get(&cid).and_then(|c| c.guild_id);
        ids.into_iter().map(|u| self.user_name(guild, u).unwrap_or("Someone").to_owned()).collect()
    }

    pub fn on_history(&mut self, cid: Id, before: Option<Id>, mut msgs: Vec<Message>) {
        let full_page = msgs.len() >= 50;
        msgs.sort_by_key(|m| m.id);
        let converted: Vec<Msg> = msgs.into_iter().map(|m| self.convert_message(m)).collect();
        let h = self.history.entry(cid).or_default();
        h.loading = false;
        h.loaded_once = true;
        if !full_page {
            h.reached_start = true;
        }
        match before {
            None => {
                // Merge: keep anything newer that arrived via the gateway meanwhile.
                let newest = converted.last().map(|m| m.id).unwrap_or(0);
                let newer: Vec<Msg> = h.msgs.drain(..).filter(|m| m.id > newest).collect();
                h.msgs = converted.into_iter().chain(newer).collect();
            }
            Some(_) => {
                for m in converted.into_iter().rev() {
                    if h.msgs.front().is_none_or(|f| f.id > m.id) {
                        h.msgs.push_front(m);
                    }
                }
            }
        }
    }

    /// Called when a channel is opened; returns true if history must be fetched.
    pub fn open_channel(&mut self, cid: Id) -> bool {
        let h = self.history.entry(cid).or_default();
        h.last_used = Some(Instant::now());
        let need = !h.loaded_once && !h.loading;
        if need {
            h.loading = true;
        }
        self.evict(cid);
        need
    }

    /// Trim history of a channel that left the screen, and evict the least recently used.
    pub fn leave_channel(&mut self, cid: Id) {
        if let Some(h) = self.history.get_mut(&cid)
            && h.msgs.len() > IDLE_KEEP
        {
            let drop_n = h.msgs.len() - IDLE_KEEP;
            h.msgs.drain(..drop_n);
            h.msgs.shrink_to_fit();
            h.reached_start = false;
        }
    }

    fn evict(&mut self, keep: Id) {
        while self.history.len() > MAX_CACHED_CHANNELS {
            let oldest = self
                .history
                .iter()
                .filter(|(id, h)| **id != keep && !h.loading)
                .min_by_key(|(_, h)| h.last_used)
                .map(|(id, _)| *id);
            match oldest {
                Some(id) => {
                    self.history.remove(&id);
                }
                None => break,
            }
        }
    }

    /// Marks a channel read up to its newest message. Returns the id to ack if it changed.
    pub fn mark_read(&mut self, cid: Id) -> Option<Id> {
        let c = self.channels.get_mut(&cid)?;
        c.mentions = 0;
        let last = c.last_message_id?;
        if c.read_up_to.is_some_and(|r| r >= last) {
            return None;
        }
        c.read_up_to = Some(last);
        Some(last)
    }

    pub fn on_ack(&mut self, a: MessageAck) {
        if let Some(c) = self.channels.get_mut(&a.channel_id) {
            c.read_up_to = Some(a.message_id);
            if c.last_message_id.is_none_or(|l| l <= a.message_id) {
                c.mentions = 0;
            }
        }
    }

    pub fn on_role(&mut self, e: GuildRoleEvent, deleted: bool) {
        let Some(g) = self.guilds.get_mut(&e.guild_id) else { return };
        if deleted {
            if let Some(id) = e.role_id {
                g.roles.remove(&id);
            }
        } else if let Some(r) = e.role {
            g.roles.insert(r.id, r);
        }
        self.layout_gen += 1;
    }

    pub fn on_member_update(&mut self, m: GuildMemberUpdate) {
        let me = self.my_id();
        self.remember_user(&m.user);
        let Some(g) = self.guilds.get_mut(&m.guild_id) else { return };
        match m.nick {
            Some(n) => {
                g.nicks.insert(m.user.id, n);
            }
            None => {
                g.nicks.remove(&m.user.id);
            }
        }
        if m.user.id == me {
            g.my_roles = Some(m.roles);
            self.layout_gen += 1;
        }
    }
}

pub fn timestamp_label(id: Id) -> String {
    use chrono::{Local, TimeZone};
    let ms = model::snowflake_ms(id) as i64;
    let Some(t) = Local.timestamp_millis_opt(ms).single() else { return String::new() };
    let now = Local::now();
    if t.date_naive() == now.date_naive() {
        t.format("Today %H:%M").to_string()
    } else if (now.date_naive() - t.date_naive()).num_days() == 1 {
        t.format("Yesterday %H:%M").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_guild() -> Store {
        let mut s = Store::default();
        let ready: Ready = serde_json::from_str(
            r#"{"session_id":"s","user":{"id":"1","username":"me"},
            "guilds":[{"id":"100","name":"G","owner_id":"999",
              "roles":[{"id":"100","permissions":"3072"},{"id":"200","permissions":"0"}],
              "channels":[
                {"id":"10","type":4,"name":"cat","position":0},
                {"id":"11","type":0,"name":"public","position":1,"parent_id":"10","last_message_id":"500"},
                {"id":"12","type":0,"name":"secret","position":2,"parent_id":"10",
                  "permission_overwrites":[{"id":"100","type":0,"allow":"0","deny":"1024"},{"id":"200","type":0,"allow":"1024","deny":"0"}]},
                {"id":"13","type":0,"name":"hidden","position":3,
                  "permission_overwrites":[{"id":"100","type":0,"allow":"0","deny":"1024"}]}
              ]}],
            "merged_members":[[{"user_id":"1","roles":["200"]}]],
            "read_state":[{"id":"11","last_message_id":"400","mention_count":2}]}"#,
        )
        .unwrap();
        s.on_ready(ready);
        s
    }

    #[test]
    fn permission_overwrites() {
        let s = store_with_guild();
        let tree = s.guild_tree(100);
        assert_eq!(tree, vec![(Some(10), vec![11, 12])]);
        assert!(s.permissions(11) & perm::SEND_MESSAGES != 0);
        assert!(s.channels[&11].unread());
        assert_eq!(s.guild_unread(100), (true, 2));
    }

    #[test]
    fn history_merge_and_trim() {
        let mut s = store_with_guild();
        assert!(s.open_channel(11));
        let mk = |id: u64| -> Message {
            serde_json::from_str(&format!(
                r#"{{"id":"{id}","channel_id":"11","author":{{"id":"5","username":"x"}},"content":"m{id}"}}"#
            ))
            .unwrap()
        };
        s.on_history(11, None, (1..=50).map(|i| mk(i << 22)).collect());
        assert_eq!(s.history[&11].msgs.len(), 50);
        s.on_message(mk(51 << 22), Some(11));
        assert_eq!(s.history[&11].msgs.back().unwrap().id, 51 << 22);
        assert_eq!(s.mark_read(11), None); // focused message already marked read
        s.leave_channel(11);
        assert_eq!(s.history[&11].msgs.len(), IDLE_KEEP);
    }
}
