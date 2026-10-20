//! Reproductor local por MPRIS2, en el bus de sesión.
//!
//! `Position` no emite `PropertiesChanged`: se lee al cambiar de pista, al
//! pausar o al recibir `Seeked`, y como mucho cada pocos segundos para
//! corregir la deriva. Entre medias la posición la avanza el reloj local.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use zbus::MatchRule;
use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::fdo::{NameOwnerChanged, PropertiesChanged};
use zbus::message::Type;
use zbus::names::BusName;
use zbus::zvariant::OwnedValue;

use crate::player::{PlayerBackend, Poll};
use crate::spotify::{Now, Track};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
/// Tope entre lecturas de `Position`. La spec no avisa de cada avance.
const RESYNC: Duration = Duration::from_secs(5);

const DESKTOP: &[&str] = &[
    "spotify",
    "mpv",
    "vlc",
    "strawberry",
    "rhythmbox",
    "audacious",
    "cmus",
];
const BROWSERS: &[&str] = &["firefox", "chromium", "chrome", "brave", "vivaldi", "opera"];
const YOUTUBE_SUFFIXES: &[&str] = &[
    " - YouTube Music",
    " – YouTube Music",
    " — YouTube Music",
    " - YouTube",
    " – YouTube",
    " — YouTube",
];
const SEPS: &[&str] = &[" - ", " – ", " — "];
const VERSION_MARKERS: &[&str] = &[
    "remix",
    "acoustic",
    "acústico",
    "instrumental",
    "cover",
    "live",
    "en vivo",
];
const CHANNEL_SUFFIXES: &[&str] = &["vevo", "- topic", "oficial", "official"];
const FEAT_MARKERS: &[&str] = &[" feat. ", " ft. ", " feat ", " ft "];

pub struct MprisBackend {
    conn: Option<Connection>,
    inbox: Option<Receiver<zbus::message::Message>>,
    players: HashMap<String, PlayerSnap>,
    /// Nombre único del bus (`:1.42`) hacia el well-known de MPRIS.
    owners: HashMap<String, String>,
    active: Option<String>,
    /// Última señal de cada reproductor. Sin señal, manda el de escritorio.
    activity: HashMap<String, Instant>,
    scanned: bool,
}

struct PlayerSnap {
    status: Playback,
    now: Now,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Playback {
    Playing,
    Paused,
    Stopped,
}

struct Candidate {
    name: String,
    status: Playback,
    seen: Option<Instant>,
}

impl MprisBackend {
    pub fn new() -> Self {
        Self {
            conn: None,
            inbox: None,
            players: HashMap::new(),
            owners: HashMap::new(),
            active: None,
            activity: HashMap::new(),
            scanned: false,
        }
    }

    fn poll_inner(&mut self, timeout: Duration) -> Result<Poll, String> {
        self.ensure()?;
        if !self.scanned {
            self.rescan()?;
            self.scanned = true;
            let pending = self.take_batch(Duration::ZERO)?;
            self.handle_all(pending)?;
            return Ok(self.snapshot());
        }
        let batch = self.take_batch(timeout.min(RESYNC))?;
        if batch.is_empty() {
            if self.active_playing() && self.refresh_position().is_err() {
                self.rescan()?;
            }
        } else {
            self.handle_all(batch)?;
        }
        Ok(self.snapshot())
    }

    fn ensure(&mut self) -> Result<(), String> {
        if self.conn.is_some() && self.inbox.is_some() {
            return Ok(());
        }
        let conn = Connection::session().map_err(|_| "No hay sesión D-Bus".to_string())?;
        install_matches(&conn)?;
        let inbox = spawn_watcher(&conn)?;
        self.conn = Some(conn);
        self.inbox = Some(inbox);
        Ok(())
    }

    fn drop_bus(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.close();
        }
        self.inbox = None;
        self.players.clear();
        self.owners.clear();
        self.active = None;
        self.activity.clear();
        self.scanned = false;
    }

