use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const PLAYING_URL: &str = "https://api.spotify.com/v1/me/player/currently-playing";
const PROFILE_URL: &str = "https://api.spotify.com/v1/me";
const TOP_TRACKS_URL: &str = "https://api.spotify.com/v1/me/top/tracks";
const SCOPES: &str =
    "user-read-currently-playing user-read-playback-state user-top-read user-read-private";
const USER_AGENT: &str = concat!(
    "lyrics/",
    env!("CARGO_PKG_VERSION"),
    " (terminal lyrics visualizer)"
);

pub fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .user_agent(USER_AGENT)
        .build()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("lyrics")
}

pub(crate) fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("lyrics")
}

fn auth_path() -> PathBuf {
    state_dir().join("spotify-auth.json")
}

fn sptlrx_auth_path() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("sptlrx/spotify-auth.json")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize)]
struct Auth {
    client_id: String,
    client_secret: String,
    #[serde(default)]
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_at: u64,
}

impl Auth {
    fn load() -> Result<Self, String> {
        let path = auth_path();
        if let Ok(text) = fs::read_to_string(&path) {
            return serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()));
        }

        #[derive(Deserialize)]
        struct Sptlrx {
            client_id: String,
            client_secret: String,
            refresh_token: String,
        }
        let text = fs::read_to_string(sptlrx_auth_path()).map_err(|_| {
            "Sin sesión de Spotify. Ejecuta: lyrics login <client_id> <client_secret>".to_string()
        })?;
        let old: Sptlrx =
            serde_json::from_str(&text).map_err(|e| format!("sesión de sptlrx: {e}"))?;
        let auth = Auth {
            client_id: old.client_id,
            client_secret: old.client_secret,
            access_token: String::new(),
            refresh_token: old.refresh_token,
            expires_at: 0,
        };
        auth.save()?;
        Ok(auth)
    }

    fn save(&self) -> Result<(), String> {
        let path = auth_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(&json))
            .and_then(|_| fs::rename(&tmp, &path))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    fn exchange(&mut self, agent: &ureq::Agent, form: &[(&str, &str)]) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            expires_in: u64,
            refresh_token: Option<String>,
        }
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.client_id, self.client_secret));
        let token: Token = agent
            .post(TOKEN_URL)
            .set("Authorization", &format!("Basic {basic}"))
            .send_form(form)
            .map_err(|e| format!("token de Spotify: {e}"))?
            .into_json()
            .map_err(|e| format!("token de Spotify: {e}"))?;
        self.access_token = token.access_token;
        self.expires_at = unix_now() + token.expires_in;
        if let Some(refresh) = token.refresh_token {
            self.refresh_token = refresh;
        }
        self.save()
    }
}

pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub main_artist: String,
    pub duration_ms: u64,
    /// App que está sonando, por ejemplo `Spotify` o `YouTube | Chrome`.
    pub source: String,
}

pub struct Now {
    pub track: Arc<Track>,
    pub progress_ms: u64,
    pub playing: bool,
    /// 1.0 en Spotify. En MPRIS es la propiedad `Rate`.
    pub rate: f64,
}

/// Periodo de las canciones más escuchadas.
#[derive(Clone, Copy)]
pub enum Period {
    Short,
    Medium,
    Long,
}

impl Period {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "corto" => Some(Self::Short),
            "mediano" => Some(Self::Medium),
            "largo" => Some(Self::Long),
            _ => None,
        }
    }

    fn query(self) -> &'static str {
        match self {
            Self::Short => "short_term",
            Self::Medium => "medium_term",
            Self::Long => "long_term",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Short => "Últimas 4 semanas",
            Self::Medium => "Últimos 6 meses",
            Self::Long => "Cerca de un año",
        }
    }
}

pub struct Stats {
    pub name: String,
    pub country: String,
    pub plan: String,
    pub tracks: Vec<RankedTrack>,
}

pub struct RankedTrack {
    pub title: String,
    pub artist: String,
}

#[derive(Deserialize)]
struct Playing {
    #[serde(default)]
    is_playing: bool,
    progress_ms: Option<u64>,
    item: Option<Item>,
}

#[derive(Deserialize)]
struct Item {
    id: Option<String>,
    name: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    artists: Vec<Named>,
    show: Option<Named>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

pub struct Spotify {
    auth: Auth,
    agent: ureq::Agent,
}

impl Spotify {
    pub fn new(agent: ureq::Agent) -> Result<Self, String> {
        Ok(Self {
            auth: Auth::load()?,
            agent,
        })
    }

    fn token(&mut self) -> Result<String, String> {
        if self.auth.access_token.is_empty() || self.auth.expires_at <= unix_now() + 30 {
            let refresh = self.auth.refresh_token.clone();
            self.auth.exchange(
                &self.agent,
                &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
            )?;
        }
        Ok(self.auth.access_token.clone())
    }

    pub fn now_playing(&mut self) -> Result<Option<Now>, String> {
        for attempt in 0..2 {
            let token = self.token()?;
            let response = self
                .agent
                .get(PLAYING_URL)
                .set("Authorization", &format!("Bearer {token}"))
                .call();
            match response {
                Ok(resp) if resp.status() == 204 => return Ok(None),
                Ok(resp) => {
                    let playing: Playing = resp.into_json().map_err(|e| format!("Spotify: {e}"))?;
                    return Ok(into_now(playing));
                }
                Err(ureq::Error::Status(401, _)) if attempt == 0 => self.auth.expires_at = 0,
                Err(ureq::Error::Status(429, _)) => {
                    return Err("Spotify limitó las peticiones, reintentando".into());
                }
                Err(e) => return Err(format!("Spotify: {e}")),
            }
        }
        Err("Spotify rechazó la sesión. Ejecuta: lyrics login".into())
    }

