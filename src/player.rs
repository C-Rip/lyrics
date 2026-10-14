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
/// Primera espera cuando lrclib responde saturado, y el paso inicial del backoff.
const RETRY_LYRICS_MIN: Duration = Duration::from_secs(5);
const RETRY_LYRICS_MAX: Duration = Duration::from_secs(60);

/// Duplica la espera entre reintentos de lrclib hasta un minuto.
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(RETRY_LYRICS_MAX)
}

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
    // Próximo intento y la espera que se usará si vuelve a fallar.
    let mut retry: Option<(Instant, Duration)> = None;
    loop {
        let requested = Instant::now();
        let wait = match client.now_playing() {
            Ok(Some(now)) => {
                let track = Arc::clone(&now.track);
                let changed = current.as_deref() != Some(track.id.as_str());
                let remaining = track.duration_ms.saturating_sub(now.progress_ms);
                let mut wait = if now.playing {
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
                let due = retry.as_ref().is_some_and(|(at, _)| requested >= *at);
                if changed || due {
                    let backoff = if changed {
                        RETRY_LYRICS_MIN
                    } else {
                        retry.map_or(RETRY_LYRICS_MIN, |(_, delay)| delay)
                    };
                    if changed {
                        current = Some(track.id.clone());
                    }
                    let found = Arc::new(lyrics::fetch(&agent, &track));
                    retry = match &*found {
                        Lyrics::Retrying(_) => {
                            Some((Instant::now() + backoff, next_backoff(backoff)))
                        }
                        _ => None,
                    };
                    let mut s = state.lock().unwrap();
                    if s.now.as_ref().is_some_and(|n| n.track.id == track.id) {
                        let same_failure = match (&*found, &*s.lyrics) {
                            (Lyrics::Retrying(next), Lyrics::Retrying(prev)) => next == prev,
                            _ => false,
                        };
                        if !same_failure {
                            s.lyrics = found;
                        }
                    }
                }
                if let Some((at, _)) = retry {
                    wait = wait.min(at.saturating_duration_since(requested));
                }
                wait
            }
            Ok(None) => {
                current = None;
                retry = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lyrics_backoff_doubles_until_one_minute() {
        let mut delay = RETRY_LYRICS_MIN;
        let mut steps = Vec::new();
        for _ in 0..6 {
            steps.push(delay);
            delay = next_backoff(delay);
        }
        assert_eq!(
            steps,
            vec![
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(20),
                Duration::from_secs(40),
                Duration::from_secs(60),
                Duration::from_secs(60),
            ]
        );
    }
}