    fn take_batch(&mut self, timeout: Duration) -> Result<Vec<zbus::message::Message>, String> {
        let Some(rx) = self.inbox.as_ref() else {
            return Err("No hay sesión D-Bus".into());
        };
        let first = match rx.recv_timeout(timeout) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => return Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("No hay sesión D-Bus".into());
            }
        };
        let mut batch = vec![first];
        while let Ok(msg) = self
            .inbox
            .as_ref()
            .expect("la bandeja sigue abierta")
            .try_recv()
        {
            batch.push(msg);
        }
        Ok(batch)
    }

    fn handle_all(&mut self, batch: Vec<zbus::message::Message>) -> Result<(), String> {
        for msg in batch {
            self.handle(msg)?;
        }
        Ok(())
    }

    fn handle(&mut self, msg: zbus::message::Message) -> Result<(), String> {
        let header = msg.header();
        let iface = header.interface().map(|name| name.as_str().to_string());
        let member = header.member().map(|name| name.as_str().to_string());
        let sender = header.sender().map(|name| name.as_str().to_string());
        match (iface.as_deref(), member.as_deref()) {
            (Some("org.freedesktop.DBus"), Some("NameOwnerChanged")) => self.on_name(msg),
            (Some("org.freedesktop.DBus.Properties"), Some("PropertiesChanged")) => {
                self.on_properties(msg, sender.as_deref())
            }
            (Some(PLAYER_IFACE), Some("Seeked")) => match sender.as_deref() {
                Some(sender) => self.on_seeked(&msg, sender),
                None => Ok(()),
            },
            _ => Ok(()),
        }
    }

    fn on_name(&mut self, msg: zbus::message::Message) -> Result<(), String> {
        let Some(changed) = NameOwnerChanged::from_message(msg) else {
            return Ok(());
        };
        let Ok(args) = changed.args() else {
            return Ok(());
        };
        if args.name().as_str().starts_with(PREFIX) {
            self.rescan()?;
        }
        Ok(())
    }

    fn on_properties(
        &mut self,
        msg: zbus::message::Message,
        sender: Option<&str>,
    ) -> Result<(), String> {
        let Some(sender) = sender else {
            return Ok(());
        };
        let Some(changed) = PropertiesChanged::from_message(msg) else {
            return Ok(());
        };
        let Ok(args) = changed.args() else {
            return Ok(());
        };
        if args.interface_name().as_str() != PLAYER_IFACE {
            return Ok(());
        }
        let changed_hit = args
            .changed_properties()
            .keys()
            .any(|key| relevant_property(key));
        let invalidated_hit = args
            .invalidated_properties()
            .iter()
            .any(|key| relevant_property(key));
        if !changed_hit && !invalidated_hit {
            return Ok(());
        }
        self.refresh_unique(sender)
    }

    fn on_seeked(&mut self, msg: &zbus::message::Message, unique: &str) -> Result<(), String> {
        let Some(name) = self.owners.get(unique).cloned() else {
            self.rescan()?;
            return Ok(());
        };
        self.activity.insert(name.clone(), Instant::now());
        let position = msg.body().deserialize::<i64>().unwrap_or(0);
        if let Some(player) = self.players.get_mut(&name) {
            player.now.progress_ms = us_to_ms(position);
        }
        self.rechoose();
        Ok(())
    }

    fn refresh_unique(&mut self, unique: &str) -> Result<(), String> {
        let Some(name) = self.owners.get(unique).cloned() else {
            self.rescan()?;
            return Ok(());
        };
        self.activity.insert(name.clone(), Instant::now());
        let Some(conn) = self.conn.clone() else {
            return Err("No hay sesión D-Bus".into());
        };
        match read_player(&conn, &name) {
            Some(snap) => {
                self.players.insert(name, snap);
                self.rechoose();
                Ok(())
            }
            None => self.rescan(),
        }
    }

    fn refresh_position(&mut self) -> Result<(), String> {
        let Some(name) = self.active.clone() else {
            return Ok(());
        };
        let Some(conn) = self.conn.clone() else {
            return Err("No hay sesión D-Bus".into());
        };
        let proxy = Proxy::new(&conn, name.as_str(), MPRIS_PATH, PLAYER_IFACE).map_err(dbus_err)?;
        let position = proxy.get_property::<i64>("Position").map_err(dbus_err)?;
        if let Some(player) = self.players.get_mut(&name) {
            player.now.progress_ms = us_to_ms(position);
        }
        Ok(())
    }

    fn rescan(&mut self) -> Result<(), String> {
        let conn = self.conn.clone().ok_or("No hay sesión D-Bus")?;
        let dbus = DBusProxy::new(&conn).map_err(dbus_err)?;
        let names = dbus.list_names().map_err(dbus_err)?;
        let mut players = HashMap::new();
        let mut owners = HashMap::new();
        for name in &names {
            let name = name.as_str();
            if !selectable(name) {
                continue;
            }
            let Ok(bus_name) = BusName::try_from(name) else {
                continue;
            };
            let Ok(owner) = dbus.get_name_owner(bus_name) else {
                continue;
            };
            owners.insert(owner.as_str().to_string(), name.to_string());
            if let Some(snap) = read_player(&conn, name) {
                players.insert(name.to_string(), snap);
            }
        }
        self.players = players;
        self.owners = owners;
        self.activity
            .retain(|name, _| self.players.contains_key(name));
        self.rechoose();
        Ok(())
    }

    fn rechoose(&mut self) {
        let candidates: Vec<Candidate> = self
            .players
            .iter()
            .map(|(name, snap)| Candidate {
                name: name.clone(),
                status: snap.status,
                seen: self.activity.get(name).copied(),
            })
            .collect();
        self.active = choose(&candidates, self.active.as_deref()).map(|picked| picked.name.clone());
    }

    fn active_playing(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|name| self.players.get(name))
            .is_some_and(|player| player.status == Playback::Playing)
    }

    fn snapshot(&self) -> Poll {
        let Some(name) = &self.active else {
            return idle();
        };
        let Some(player) = self.players.get(name) else {
            return idle();
        };
        if player.status == Playback::Stopped {
            return idle();
        }
        Poll {
            now: Some(clone_now(&player.now)),
            message: None,
        }
    }
}

