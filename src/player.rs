use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::lyrics::{self, Lyrics};
use crate::spotify::{self, Now, Spotify, Track};

const POLL: Duration = Duration::from_secs(2);
/// En pausa se consulta más seguido para detectar pronto el play y volver a
/// la línea que suena.
const POLL_PAUSED: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(5);

pub struct View {
    pub track: Option<Arc<Track>>,
    pub lyrics: Arc<Lyrics>,
    pub position_ms: u64,
    pub playing: bool,
    pub message: Option<String>,
}

struct State {
    now: Option<Now>,
    at: Instant,
    lyrics: Arc<Lyrics>,
    message: Option<String>,
}

/// Consulta Spotify en segundo plano. Entre consultas, la posición avanza con el reloj local.
pub struct Player {
    state: Arc<Mutex<State>>,
}

impl Player {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State {
            now: None,
            at: Instant::now(),
            lyrics: Arc::new(Lyrics::Loading),
            message: Some("Conectando con Spotify".into()),
        }));
        let shared = Arc::clone(&state);
        thread::Builder::new()
            .name("spotify".into())
            .spawn(move || run(&shared))
            .expect("no se pudo crear el hilo de Spotify");
        Self { state }
    }

    pub fn view(&self) -> View {
        let state = self.state.lock().unwrap();
        let position_ms = state.now.as_ref().map_or(0, |now| {
            let mut position = now.progress_ms;
            if now.playing {
                position += state.at.elapsed().as_millis() as u64;
            }
            if now.track.duration_ms > 0 {
                position = position.min(now.track.duration_ms);
            }
            position
        });
        View {
            track: state.now.as_ref().map(|now| Arc::clone(&now.track)),
            lyrics: Arc::clone(&state.lyrics),
            position_ms,
            playing: state.now.as_ref().is_some_and(|now| now.playing),
            message: state.message.clone(),
        }
    }
}

fn run(state: &Mutex<State>) {
    lyrics::migrate_cache();
    let agent = spotify::http_agent();
    let mut client = loop {
        match Spotify::new(agent.clone()) {
            Ok(client) => break client,
            Err(e) => {
                state.lock().unwrap().message = Some(e);
                thread::sleep(RETRY);
            }
        }
    };

    let mut current: Option<String> = None;
    loop {
        let requested = Instant::now();
        let wait = match client.now_playing() {
            Ok(Some(now)) => {
                let track = Arc::clone(&now.track);
                let changed = current.as_deref() != Some(track.id.as_str());
                let remaining = track.duration_ms.saturating_sub(now.progress_ms);
                let wait = if now.playing {
                    POLL.min(Duration::from_millis(remaining + 300))
                } else {
                    POLL_PAUSED
                };
                {
                    let mut s = state.lock().unwrap();
                    s.now = Some(now);
                    s.at = requested;
                    s.message = None;
                    if changed {
                        s.lyrics = Arc::new(Lyrics::Loading);
                    }
                }
                if changed {
                    current = Some(track.id.clone());
                    let found = Arc::new(lyrics::fetch(&agent, &track));
                    let mut s = state.lock().unwrap();
                    if s.now.as_ref().is_some_and(|n| n.track.id == track.id) {
                        s.lyrics = found;
                    }
                }
                wait
            }
            Ok(None) => {
                current = None;
                let mut s = state.lock().unwrap();
                s.now = None;
                s.lyrics = Arc::new(Lyrics::NotFound);
                s.message = Some("Nada sonando en Spotify".into());
                POLL
            }
            Err(e) => {
                state.lock().unwrap().message = Some(e);
                RETRY
            }
        };
        thread::sleep(wait.saturating_sub(requested.elapsed()));
    }
}