    pub fn stats(&mut self, period: Period) -> Result<Stats, String> {
        let me: Me = self.get_json(PROFILE_URL)?;
        let country = me
            .country
            .filter(|c| !c.is_empty())
            .ok_or_else(stats_scope)?;
        let product = me
            .product
            .filter(|p| !p.is_empty())
            .ok_or_else(stats_scope)?;
        let url = format!("{}?time_range={}&limit=10", TOP_TRACKS_URL, period.query());
        let top: TopTracks = self.get_json(&url)?;
        Ok(Stats {
            name: me.display_name.filter(|n| !n.is_empty()).unwrap_or(me.id),
            country,
            plan: plan_label(&product),
            tracks: top
                .items
                .into_iter()
                .map(|item| RankedTrack {
                    artist: item
                        .artists
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    title: item.name,
                })
                .collect(),
        })
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&mut self, url: &str) -> Result<T, String> {
        for attempt in 0..2 {
            let token = self.token()?;
            let response = self
                .agent
                .get(url)
                .set("Authorization", &format!("Bearer {token}"))
                .call();
            match response {
                Ok(resp) => {
                    return resp.into_json().map_err(|e| format!("Spotify: {e}"));
                }
                Err(ureq::Error::Status(401, _)) if attempt == 0 => self.auth.expires_at = 0,
                Err(ureq::Error::Status(403, _)) => return Err(stats_scope()),
                Err(ureq::Error::Status(429, _)) => {
                    return Err("Spotify limitó las peticiones, reintentando".into());
                }
                Err(e) => return Err(format!("Spotify: {e}")),
            }
        }
        Err("Spotify rechazó la sesión. Ejecuta: lyrics login".into())
    }
}

fn stats_scope() -> String {
    "Faltan permisos de estadísticas. Vuelve a ejecutar: lyrics login".into()
}

fn plan_label(product: &str) -> String {
    match product {
        "premium" => "Premium".into(),
        "free" | "open" => "Gratis".into(),
        other => other.to_string(),
    }
}

#[derive(Deserialize)]
struct Me {
    id: String,
    display_name: Option<String>,
    country: Option<String>,
    product: Option<String>,
}

#[derive(Deserialize)]
struct TopTracks {
    #[serde(default)]
    items: Vec<TopItem>,
}

#[derive(Deserialize)]
struct TopItem {
    name: String,
    #[serde(default)]
    artists: Vec<Named>,
}

fn into_now(playing: Playing) -> Option<Now> {
    let item = playing.item?;
    let names: Vec<&str> = item.artists.iter().map(|a| a.name.as_str()).collect();
    let artist = if names.is_empty() {
        item.show
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_default()
    } else {
        names.join(", ")
    };
    let main_artist = names
        .first()
        .map(|s| s.to_string())
        .unwrap_or_else(|| artist.clone());
    let id = item
        .id
        .unwrap_or_else(|| format!("local-{artist}-{}", item.name));
    Some(Now {
        track: Arc::new(Track {
            id,
            title: item.name,
            artist,
            main_artist,
            duration_ms: item.duration_ms,
            source: "Spotify".into(),
        }),
        progress_ms: playing.progress_ms.unwrap_or(0),
        playing: playing.is_playing,
        rate: 1.0,
    })
}

pub fn login(client_id: &str, client_secret: &str, port: u16) -> Result<(), String> {
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let url = format!(
        "https://accounts.spotify.com/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}",
        encode(client_id),
        encode(&redirect),
        encode(SCOPES)
    );
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("no se pudo abrir el puerto {port}: {e}"))?;

    println!("En tu app de developer.spotify.com, la Redirect URI debe ser:\n  {redirect}\n");
    println!("Abre esta URL para autorizar:\n  {url}\n");
    let _ = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    let code = loop {
        let (stream, _) = listener.accept().map_err(|e| e.to_string())?;
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let Some(query) = target.strip_prefix("/callback?") else {
            respond(&stream, "404 Not Found", "");
            continue;
        };
        let param = |key: &str| {
            query.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                (k == key).then(|| v.to_string())
            })
        };
        if let Some(code) = param("code") {
            respond(&stream, "200 OK", "Listo. Ya puedes cerrar esta pestaña.");
            break code;
        }
        respond(&stream, "400 Bad Request", "Spotify no devolvió el código.");
        return Err(format!(
            "Spotify respondió: {}",
            param("error").unwrap_or_else(|| "sin código".into())
        ));
    };

    let mut auth = Auth {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        access_token: String::new(),
        refresh_token: String::new(),
        expires_at: 0,
    };
    auth.exchange(
        &http_agent(),
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
        ],
    )?;
    if auth.refresh_token.is_empty() {
        return Err("Spotify no devolvió refresh_token".into());
    }
    println!("Sesión guardada en {}", auth_path().display());
    Ok(())
}

fn respond(mut stream: &TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