impl PlayerBackend for MprisBackend {
    fn startup_message(&self) -> &'static str {
        "Buscando reproductor"
    }

    fn poll(&mut self, timeout: Duration) -> Result<Poll, String> {
        match self.poll_inner(timeout) {
            Ok(poll) => Ok(poll),
            Err(err) => {
                self.drop_bus();
                Err(err)
            }
        }
    }
}

fn idle() -> Poll {
    Poll {
        now: None,
        message: Some("Nada sonando".into()),
    }
}

fn clone_now(now: &Now) -> Now {
    Now {
        track: Arc::clone(&now.track),
        progress_ms: now.progress_ms,
        playing: now.playing,
        rate: now.rate,
    }
}

fn install_matches(conn: &Connection) -> Result<(), String> {
    let dbus = DBusProxy::new(conn).map_err(dbus_err)?;
    let players = MatchRule::builder()
        .msg_type(Type::Signal)
        .path(MPRIS_PATH)
        .map_err(dbus_err)?
        .build();
    dbus.add_match_rule(players).map_err(dbus_err)?;
    let owners = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(dbus_err)?
        .interface("org.freedesktop.DBus")
        .map_err(dbus_err)?
        .member("NameOwnerChanged")
        .map_err(dbus_err)?
        .build();
    dbus.add_match_rule(owners).map_err(dbus_err)?;
    Ok(())
}

fn spawn_watcher(conn: &Connection) -> Result<Receiver<zbus::message::Message>, String> {
    let conn = conn.clone();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("mpris".into())
        .spawn(move || {
            for msg in MessageIterator::from(&conn) {
                let Ok(msg) = msg else { break };
                if msg.message_type() != Type::Signal {
                    continue;
                }
                if tx.send(msg).is_err() {
                    break;
                }
            }
        })
        .map_err(dbus_err)?;
    Ok(rx)
}

