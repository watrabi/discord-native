//! The egui front-end. Runs in reactive mode: a frame is only produced on
//! input, on a network event, or when a timer (typing indicator / toast
//! expiry) explicitly asks for one.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, CursorIcon, Frame, Key, Layout, Margin, Modifiers, OpenUrl, Rect, RichText, ScrollArea,
    Sense, Stroke, TextEdit, Ui, ViewportCommand, vec2,
};

use crate::config;
use crate::markdown::{self, Ref, Theme};
use crate::model::{Id, channel_kind as ck};
use crate::net::{self, Command, Credentials, NetEvent, NetHandle};
use crate::store::{History, Store, perm, timestamp_label};

const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤", "😂", "😮", "😢", "🎉"];

enum Phase {
    Login { input: String, bot: bool, remember: bool, error: Option<String> },
    Running,
}

pub struct App {
    phase: Phase,
    net: Option<NetHandle>,
    creds: Option<Credentials>,
    store: Store,
    status: String,
    selected_guild: Option<Id>,
    selected_channel: Option<Id>,
    last_channel: HashMap<Option<Id>, Id>,
    collapsed: HashSet<Id>,
    composer: String,
    reply_to: Option<(Id, String)>,
    editing: Option<Id>,
    attach_path: Option<String>,
    pending_files: Vec<PathBuf>,
    last_typing_sent: Option<Instant>,
    toasts: Vec<(String, Instant)>,
    scroll_to_bottom: bool,
    scroll_adjust: Option<f32>,
    prev_content_h: f32,
    pending_offset: Option<f32>,
    /// True once the message view has reached the bottom after opening a channel;
    /// auto-loading older history waits for this so it can't fire mid-jump.
    bottom_settled: bool,
    last_pinned_h: f32,
    focus_composer: bool,
    theme: Option<Theme>,
    title: String,
    was_focused: bool,
    /// Per-message cached layout height: (message id) -> (width, height, with_header)
    heights: HashMap<Id, (f32, f32, bool)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        ctx.set_visuals(egui::Visuals::dark());
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = vec2(6.0, 3.0);
            s.interaction.tooltip_delay = 0.3;
            // A blinking caret forces a full repaint twice a second, forever.
            s.visuals.text_cursor.blink = false;
        });
        let mut app = App {
            phase: Phase::Login { input: String::new(), bot: false, remember: true, error: None },
            net: None,
            creds: None,
            store: Store::default(),
            status: String::new(),
            selected_guild: None,
            selected_channel: None,
            last_channel: HashMap::new(),
            collapsed: HashSet::new(),
            composer: String::new(),
            reply_to: None,
            editing: None,
            attach_path: None,
            pending_files: Vec::new(),
            last_typing_sent: None,
            toasts: Vec::new(),
            scroll_to_bottom: false,
            scroll_adjust: None,
            prev_content_h: 0.0,
            pending_offset: None,
            bottom_settled: false,
            last_pinned_h: 0.0,
            focus_composer: false,
            theme: None,
            title: String::new(),
            was_focused: true,
            heights: HashMap::new(),
        };
        if let Some(c) = config::load() {
            app.start(c, ctx.clone());
        }
        app
    }

    fn start(&mut self, creds: Credentials, ctx: egui::Context) {
        self.store = Store::default();
        self.net = Some(net::spawn(creds.clone(), ctx));
        self.creds = Some(creds);
        self.phase = Phase::Running;
        self.status = "Connecting…".into();
    }

    fn logout(&mut self) {
        config::forget();
        // Dropping the handle closes the command channel, which ends the net
        // thread's runtime and with it the gateway connection.
        self.net = None;
        self.creds = None;
        self.store = Store::default();
        self.selected_channel = None;
        self.selected_guild = None;
        self.heights.clear();
        self.phase = Phase::Login { input: String::new(), bot: false, remember: true, error: None };
    }

    fn cmd(&self, c: Command) {
        if let Some(n) = &self.net {
            n.send(c);
        }
    }

    fn toast(&mut self, s: String) {
        self.toasts.push((s, Instant::now()));
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    // ------------------------------------------------------------ events --

    fn drain_events(&mut self, ctx: &egui::Context) {
        let Some(net) = &self.net else { return };
        let events: Vec<NetEvent> = net.rx.try_iter().collect();
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let focus_ch = if focused { self.selected_channel } else { None };
        for ev in events {
            match ev {
                NetEvent::Status(s) => self.status = s,
                NetEvent::AuthFailed(e) => {
                    let msg = e;
                    self.net = None;
                    self.creds = None;
                    self.phase = Phase::Login { input: String::new(), bot: false, remember: true, error: Some(msg) };
                    return;
                }
                NetEvent::Ready(r) => {
                    self.store.on_ready(*r);
                    self.heights.clear();
                    // Re-open the current channel (READY wipes caches).
                    if let Some(c) = self.selected_channel {
                        if self.store.channels.contains_key(&c) {
                            self.open(c);
                        } else {
                            self.selected_channel = None;
                        }
                    }
                }
                NetEvent::GuildCreate(g) => self.store.add_guild(*g, None),
                NetEvent::GuildUpdate(g) => self.store.update_guild(*g),
                NetEvent::GuildDelete(g) => {
                    if !g.unavailable {
                        self.store.remove_guild(g.id);
                        if self.selected_guild == Some(g.id) {
                            self.selected_guild = None;
                            self.selected_channel = None;
                        }
                    }
                }
                NetEvent::ChannelUpsert(c) => self.store.upsert_channel(*c),
                NetEvent::ChannelDelete(c) => {
                    self.store.remove_channel(c.id);
                    if self.selected_channel == Some(c.id) {
                        self.selected_channel = None;
                    }
                }
                NetEvent::Role(e, del) => self.store.on_role(e, del),
                NetEvent::MemberUpdate(m) => self.store.on_member_update(m),
                NetEvent::MessageCreate(m) => {
                    let cid = m.channel_id;
                    let mid = m.id;
                    let from_me = m.author.id == self.store.my_id();
                    self.store.on_message(*m, focus_ch);
                    if focus_ch == Some(cid) && !from_me {
                        self.cmd(Command::Ack { channel_id: cid, message_id: mid });
                    }
                }
                NetEvent::MessageUpdate(u) => {
                    self.heights.remove(&u.id);
                    self.store.on_message_update(*u);
                }
                NetEvent::MessageDelete(d) => self.store.on_message_delete(d),
                NetEvent::Reaction(r, add) => {
                    self.heights.remove(&r.message_id);
                    self.store.on_reaction(r, add);
                }
                NetEvent::Typing(t) => self.store.on_typing(t),
                NetEvent::Ack(a) => self.store.on_ack(a),
                NetEvent::History { channel_id, before, messages } => {
                    if before.is_some() && Some(channel_id) == self.selected_channel {
                        self.scroll_adjust = Some(self.prev_content_h);
                    }
                    self.store.on_history(channel_id, before, messages);
                    if before.is_none() && Some(channel_id) == self.selected_channel {
                        self.scroll_to_bottom = true;
                        self.bottom_settled = false;
                        if focused {
                            self.mark_read(channel_id);
                        }
                    }
                }
                NetEvent::HistoryFailed { channel_id, status } => {
                    if let Some(h) = self.store.history.get_mut(&channel_id) {
                        h.loading = false;
                        h.loaded_once = true;
                        h.reached_start = true;
                    }
                    if status == 403
                        && let Some(c) = self.store.channels.get_mut(&channel_id)
                    {
                        c.forbidden = true;
                        self.toast("You don't have access to that channel.".into());
                    }
                }
                NetEvent::Error(e) => self.toast(e),
            }
        }
        // Coming back to the window marks the open channel read.
        if focused
            && !self.was_focused
            && let Some(c) = self.selected_channel
        {
            self.mark_read(c);
        }
        self.was_focused = focused;
    }

    fn mark_read(&mut self, cid: Id) {
        if let Some(mid) = self.store.mark_read(cid) {
            self.cmd(Command::Ack { channel_id: cid, message_id: mid });
        }
    }

    fn open(&mut self, cid: Id) {
        if let Some(prev) = self.selected_channel
            && prev != cid
        {
            self.store.leave_channel(prev);
        }
        self.selected_channel = Some(cid);
        let guild = self.store.channels.get(&cid).and_then(|c| c.guild_id);
        self.last_channel.insert(guild, cid);
        if self.store.open_channel(cid) {
            self.cmd(Command::FetchHistory { channel_id: cid, before: None });
        } else {
            self.mark_read(cid);
        }
        self.scroll_to_bottom = true;
        self.bottom_settled = false;
        self.reply_to = None;
        self.editing = None;
        self.focus_composer = true;
    }

    fn select_guild(&mut self, g: Option<Id>) {
        self.selected_guild = g;
        let target =
            self.last_channel.get(&g).copied().filter(|c| self.store.channels.contains_key(c)).or_else(|| match g {
                Some(gid) => self.store.guild_tree(gid).into_iter().flat_map(|(_, v)| v).find(|c| {
                    self.store.channels.get(c).is_some_and(|c| matches!(c.kind, ck::TEXT | ck::ANNOUNCEMENT))
                }),
                None => None,
            });
        match target {
            Some(c) => self.open(c),
            None => {
                if let Some(prev) = self.selected_channel.take() {
                    self.store.leave_channel(prev);
                }
            }
        }
    }

    fn theme(&mut self, ui: &Ui) -> Theme {
        if self.theme.is_none() {
            let v = ui.visuals();
            self.theme = Some(Theme {
                size: 14.0,
                text: v.text_color(),
                strong: v.strong_text_color(),
                weak: v.weak_text_color(),
                link: v.hyperlink_color,
                code_bg: v.code_bg_color,
                mention_fg: Color32::from_rgb(201, 205, 251),
                mention_bg: Color32::from_rgba_unmultiplied(88, 101, 242, 60),
                spoiler: Color32::from_gray(20),
            });
        }
        self.theme.clone().unwrap()
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let mut count = self.store.dm_unread();
        for g in &self.store.guild_order {
            count += self.store.guild_unread(*g).1;
        }
        let title = if count > 0 { format!("({count}) discord-native") } else { "discord-native".to_string() };
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    // ------------------------------------------------------------- login --

    fn login_ui(&mut self, ui: &mut Ui) {
        let mut submit: Option<Credentials> = None;
        let mut remember_choice = false;
        egui::CentralPanel::default().show(ui, |ui| {
            let Phase::Login { input, bot, remember, error } = &mut self.phase else { return };
            ui.add_space(ui.available_height() * 0.25);
            ui.vertical_centered(|ui| {
                ui.heading("discord-native");
                ui.label(RichText::new("A lightweight native Discord client").weak());
                ui.add_space(16.0);
                let resp = ui.add(TextEdit::singleline(input).password(true).hint_text("Token").desired_width(320.0));
                ui.checkbox(bot, "This is a bot token");
                ui.checkbox(remember, "Remember token (stored in your config dir, 0600)");
                if !*bot {
                    ui.label(
                        RichText::new(
                            "Note: using a user-account token with third-party clients is against\n\
                             Discord's Terms of Service and can get the account flagged.",
                        )
                        .small()
                        .color(Color32::from_rgb(250, 168, 26)),
                    );
                }
                ui.add_space(8.0);
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (ui.button("Log in").clicked() || enter) && !input.trim().is_empty() {
                    submit = Some(config::parse_token(input, *bot));
                    remember_choice = *remember;
                }
                if let Some(e) = error {
                    ui.add_space(8.0);
                    ui.colored_label(Color32::from_rgb(237, 66, 69), e.as_str());
                }
            });
        });
        if let Some(c) = submit {
            if remember_choice && let Err(e) = config::save(&c) {
                self.toast(format!("Could not save token: {e}"));
            }
            self.start(c, ui.ctx().clone());
        }
    }

    // ------------------------------------------------------------ panels --

    fn guild_strip(&mut self, ui: &mut Ui) {
        let mut clicked: Option<Option<Id>> = None;
        ScrollArea::vertical()
            .id_salt("guild_strip")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.add_space(6.0);
                let dm_badge = self.store.dm_unread();
                if guild_button(ui, "DM", "Direct Messages", self.selected_guild.is_none(), dm_badge > 0, dm_badge)
                    .clicked()
                {
                    clicked = Some(None);
                }
                ui.separator();
                for &gid in &self.store.guild_order {
                    let Some(g) = self.store.guilds.get(&gid) else { continue };
                    let (unread, mentions) = self.store.guild_unread(gid);
                    if guild_button(ui, &abbrev(&g.name), &g.name, self.selected_guild == Some(gid), unread, mentions)
                        .clicked()
                    {
                        clicked = Some(Some(gid));
                    }
                }
            });
        if let Some(g) = clicked {
            self.select_guild(g);
        }
    }

    fn channel_list(&mut self, ui: &mut Ui) {
        let title = match self.selected_guild {
            Some(g) => self.store.guilds.get(&g).map(|g| g.name.clone()).unwrap_or_default(),
            None => "Direct Messages".into(),
        };
        ui.add_space(6.0);
        ui.label(RichText::new(title).strong().size(15.0));
        ui.separator();

        let mut open: Option<Id> = None;
        let mut toggle: Option<Id> = None;
        egui::Panel::bottom("me").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let me = self.store.me.as_ref().map(|u| u.display_name().to_owned()).unwrap_or_else(|| "…".into());
                ui.label(RichText::new(me).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Log out").on_hover_text("Forget token and log out").clicked() {
                        self.logout();
                    }
                });
            });
            ui.label(RichText::new(&self.status).small().weak());
            ui.add_space(2.0);
        });
        if self.net.is_none() {
            return;
        }
        ScrollArea::vertical().id_salt("channels").auto_shrink(false).show(ui, |ui| match self.selected_guild {
            None => {
                for cid in self.store.dm_list() {
                    let Some(c) = self.store.channels.get(&cid) else { continue };
                    let name = self.store.channel_display_name(c);
                    let icon = if c.kind == ck::GROUP_DM { "👥" } else { "@" };
                    if channel_row(
                        ui,
                        icon,
                        &name,
                        self.selected_channel == Some(cid),
                        c.unread() && !c.muted,
                        c.mentions,
                        false,
                    )
                    .clicked()
                    {
                        open = Some(cid);
                    }
                }
            }
            Some(gid) => {
                for (cat, kids) in self.store.guild_tree(gid) {
                    let collapsed = cat.is_some_and(|c| self.collapsed.contains(&c));
                    if let Some(cat_id) = cat {
                        let name = self.store.channels.get(&cat_id).map(|c| c.name.to_uppercase()).unwrap_or_default();
                        let arrow = if collapsed { "⏵" } else { "⏷" };
                        ui.add_space(6.0);
                        let r = ui.add(
                            egui::Label::new(RichText::new(format!("{arrow} {name}")).small().weak())
                                .sense(Sense::click())
                                .truncate(),
                        );
                        if r.clicked() {
                            toggle = Some(cat_id);
                        }
                    }
                    for cid in kids {
                        let Some(c) = self.store.channels.get(&cid) else { continue };
                        let selected = self.selected_channel == Some(cid);
                        let unread = c.unread() && !c.muted;
                        if collapsed && !selected && !unread && c.mentions == 0 {
                            continue;
                        }
                        let (icon, usable) = match c.kind {
                            ck::VOICE | ck::STAGE => ("🔊", true),
                            ck::ANNOUNCEMENT => ("📢", true),
                            ck::FORUM | ck::MEDIA => ("💬", false),
                            _ => ("#", true),
                        };
                        let r = channel_row(ui, icon, &c.name, selected, unread, c.mentions, c.muted || !usable);
                        if r.clicked() && usable {
                            open = Some(cid);
                        }
                        if !usable {
                            r.on_hover_text("Forum channels are not supported yet");
                        }
                    }
                }
            }
        });
        if let Some(c) = toggle
            && !self.collapsed.remove(&c)
        {
            self.collapsed.insert(c);
        }
        if let Some(c) = open {
            self.open(c);
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        let Some(c) = self.selected_channel.and_then(|c| self.store.channels.get(&c)) else { return };
        ui.horizontal(|ui| {
            let prefix = if c.guild_id.is_some() { "#" } else { "@" };
            ui.label(RichText::new(format!("{prefix} {}", self.store.channel_display_name(c))).strong().size(15.0));
            if let Some(t) = &c.topic {
                ui.separator();
                let first = t.lines().next().unwrap_or("");
                ui.add(egui::Label::new(RichText::new(first).weak()).truncate()).on_hover_text(t.as_str());
            }
        });
    }

    fn composer(&mut self, ui: &mut Ui) {
        let Some(cid) = self.selected_channel else { return };
        let perms = self.store.permissions(cid);
        let can_send = perms & perm::SEND_MESSAGES != 0;
        let can_attach = perms & perm::ATTACH_FILES != 0;
        let names = self.store.typing_in(cid);
        ui.add_space(2.0);

        // Typing indicator (always reserves a line so the layout doesn't jump).
        let typing = match names.len() {
            0 => String::new(),
            1 => format!("{} is typing…", names[0]),
            2 => format!("{} and {} are typing…", names[0], names[1]),
            3 => format!("{}, {} and {} are typing…", names[0], names[1], names[2]),
            _ => "Several people are typing…".into(),
        };
        ui.label(RichText::new(typing).small().weak());
        if !names.is_empty() {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }

        if let Some((_, who)) = &self.reply_to {
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Replying to {who}")).small());
                cancel = ui.small_button("✖").clicked();
            });
            if cancel {
                self.reply_to = None;
            }
        }
        if self.editing.is_some() {
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Editing message — Enter to save, Esc to cancel").small());
                cancel = ui.small_button("✖").clicked();
            });
            if cancel {
                self.editing = None;
                self.composer.clear();
            }
        }
        if !self.pending_files.is_empty() {
            let mut remove = None;
            ui.horizontal_wrapped(|ui| {
                for (i, f) in self.pending_files.iter().enumerate() {
                    let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if ui.small_button(format!("📎 {name} ✖")).clicked() {
                        remove = Some(i);
                    }
                }
            });
            if let Some(i) = remove {
                self.pending_files.remove(i);
            }
        }
        if let Some(path) = &mut self.attach_path {
            let mut done = None;
            ui.horizontal(|ui| {
                let r = ui.add(TextEdit::singleline(path).hint_text("Path to file").desired_width(300.0));
                r.request_focus();
                if ui.button("Attach").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = Some(false);
                }
            });
            match done {
                Some(true) => {
                    let p = PathBuf::from(path.trim());
                    if p.is_file() {
                        self.pending_files.push(p);
                    } else {
                        self.toast(format!("Not a file: {}", p.display()));
                    }
                    self.attach_path = None;
                }
                Some(false) => self.attach_path = None,
                None => {}
            }
        }

        let composer_id = egui::Id::unique("composer");
        let has_focus = ui.memory(|m| m.has_focus(composer_id));
        let mut send = false;
        if has_focus {
            send = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                if self.editing.take().is_some() {
                    self.composer.clear();
                }
                self.reply_to = None;
            }
            // Up on an empty composer edits your last message.
            if self.composer.is_empty()
                && self.editing.is_none()
                && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp))
            {
                let me = self.store.my_id();
                if let Some(m) =
                    self.store.history.get(&cid).and_then(|h| h.msgs.iter().rev().find(|m| m.author_id == me))
                {
                    self.editing = Some(m.id);
                    self.composer = m.content.clone();
                }
            }
        }
        let hint = if can_send {
            format!(
                "Message {}",
                self.store.channels.get(&cid).map(|c| self.store.channel_display_name(c)).unwrap_or_default()
            )
        } else {
            "You do not have permission to send messages in this channel".into()
        };
        let before = self.composer.len();
        ui.horizontal(|ui| {
            if can_send
                && can_attach
                && ui.button("➕").on_hover_text("Attach file (or drop files on the window)").clicked()
            {
                self.attach_path = Some(String::new());
            }
            let r = ui.add_enabled(
                can_send,
                TextEdit::multiline(&mut self.composer)
                    .id(composer_id)
                    .hint_text(hint)
                    .desired_rows(1)
                    .desired_width(f32::INFINITY)
                    .lock_focus(true),
            );
            if self.focus_composer && can_send {
                r.request_focus();
                self.focus_composer = false;
            }
        });
        ui.add_space(6.0);

        if can_send && self.composer.len() != before && !self.composer.is_empty() && self.editing.is_none() {
            let now = Instant::now();
            if self.last_typing_sent.is_none_or(|t| now - t > Duration::from_secs(8)) {
                self.last_typing_sent = Some(now);
                self.cmd(Command::Typing { channel_id: cid });
            }
        }

        if send && can_send {
            let text = self.composer.trim().to_owned();
            if let Some(mid) = self.editing.take() {
                if !text.is_empty() {
                    self.cmd(Command::Edit { channel_id: cid, message_id: mid, content: text });
                }
                self.composer.clear();
            } else if !text.is_empty() || !self.pending_files.is_empty() {
                if text.chars().count() > 2000 {
                    self.toast("Message is longer than 2000 characters.".into());
                    return;
                }
                let reply_to = self.reply_to.take().map(|r| r.0);
                let files = std::mem::take(&mut self.pending_files);
                self.cmd(Command::Send { channel_id: cid, content: text, reply_to, files });
                self.composer.clear();
                self.last_typing_sent = None;
                self.scroll_to_bottom = true;
            }
        }
    }

    fn messages(&mut self, ui: &mut Ui) {
        let Some(cid) = self.selected_channel else {
            ui.centered_and_justified(|ui| {
                let msg = if self.store.me.is_none() { "Connecting…" } else { "Select a channel" };
                ui.label(RichText::new(msg).weak());
            });
            return;
        };
        // Take the history out so rendering can borrow the store freely.
        let Some(mut hist) = self.store.history.remove(&cid) else {
            ui.spinner();
            return;
        };
        let theme = self.theme(ui);
        let mut actions = Vec::new();
        let scroll_bottom = self.scroll_to_bottom && hist.loaded_once;
        let mut area = ScrollArea::vertical();
        if let Some(y) = self.pending_offset.take() {
            area = area.vertical_scroll_offset(y);
        }
        let out = area.id_salt(("messages", cid)).stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !hist.reached_start {
                if hist.loading {
                    ui.vertical_centered(|ui| ui.spinner());
                } else if hist.loaded_once {
                    let r = ui.vertical_centered(|ui| ui.button("Load older messages")).inner;
                    let visible = ui.is_rect_visible(r.rect)
                        && self.scroll_adjust.is_none()
                        && self.bottom_settled
                        && !scroll_bottom;
                    if r.clicked() || visible {
                        actions.push(Action::LoadOlder);
                    }
                }
            } else if hist.loaded_once {
                ui.label(RichText::new("This is the beginning of the channel.").weak());
            }
            if !hist.loaded_once && hist.loading {
                ui.vertical_centered(|ui| ui.spinner());
            }
            self.message_rows(ui, cid, &mut hist, &theme, &mut actions);
        });
        if let Some(old) = self.scroll_adjust.take() {
            let delta = out.content_size.y - old;
            if delta > 0.0 {
                self.pending_offset = Some(out.state.clamped_offset().y + delta);
                ui.ctx().request_discard("keep scroll position after loading history");
            }
        }
        self.prev_content_h = out.content_size.y;
        let max_offset = (out.content_size.y - out.inner_rect.height()).max(0.0);
        let offset = out.state.clamped_offset().y;
        let at_bottom = offset >= max_offset - 2.0;
        if scroll_bottom {
            // Pin to the exact end offset: egui only engages stick-to-bottom
            // when offset == max exactly. Repeat until layout is stable
            // (a scrollbar appearing changes wrap width and content height).
            let stable = (out.content_size.y - self.last_pinned_h).abs() < 0.5;
            if offset == max_offset && stable {
                self.scroll_to_bottom = false;
            } else {
                self.pending_offset = Some(max_offset);
                ui.ctx().request_discard("pin message view to bottom");
            }
            self.last_pinned_h = out.content_size.y;
        } else if hist.loaded_once && at_bottom {
            self.bottom_settled = true;
        }
        self.store.history.insert(cid, hist);

        for a in actions {
            self.apply(cid, a, ui.ctx());
        }
    }

    fn message_rows(&mut self, ui: &mut Ui, cid: Id, hist: &mut History, theme: &Theme, actions: &mut Vec<Action>) {
        let me = self.store.my_id();
        let guild = self.store.channels.get(&cid).and_then(|c| c.guild_id);
        let can_manage = self.store.permissions(cid) & perm::MANAGE_MESSAGES != 0;
        let width = ui.available_width();
        let mut prev: Option<(Id, Id)> = None; // (author, message id)
        for m in hist.msgs.iter_mut() {
            let header = match prev {
                Some((a, pid)) => {
                    a != m.author_id
                        || m.reply.is_some()
                        || crate::model::snowflake_ms(m.id).saturating_sub(crate::model::snowflake_ms(pid))
                            > GROUP_WINDOW_MS
                }
                None => true,
            };
            prev = Some((m.author_id, m.id));
            let top_pad = if header { 8.0 } else { 0.0 };

            // Skip layout for off-screen rows whose height we already know.
            if let Some(&(w, h, hd)) = self.heights.get(&m.id)
                && (w - width).abs() < 0.5
                && hd == header
            {
                let rect = Rect::from_min_size(ui.cursor().min, vec2(width, h));
                if !ui.is_rect_visible(rect) {
                    ui.allocate_space(vec2(width, h));
                    continue;
                }
            }

            let start = ui.cursor().min.y;
            ui.add_space(top_pad);
            let highlight =
                m.mentions_me || self.editing == Some(m.id) || self.reply_to.as_ref().is_some_and(|r| r.0 == m.id);
            let frame = if highlight {
                Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(250, 168, 26, 18))
                    .stroke(Stroke::NONE)
                    .inner_margin(Margin { left: 4, right: 4, top: 1, bottom: 1 })
            } else {
                Frame::new().inner_margin(Margin { left: 4, right: 4, top: 0, bottom: 0 })
            };
            let mine = m.author_id == me;
            frame.show(ui, |ui| {
                ui.set_width(ui.available_width());
                if let Some((who, snippet)) = &m.reply {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.label(RichText::new("╭").monospace().weak());
                        ui.add(egui::Label::new(RichText::new(format!("{who}: {snippet}")).small().weak()).truncate());
                    });
                }
                let mut menu_targets: Vec<egui::Response> = Vec::new();
                if header {
                    let r = ui
                        .horizontal(|ui| {
                            let mut name = RichText::new(&m.author).strong();
                            if m.author_color != 0 {
                                let c = m.author_color;
                                name = name.color(Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8));
                            }
                            ui.label(name);
                            if m.author_bot {
                                ui.label(
                                    RichText::new("BOT")
                                        .small()
                                        .background_color(Color32::from_rgb(88, 101, 242))
                                        .color(Color32::WHITE),
                                );
                            }
                            ui.label(RichText::new(timestamp_label(m.id)).small().weak());
                        })
                        .response;
                    menu_targets.push(r.interact(Sense::click()));
                }
                if let Some(sys) = system_text(m.kind) {
                    ui.label(RichText::new(sys).italics().weak());
                } else if !m.content.is_empty() {
                    let rendered = m
                        .rendered
                        .get_or_insert_with(|| {
                            let store = &self.store;
                            let mentions = &m.mention_users;
                            Arc::new(markdown::render(&m.content, theme, &|r| match r {
                                Ref::User(id) => mentions
                                    .iter()
                                    .find(|(u, _)| *u == id)
                                    .map(|(_, n)| store.user_name(guild, id).unwrap_or(n).to_owned())
                                    .or_else(|| store.user_name(guild, id).map(str::to_owned)),
                                Ref::Channel(id) => store.channels.get(&id).map(|c| store.channel_display_name(c)),
                                Ref::Role(id) => guild
                                    .and_then(|g| store.guilds.get(&g))
                                    .and_then(|g| g.roles.get(&id))
                                    .map(|r| r.name.clone()),
                            }))
                        })
                        .clone();
                    let mut job = rendered.job.clone();
                    job.wrap.max_width = ui.available_width();
                    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
                    let (rect, resp) = ui.allocate_exact_size(galley.size(), Sense::click());
                    let mut hovered_link = None;
                    if let Some(pos) = resp.hover_pos() {
                        let cursor = galley.cursor_from_pos(pos - rect.min);
                        // cursor_from_pos snaps to the nearest char; only count real hits.
                        if galley.rect.translate(rect.min.to_vec2()).contains(pos) {
                            hovered_link = rendered.link_at(cursor.index.0).map(str::to_owned);
                        }
                    }
                    ui.painter().galley(rect.min, galley, theme.text);
                    if let Some(url) = hovered_link {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                        if resp.clicked() {
                            ui.ctx().open_url(OpenUrl::new_tab(url));
                        }
                    }
                    menu_targets.push(resp);
                }
                if m.edited {
                    ui.label(RichText::new("(edited)").small().weak());
                }
                for a in &m.attachments {
                    ui.hyperlink_to(format!("📎 {} ({})", a.filename, human_size(a.size)), &a.url);
                }
                for e in &m.embeds {
                    if e.title.is_none() && e.description.is_none() {
                        continue;
                    }
                    Frame::new()
                        .stroke(Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color))
                        .inner_margin(Margin::same(6))
                        .show(ui, |ui| {
                            if let Some(t) = &e.title {
                                match &e.url {
                                    Some(u) => {
                                        ui.hyperlink_to(RichText::new(t).strong(), u);
                                    }
                                    None => {
                                        ui.label(RichText::new(t).strong());
                                    }
                                }
                            }
                            if let Some(d) = &e.description {
                                let d: String = d.chars().take(400).collect();
                                ui.label(RichText::new(d).small());
                            }
                        });
                }
                if !m.reactions.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for r in &m.reactions {
                            let label = format!("{} {}", r.emoji.display(), r.count);
                            if ui.selectable_label(r.me, label).clicked() {
                                actions.push(Action::React(m.id, r.emoji.route(), !r.me));
                            }
                        }
                    });
                }
                for t in menu_targets {
                    t.context_menu(|ui| {
                        ui.horizontal(|ui| {
                            for e in QUICK_REACTIONS {
                                if ui.button(e).clicked() {
                                    actions.push(Action::React(m.id, e.to_string(), true));
                                    ui.close();
                                }
                            }
                        });
                        ui.separator();
                        if ui.button("Reply").clicked() {
                            actions.push(Action::Reply(m.id, m.author.clone()));
                            ui.close();
                        }
                        if mine && ui.button("Edit").clicked() {
                            actions.push(Action::Edit(m.id, m.content.clone()));
                            ui.close();
                        }
                        if (mine || can_manage) && ui.button("Delete").clicked() {
                            actions.push(Action::Delete(m.id));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Copy text").clicked() {
                            ui.ctx().copy_text(m.content.clone());
                            ui.close();
                        }
                        if ui.button("Copy message link").clicked() {
                            let g = guild.map(|g| g.to_string()).unwrap_or_else(|| "@me".into());
                            ui.ctx().copy_text(format!("https://discord.com/channels/{g}/{cid}/{}", m.id));
                            ui.close();
                        }
                        if ui.button("Copy ID").clicked() {
                            ui.ctx().copy_text(m.id.to_string());
                            ui.close();
                        }
                    });
                }
            });
            let h = ui.cursor().min.y - start;
            self.heights.insert(m.id, (width, h, header));
        }
        // Keep the height cache bounded to what's loaded.
        if self.heights.len() > 4 * crate::store::MAX_IN_VIEW {
            let live: HashSet<Id> = self
                .store
                .history
                .values()
                .chain(std::iter::once(&*hist))
                .flat_map(|h| h.msgs.iter().map(|m| m.id))
                .collect();
            self.heights.retain(|id, _| live.contains(id));
        }
    }

    fn apply(&mut self, cid: Id, a: Action, _ctx: &egui::Context) {
        match a {
            Action::LoadOlder => {
                if let Some(h) = self.store.history.get_mut(&cid)
                    && !h.loading
                    && !h.reached_start
                {
                    h.loading = true;
                    let before = h.msgs.front().map(|m| m.id);
                    self.cmd(Command::FetchHistory { channel_id: cid, before });
                }
            }
            Action::Reply(id, who) => {
                self.reply_to = Some((id, who));
                self.editing = None;
                self.focus_composer = true;
            }
            Action::Edit(id, content) => {
                self.editing = Some(id);
                self.reply_to = None;
                self.composer = content;
                self.focus_composer = true;
            }
            Action::Delete(id) => self.cmd(Command::Delete { channel_id: cid, message_id: id }),
            Action::React(id, emoji, add) => self.cmd(Command::React { channel_id: cid, message_id: id, emoji, add }),
        }
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|(_, t)| t.elapsed() < Duration::from_secs(6));
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, vec2(-12.0, -80.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                for (t, _) in &self.toasts {
                    Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(360.0);
                        ui.label(t.as_str());
                    });
                }
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