fn read_player(conn: &Connection, bus_name: &str) -> Option<PlayerSnap> {
    let player = Proxy::new(conn, bus_name, MPRIS_PATH, PLAYER_IFACE).ok()?;
    let status = player.get_property::<String>("PlaybackStatus").ok()?;
    let status = match status.as_str() {
        "Playing" => Playback::Playing,
        "Paused" => Playback::Paused,
        _ => Playback::Stopped,
    };
    let metadata = player
        .get_property::<HashMap<String, OwnedValue>>("Metadata")
        .unwrap_or_default();
    let position = player.get_property::<i64>("Position").unwrap_or(0);
    let rate = sane_rate(player.get_property::<f64>("Rate").unwrap_or(1.0));
    let identity = Proxy::new(conn, bus_name, MPRIS_PATH, ROOT_IFACE)
        .ok()
        .and_then(|root| root.get_property::<String>("Identity").ok())
        .unwrap_or_default();
    let raw_title = metadata
        .get("xesam:title")
        .and_then(value_string)
        .unwrap_or_default();
    let artists = metadata
        .get("xesam:artist")
        .map(value_strings)
        .unwrap_or_default();
    let url = metadata.get("xesam:url").and_then(value_string);
    let duration_ms = metadata
        .get("mpris:length")
        .and_then(value_i64)
        .map(us_to_ms)
        .unwrap_or(0);
    let source = source_label(&identity, bus_name, url.as_deref(), &raw_title);
    let (title, artist, main_artist) = clean_track(bus_name, &raw_title, &artists);
    Some(PlayerSnap {
        status,
        now: Now {
            track: Arc::new(Track {
                id: cache_id(url.as_deref(), &artist, &title, duration_ms),
                title,
                artist,
                main_artist,
                duration_ms,
                source,
            }),
            progress_ms: us_to_ms(position),
            playing: status == Playback::Playing,
            rate,
        },
    })
}

fn dbus_err(err: impl std::fmt::Display) -> String {
    format!("D-Bus: {err}")
}

fn sane_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        1.0
    }
}

fn us_to_ms(us: i64) -> u64 {
    if us <= 0 { 0 } else { (us as u64) / 1000 }
}

fn relevant_property(name: &str) -> bool {
    matches!(name, "PlaybackStatus" | "Metadata" | "Rate")
}

fn value_string(value: &OwnedValue) -> Option<String> {
    String::try_from(value.try_clone().ok()?).ok()
}

fn value_i64(value: &OwnedValue) -> Option<i64> {
    let owned = value.try_clone().ok()?;
    if let Ok(number) = i64::try_from(owned) {
        return Some(number);
    }
    u64::try_from(value.try_clone().ok()?)
        .ok()
        .map(|number| number as i64)
}

fn value_strings(value: &OwnedValue) -> Vec<String> {
    let Some(owned) = value.try_clone().ok() else {
        return Vec::new();
    };
    if let Ok(list) = Vec::<String>::try_from(owned) {
        return nonempty(list);
    }
    nonempty(value_string(value).into_iter().collect())
}

fn nonempty(list: Vec<String>) -> Vec<String> {
    list.into_iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn player_key(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(PREFIX).unwrap_or(name);
    let head = rest.split(['.', '-']).next().unwrap_or("");
    (!head.is_empty()).then_some(head)
}

fn ignored_player(name: &str) -> bool {
    let Some(key) = player_key(name) else {
        return true;
    };
    matches!(
        key.to_ascii_lowercase().as_str(),
        "playerctld" | "kdeconnect" | "gsconnect"
    )
}

fn selectable(name: &str) -> bool {
    name.starts_with(PREFIX) && !ignored_player(name)
}

fn is_desktop(name: &str) -> bool {
    player_key(name).is_some_and(|key| DESKTOP.iter().any(|desk| key.eq_ignore_ascii_case(desk)))
}

fn is_browser(name: &str) -> bool {
    player_key(name).is_some_and(|key| {
        BROWSERS
            .iter()
            .any(|browser| key.eq_ignore_ascii_case(browser))
    })
}

fn choose<'a>(candidates: &'a [Candidate], previous: Option<&str>) -> Option<&'a Candidate> {
    let playing: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.status == Playback::Playing)
        .collect();
    if !playing.is_empty() {
        return playing
            .into_iter()
            .max_by(|left, right| prefer_playing(left, right));
    }
    previous.and_then(|prev| {
        candidates
            .iter()
            .find(|candidate| candidate.name == prev && candidate.status == Playback::Paused)
    })
}

fn prefer_playing(left: &Candidate, right: &Candidate) -> Ordering {
    match (left.seen, right.seen) {
        (Some(earlier), Some(later)) if earlier != later => earlier.cmp(&later),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        _ => (is_desktop(&left.name) as u8)
            .cmp(&(is_desktop(&right.name) as u8))
            .then_with(|| right.name.cmp(&left.name)),
    }
}

/// Quita el ruido de YouTube y, si cabe, separa artista y título.
/// El título va primero; el artista es el segundo valor.
#[cfg(test)]
fn sanitize_browser_title(title: &str) -> (String, Option<String>) {
    let (title, artist) = browser_names(title, "");
    let artist = (!artist.is_empty()).then_some(artist);
    (title, artist)
}

fn clean_track(bus_name: &str, raw_title: &str, artists: &[String]) -> (String, String, String) {
    let (joined, main) = join_artists(artists);
    if should_sanitize(bus_name, raw_title) {
        let (title, artist) = browser_names(raw_title, &main);
        return (title, artist.clone(), artist);
    }
    (raw_title.trim().to_string(), joined, main)
}

/// En un navegador el título manda si trae un separador con espacios.
/// Sin ese separador se limpia el artista del canal.
fn browser_names(raw_title: &str, channel: &str) -> (String, String) {
    let text = strip_leading_brackets(strip_youtube_suffix(raw_title));
    let (title, artist) = if let Some((left, right)) = split_first(&text) {
        let left = left.trim();
        let right = right.trim();
        if !left.is_empty() && !right.is_empty() {
            (right.to_string(), left.to_string())
        } else {
            (text, clean_channel_artist(channel))
        }
    } else {
        (text, clean_channel_artist(channel))
    };
    (finish_name(title), finish_name(artist))
}

fn finish_name(text: String) -> String {
    cut_feat(&unwrap_quotes(&strip_trailing_blocks(text)))
}

fn should_sanitize(bus_name: &str, title: &str) -> bool {
    is_browser(bus_name) || has_youtube_suffix(title)
}

fn join_artists(artists: &[String]) -> (String, String) {
    let names: Vec<&str> = artists
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    let main = names.first().copied().unwrap_or("").to_string();
    (names.join(", "), main)
}