enum Action {
    LoadOlder,
    Reply(Id, String),
    Edit(Id, String),
    Delete(Id),
    React(Id, String, bool),
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if std::env::var_os("DISCORD_NATIVE_DEBUG_REPAINT").is_some() {
            eprintln!("frame {}: {:?}", ctx.cumulative_pass_nr(), ctx.repaint_causes());
        }
        self.drain_events(&ctx);

        if matches!(self.phase, Phase::Login { .. }) {
            self.login_ui(ui);
            self.toasts(&ctx);
            return;
        }

        // Files dropped onto the window are queued as attachments.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() && self.selected_channel.is_some() {
            self.pending_files.extend(dropped);
        }

        egui::Panel::left("guilds").exact_size(64.0).resizable(false).show(ui, |ui| self.guild_strip(ui));
        egui::Panel::left("channel_list")
            .default_size(220.0)
            .size_range(150.0..=400.0)
            .resizable(true)
            .show(ui, |ui| self.channel_list(ui));
        if self.net.is_none() {
            return;
        }
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            self.header(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("composer").show(ui, |ui| self.composer(ui));
        egui::CentralPanel::default().show(ui, |ui| self.messages(ui));
        self.toasts(&ctx);
        self.update_title(&ctx);
    }
}

// --------------------------------------------------------------- widgets --