fn strip_youtube_suffix(title: &str) -> String {
    let trimmed = title.trim();
    for suffix in YOUTUBE_SUFFIXES {
        if let Some(stripped) = strip_suffix_ci(trimmed, suffix) {
            return stripped.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn has_youtube_suffix(title: &str) -> bool {
    let trimmed = title.trim();
    YOUTUBE_SUFFIXES
        .iter()
        .any(|suffix| strip_suffix_ci(trimmed, suffix).is_some())
}

fn strip_suffix_ci<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    if text.len() < suffix.len() {
        return None;
    }
    let start = text.len() - suffix.len();
    if !text.is_char_boundary(start) {
        return None;
    }
    text[start..]
        .eq_ignore_ascii_case(suffix)
        .then_some(&text[..start])
}

fn strip_leading_brackets(mut text: String) -> String {
    loop {
        let start = text.len() - text.trim_start().len();
        text.replace_range(..start, "");
        if !text.starts_with('[') {
            break;
        }
        let Some(end) = balanced_close(&text, '[', ']') else {
            break;
        };
        text.replace_range(..=end, "");
    }
    text.trim().to_string()
}

fn balanced_close(text: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0i32;
    for (index, ch) in text.char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// Quita el bloque final `(...)` o `[...]`, incluidos los anidados,
/// salvo que el texto sea una versión.
fn strip_trailing_blocks(mut text: String) -> String {
    loop {
        let end = text.trim_end().len();
        text.truncate(end);
        let Some(start) = trailing_group_start(&text) else {
            break;
        };
        let close = text.chars().next_back().unwrap();
        let inner_start = start + '('.len_utf8();
        let inner_end = text.len() - close.len_utf8();
        if inner_start > inner_end || is_version(&text[inner_start..inner_end]) {
            break;
        }
        text.truncate(start);
    }
    text.trim().to_string()
}

fn trailing_group_start(text: &str) -> Option<usize> {
    let close = text.chars().next_back()?;
    let open = match close {
        ')' => '(',
        ']' => '[',
        _ => return None,
    };
    let mut depth = 0i32;
    for (index, ch) in text.char_indices().rev() {
        if ch == close {
            depth += 1;
        } else if ch == open {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn is_version(inner: &str) -> bool {
    let lower = inner.to_lowercase();
    VERSION_MARKERS
        .iter()
        .any(|marker| contains_word(&lower, marker))
}

fn contains_word(lower: &str, marker: &str) -> bool {
    let mut rest = lower;
    while let Some(at) = rest.find(marker) {
        let after = at + marker.len();
        let before_ok = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphabetic());
        let after_ok = rest[after..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_alphabetic());
        if before_ok && after_ok {
            return true;
        }
        let next = rest[at..].chars().next().map_or(1, char::len_utf8);
        rest = &rest[at + next..];
    }
    false
}

fn unwrap_quotes(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let Some(last) = text.chars().next_back() else {
        return String::new();
    };
    if text.chars().count() >= 2 && is_quote(first) && is_quote(last) {
        let start = first.len_utf8();
        let end = text.len() - last.len_utf8();
        return text[start..end].trim().to_string();
    }
    text.to_string()
}

fn is_quote(ch: char) -> bool {
    matches!(ch, '"' | '\'' | '\u{201c}' | '\u{201d}' | '«' | '»')
}

fn cut_feat(text: &str) -> String {
    let lower = text.to_lowercase();
    let cut = FEAT_MARKERS
        .iter()
        .filter_map(|marker| lower.find(marker))
        .min();
    match cut {
        Some(at) => text[..at].trim().to_string(),
        None => text.trim().to_string(),
    }
}

fn clean_channel_artist(name: &str) -> String {
    let mut text = name.trim().to_string();
    while let Some(stripped) = CHANNEL_SUFFIXES.iter().find_map(|suffix| {
        let rest = strip_suffix_ci(&text, suffix)?.trim();
        (!rest.is_empty()).then(|| rest.to_string())
    }) {
        text = stripped;
    }
    space_camel(&text)
}

fn space_camel(text: &str) -> String {
    if text.contains(' ') {
        return text.to_string();
    }
    let mut out = String::new();
    let mut prev_lower = false;
    for ch in text.chars() {
        if prev_lower && ch.is_uppercase() {
            out.push(' ');
        }
        prev_lower = ch.is_lowercase();
        out.push(ch);
    }
    out
}

fn split_first(text: &str) -> Option<(&str, &str)> {
    let mut best: Option<(usize, usize)> = None;
    for sep in SEPS {
        if let Some(index) = text.find(sep)
            && best.map(|(at, _)| index < at).unwrap_or(true)
        {
            best = Some((index, sep.len()));
        }
    }
    best.map(|(index, len)| (&text[..index], &text[index + len..]))
}

fn source_label(identity: &str, bus_name: &str, url: Option<&str>, raw_title: &str) -> String {
    let app = app_name(identity, bus_name);
    if url.is_some_and(is_youtube_url) || has_youtube_suffix(raw_title) {
        format!("YouTube | {app}")
    } else {
        app
    }
}

fn app_name(identity: &str, bus_name: &str) -> String {
    let id = identity.trim();
    if id.eq_ignore_ascii_case("Google Chrome") {
        return "Chrome".into();
    }
    if !id.is_empty() {
        return id.to_string();
    }
    match player_key(bus_name) {
        Some(key) if key.eq_ignore_ascii_case("chrome") => "Chrome".into(),
        Some(key) => key.to_string(),
        None => "MPRIS".into(),
    }
}

fn is_youtube_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim_start_matches("www.");
    host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com")
}

fn cache_id(url: Option<&str>, artist: &str, title: &str, duration_ms: u64) -> String {
    if let Some(url) = url
        && let Some(id) = youtube_video_id(url)
    {
        return format!("yt:{id}");
    }
    format!("mpris:{artist}|{title}|{duration_ms}")
}

fn youtube_video_id(url: &str) -> Option<String> {
    if !is_youtube_url(url) {
        return None;
    }
    if let Some(id) = query_value(url, "v")
        && !id.is_empty()
    {
        return Some(id);
    }
    let path = url.split_once("://")?.1;
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let path = path.split_once('/')?.1;
    for prefix in ["embed/", "shorts/", "live/", "v/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let id = rest.split('/').next().unwrap_or("");
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    if url_host(url) == Some("youtu.be") {
        let id = path.split('/').next().unwrap_or("");
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

fn url_host(url: &str) -> Option<&str> {
    let host = url.split_once("://")?.1;
    let host = host.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    Some(host.trim_start_matches("www."))
}

fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next().unwrap_or("");
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_titles_drop_youtube_noise_and_split_artist() {
        let cases = [
            (
                "Artista - Canción (Official Video) - YouTube",
                "Canción",
                Some("Artista"),
            ),
            (
                "Artista - Canción [Official Audio]",
                "Canción",
                Some("Artista"),
            ),
            ("Artista - Canción (Visualizer)", "Canción", Some("Artista")),
            ("Canción (Official Audio) - YouTube", "Canción", None),
        ];
        for (raw, title, artist) in cases {
            assert_eq!(
                sanitize_browser_title(raw),
                (title.into(), artist.map(str::to_string)),
                "{raw}"
            );
        }
        assert_eq!(
            sanitize_browser_title("Artist - Song - Remaster"),
            ("Song - Remaster".into(), Some("Artist".into()))
        );
    }

    #[test]
    fn desktop_title_is_not_split() {
        let (title, artist, main) = clean_track(
            "org.mpris.MediaPlayer2.spotify",
            "Artist - Song (Live)",
            &[],
        );
        assert_eq!(title, "Artist - Song (Live)");
        assert_eq!(artist, "");
        assert_eq!(main, "");
    }

    #[test]
    fn browser_title_split_ignores_the_channel() {
        let (title, artist, main) = clean_track(
            "org.mpris.MediaPlayer2.firefox.instance1",
            "Artista - Canción (Official Video) - YouTube",
            &["Canal".into()],
        );
        assert_eq!(title, "Canción");
        assert_eq!(artist, "Artista");
        assert_eq!(main, "Artista");
    }

    #[test]
    fn browser_titles_keep_versions_and_drop_the_rest() {
        let firefox = "org.mpris.MediaPlayer2.firefox.instance1";
        let cases = [
            (
                "MecanoVEVO",
                "Mecano - Hijo de la Luna (Videoclip oficial)",
                "Mecano",
                "Hijo de la Luna",
            ),
            (
                "JeanetteOficialVEVO",
                "Jeanette - Frente a Frente (Aplauso (Actuación TVE))",
                "Jeanette",
                "Frente a Frente",
            ),
            ("Canal", "Song (Live)", "Canal", "Song (Live)"),
            ("Canal", "Song (En vivo)", "Canal", "Song (En vivo)"),
            (
                "Canal",
                "Song (Acoustic Remix)",
                "Canal",
                "Song (Acoustic Remix)",
            ),
            (
                "Canal",
                "Blink-182 - All the Small Things",
                "Blink-182",
                "All the Small Things",
            ),
            ("Canal", "Jay-Z - 99 Problems", "Jay-Z", "99 Problems"),
            ("Canal", "Spider-Man", "Canal", "Spider-Man"),
            ("Canal", "[MV] Artista - \"Canción\"", "Artista", "Canción"),
            (
                "Daft Punk",
                "Daft Punk - Get Lucky ft. Pharrell",
                "Daft Punk",
                "Get Lucky",
            ),
            (
                "SodaStereoVEVO",
                "De Música Ligera",
                "Soda Stereo",
                "De Música Ligera",
            ),
            (
                "MecanoVEVO",
                "Hijo de la Luna (odifical)",
                "Mecano",
                "Hijo de la Luna",
            ),
        ];
        for (channel, raw, artist, title) in cases {
            let (got_title, got_artist, got_main) = clean_track(firefox, raw, &[channel.into()]);
            assert_eq!(got_title, title, "{raw}");
            assert_eq!(got_artist, artist, "{raw}");
            assert_eq!(got_main, artist, "{raw}");
        }
    }

    #[test]
    fn youtube_source_uses_the_app_name() {
        assert_eq!(
            source_label(
                "Google Chrome",
                "org.mpris.MediaPlayer2.chrome.instance1",
                Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
                "Anything",
            ),
            "YouTube | Chrome"
        );
        assert_eq!(
            cache_id(
                Some("https://youtu.be/dQw4w9WgXcQ"),
                "Artista",
                "Canción",
                1_000,
            ),
            "yt:dQw4w9WgXcQ"
        );
        assert_eq!(
            source_label(
                "Firefox",
                "org.mpris.MediaPlayer2.firefox.instance1",
                Some("https://example.com/a"),
                "Hello",
            ),
            "Firefox"
        );
    }

    fn at(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    fn candidate(name: &str, status: Playback, seen: Option<u64>) -> Candidate {
        Candidate {
            name: name.into(),
            status,
            seen: seen.map(at),
        }
    }

    #[test]
    fn playing_players_prefer_the_latest_signal_then_desktop() {
        let spotify = candidate("org.mpris.MediaPlayer2.spotify", Playback::Playing, Some(1));
        let mpv = candidate("org.mpris.MediaPlayer2.mpv", Playback::Playing, Some(5));
        let playing = [spotify, mpv];
        let picked = choose(&playing, None).unwrap();
        assert_eq!(picked.name, "org.mpris.MediaPlayer2.mpv");

        let browser = candidate(
            "org.mpris.MediaPlayer2.chromium.instance1",
            Playback::Playing,
            None,
        );
        let desk = candidate("org.mpris.MediaPlayer2.spotify", Playback::Playing, None);
        let tied = [browser, desk];
        let picked = choose(&tied, None).unwrap();
        assert_eq!(picked.name, "org.mpris.MediaPlayer2.spotify");
    }

    #[test]
    fn pause_keeps_the_active_player() {
        let spotify = candidate("org.mpris.MediaPlayer2.spotify", Playback::Paused, None);
        let browser = candidate(
            "org.mpris.MediaPlayer2.firefox.instance1",
            Playback::Paused,
            None,
        );
        let paused = [spotify, browser];
        let picked = choose(&paused, Some("org.mpris.MediaPlayer2.spotify")).unwrap();
        assert_eq!(picked.name, "org.mpris.MediaPlayer2.spotify");

        let stopped = [candidate(
            "org.mpris.MediaPlayer2.spotify",
            Playback::Stopped,
            None,
        )];
        assert!(choose(&stopped, Some("org.mpris.MediaPlayer2.spotify")).is_none());
    }

    #[test]
    fn phone_bridges_are_ignored() {
        assert!(ignored_player("org.mpris.MediaPlayer2.kdeconnect.phone"));
        assert!(ignored_player("org.mpris.MediaPlayer2.gsconnect"));
        assert!(ignored_player("org.mpris.MediaPlayer2.playerctld"));
        assert!(!ignored_player("org.mpris.MediaPlayer2.spotify"));
        assert!(!selectable("org.mpris.MediaPlayer2.kdeconnect.phone"));
    }
}