fn guild_button(
    ui: &mut Ui,
    label: &str,
    tooltip: &str,
    selected: bool,
    unread: bool,
    mentions: u32,
) -> egui::Response {
    let size = vec2(48.0, 44.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), size.y), Sense::click());
    let rect_btn = Rect::from_center_size(rect.center(), size);
    let v = ui.visuals();
    let fill = if selected {
        Color32::from_rgb(88, 101, 242)
    } else if resp.hovered() {
        v.widgets.hovered.bg_fill
    } else {
        v.widgets.inactive.bg_fill
    };
    let p = ui.painter();
    p.rect_filled(rect_btn, if selected { 12.0 } else { 18.0 }, fill);
    p.text(rect_btn.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(13.0), Color32::WHITE);
    if unread || selected {
        let h = if selected { 30.0 } else { 8.0 };
        p.rect_filled(
            Rect::from_center_size(egui::pos2(rect.left() + 2.0, rect.center().y), vec2(4.0, h)),
            2.0,
            Color32::WHITE,
        );
    }
    if mentions > 0 {
        let c = egui::pos2(rect_btn.right() - 4.0, rect_btn.bottom() - 4.0);
        p.circle_filled(c, 8.0, Color32::from_rgb(237, 66, 69));
        let t = if mentions > 99 { "99+".to_string() } else { mentions.to_string() };
        p.text(c, egui::Align2::CENTER_CENTER, t, egui::FontId::proportional(10.0), Color32::WHITE);
    }
    ui.add_space(2.0);
    resp.on_hover_text(tooltip)
}

fn channel_row(
    ui: &mut Ui,
    icon: &str,
    name: &str,
    selected: bool,
    unread: bool,
    mentions: u32,
    dim: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let v = ui.visuals();
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 4.0, v.widgets.active.weak_bg_fill);
    } else if resp.hovered() {
        p.rect_filled(rect, 4.0, v.widgets.hovered.weak_bg_fill);
    }
    let color = if selected || unread || mentions > 0 {
        v.strong_text_color()
    } else if dim {
        v.weak_text_color().gamma_multiply(0.7)
    } else {
        v.weak_text_color()
    };
    let badge_w = if mentions > 0 { 26.0 } else { 0.0 };
    let max_w = (rect.width() - 12.0 - badge_w).max(10.0);
    let mut job =
        egui::text::LayoutJob::simple_singleline(format!("{icon}  {name}"), egui::FontId::proportional(14.0), color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w);
    let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
    let text_pos = egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0);
    p.galley(text_pos, galley, color);
    if unread && !selected {
        p.rect_filled(
            Rect::from_center_size(egui::pos2(rect.left() + 1.0, rect.center().y), vec2(3.0, 8.0)),
            1.5,
            Color32::WHITE,
        );
    }
    if mentions > 0 {
        let c = egui::pos2(rect.right() - 14.0, rect.center().y);
        p.rect_filled(Rect::from_center_size(c, vec2(22.0, 16.0)), 8.0, Color32::from_rgb(237, 66, 69));
        let t = if mentions > 99 { "99+".to_string() } else { mentions.to_string() };
        p.text(c, egui::Align2::CENTER_CENTER, t, egui::FontId::proportional(11.0), Color32::WHITE);
    }
    resp
}

fn abbrev(name: &str) -> String {
    let s: String = name.split_whitespace().filter_map(|w| w.chars().find(|c| c.is_alphanumeric())).take(3).collect();
    if s.is_empty() { name.chars().take(2).collect() } else { s }
}

fn human_size(b: u64) -> String {
    match b {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

fn system_text(kind: u8) -> Option<&'static str> {
    Some(match kind {
        0 | 19 | 20 | 23 => return None,
        1 => "added someone to the group",
        2 => "removed someone from the group",
        3 => "started a call",
        4 => "changed the channel name",
        5 => "changed the channel icon",
        6 => "pinned a message to this channel",
        7 => "joined the server",
        8..=11 => "boosted the server",
        12 => "added a channel follower",
        18 => "started a thread",
        _ => "system message",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviations() {
        assert_eq!(abbrev("Rust Programming Language"), "RPL");
        assert_eq!(abbrev("my cool server here"), "mcs");
        assert_eq!(abbrev("🙂"), "🙂");
        assert_eq!(human_size(2048), "2 KB");
    }
}
