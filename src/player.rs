use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::lyrics::{self, Lyrics};
use crate::spotify::{self, Now, Spotify, Track};
use crate::translate::{self, Job, Lang};

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
    pub notice: Option<String>,
}

pub(crate) struct State {
    pub(crate) now: Option<Now>,
    at: Instant,
    /// Letra de lrclib, o el estado de carga.
    original: Arc<Lyrics>,
    /// Traducción de esa letra, cuando ya está lista.
    pub(crate) translated: Option<Arc<Lyrics>>,
    /// El usuario quiere ver la traducción. Se conserva al cambiar de canción.
    show_translation: bool,
    /// Hubo `--translate`, así que la tecla `t` puede alternar.
    translate: bool,
    message: Option<String>,
    pub(crate) notice: Option<String>,
}

/// Foto de la reproducción. `now` vacío significa que no hay nada sonando.
pub struct Poll {
    pub now: Option<Now>,
    pub message: Option<String>,
}

/// Origen de la canción y la posición. Las letras y la traducción viven fuera.
pub trait PlayerBackend: Send {
    fn startup_message(&self) -> &'static str;

    /// Espera como mucho `timeout` y devuelve la reproducción conocida.
    fn poll(&mut self, timeout: Duration) -> Result<Poll, String>;
}

/// Spotify Web API. El reloj local sigue avanzando entre consultas.
pub struct SpotifyApiBackend {
    agent: ureq::Agent,
    client: Option<Spotify>,
    /// En pausa se consulta más seguido. Sin canción, el intervalo es el normal.
    paused: bool,
    /// La primera consulta sale ya; las siguientes esperan el intervalo.
    primed: bool,
}

impl SpotifyApiBackend {
    pub fn new() -> Self {
        Self {
            agent: spotify::http_agent(),
            client: None,
            paused: false,
            primed: false,
        }
    }
}

impl PlayerBackend for SpotifyApiBackend {
    fn startup_message(&self) -> &'static str {
        "Conectando con Spotify"
    }

    fn poll(&mut self, timeout: Duration) -> Result<Poll, String> {
        if self.client.is_none() {
            self.client = Some(Spotify::new(self.agent.clone())?);
        }
        if self.primed {
            let cap = if self.paused { POLL_PAUSED } else { POLL };
            thread::sleep(timeout.min(cap));
        }
        self.primed = true;
        let client = self.client.as_mut().expect("cliente recién creado");
        match client.now_playing() {
            Ok(Some(now)) => {
                self.paused = !now.playing;
                Ok(Poll {
                    now: Some(now),
                    message: None,
                })
            }
            Ok(None) => {
                self.paused = false;
                Ok(Poll {
                    now: None,
                    message: Some("Nada sonando en Spotify".into()),
                })
            }
            Err(e) => {
                self.primed = false;
                Err(e)
            }
        }
    }
}

/// Consulta el reproductor en segundo plano. Entre consultas, la posición avanza con el reloj local.
pub struct Player {
    state: Arc<Mutex<State>>,
}

impl Player {
    pub fn start(backend: Box<dyn PlayerBackend>, lang: Option<Lang>) -> Self {
        let state = Arc::new(Mutex::new(State {
            now: None,
            at: Instant::now(),
            original: Arc::new(Lyrics::Loading),
            translated: None,
            show_translation: lang.is_some(),
            translate: lang.is_some(),
            message: Some(backend.startup_message().into()),
            notice: None,
        }));
        let shared = Arc::clone(&state);
        let jobs = lang.map(|lang| translate::spawn(lang, Arc::clone(&state)));
        thread::Builder::new()
            .name("player".into())
            .spawn(move || run(&shared, lang, jobs, backend))
            .expect("no se pudo crear el hilo del reproductor");
        Self { state }
    }

    pub fn view(&self) -> View {
        let state = self.state.lock().unwrap();
        let position_ms = state.now.as_ref().map_or(0, |now| {
            position_at(
                now.progress_ms,
                now.playing,
                now.rate,
                state.at.elapsed(),
                now.track.duration_ms,
            )
        });
        View {
            track: state.now.as_ref().map(|now| Arc::clone(&now.track)),
            lyrics: visible_lyrics(state.show_translation, &state.original, &state.translated),
            position_ms,
            playing: state.now.as_ref().is_some_and(|now| now.playing),
            message: state.message.clone(),
            notice: state.notice.clone(),
        }
    }

    /// Vuelve a la letra original o a la traducción. No hace nada sin `--translate`.
    pub fn toggle_translation(&self) {
        let mut state = self.state.lock().unwrap();
        if state.translate {
            state.show_translation = !state.show_translation;
        }
    }
}

fn run(
    state: &Mutex<State>,
    lang: Option<Lang>,
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    mut backend: Box<dyn PlayerBackend>,
) {
    lyrics::migrate_cache();
    let agent = spotify::http_agent();

    let mut current: Option<String> = None;
    // Próximo intento y la espera que se usará si vuelve a fallar.
    let mut retry: Option<(Instant, Duration)> = None;
    let mut playing = false;
    let mut progress_ms = 0u64;
    let mut duration_ms = 0u64;
    loop {
        let requested = Instant::now();
        let mut timeout = Duration::from_secs(30);
        if playing && duration_ms > 0 {
            let remaining = duration_ms.saturating_sub(progress_ms);
            timeout = timeout.min(Duration::from_millis(remaining.saturating_add(300)));
        }
        if let Some((at, _)) = retry {
            timeout = timeout.min(at.saturating_duration_since(requested));
        }

        let polled = match backend.poll(timeout) {
            Ok(polled) => polled,
            Err(e) => {
                state.lock().unwrap().message = Some(e);
                thread::sleep(RETRY);
                continue;
            }
        };
        let sampled = Instant::now();

        let Some(now) = polled.now else {
            playing = false;
            progress_ms = 0;
            duration_ms = 0;
            current = None;
            retry = None;
            let mut s = state.lock().unwrap();
            s.now = None;
            s.original = Arc::new(Lyrics::NotFound);
            s.translated = None;
            s.notice = None;
            s.message = polled.message.or_else(|| Some("Nada sonando".into()));
            continue;
        };

        let track = Arc::clone(&now.track);
        playing = now.playing;
        progress_ms = now.progress_ms;
        duration_ms = track.duration_ms;
        let changed = current.as_deref() != Some(track.id.as_str());
        {
            let mut s = state.lock().unwrap();
            s.now = Some(now);
            s.at = sampled;
            s.message = None;
            if changed {
                s.original = Arc::new(Lyrics::Loading);
                s.translated = None;
                s.notice = None;
            }
        }
        let due = retry.as_ref().is_some_and(|(at, _)| sampled >= *at);
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
                Lyrics::Retrying(_) => Some((Instant::now() + backoff, next_backoff(backoff))),
                _ => None,
            };
            let (original, translated, pending) = match lang {
                Some(lang) => prepare_translation(&track.id, &found, &lang),
                None => (found, None, None),
            };
            let pending = {
                let mut s = state.lock().unwrap();
                if s.now.as_ref().is_some_and(|n| n.track.id == track.id) {
                    let same_failure = match (&*original, &*s.original) {
                        (Lyrics::Retrying(next), Lyrics::Retrying(prev)) => next == prev,
                        _ => false,
                    };
                    if !same_failure {
                        s.original = original;
                        s.translated = translated;
                    }
                    pending.filter(|_| !same_failure)
                } else {
                    None
                }
            };
            if let Some(job) = pending
                && let Some(jobs) = &jobs
            {
                let _ = jobs.send(job);
            }
        }
    }
}

/// Avanza `progress_ms` con el reloj local mientras suena. `rate` 1 es tiempo real.
pub(crate) fn position_at(
    progress_ms: u64,
    playing: bool,
    rate: f64,
    elapsed: Duration,
    duration_ms: u64,
) -> u64 {
    let mut position = progress_ms;
    if playing {
        let rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            1.0
        };
        let advanced = elapsed.as_secs_f64() * 1000.0 * rate;
        if advanced > 0.0 {
            position = position.saturating_add(advanced as u64);
        }
    }
    if duration_ms > 0 {
        position = position.min(duration_ms);
    }
    position
}

/// Si la traducción ya está en disco, la devuelve. Si no, deja la original y
/// pide traducirla en el otro hilo.
fn prepare_translation(
    track_id: &str,
    found: &Arc<Lyrics>,
    lang: &Lang,
) -> (Arc<Lyrics>, Option<Arc<Lyrics>>, Option<Job>) {
    if let Some(hit) = translate::cached(track_id, found, lang) {
        return (Arc::clone(found), Some(Arc::new(hit)), None);
    }
    let pending = lines_pending(found).then(|| Job {
        track_id: track_id.to_string(),
        lyrics: Arc::clone(found),
    });
    (Arc::clone(found), None, pending)
}

pub(crate) fn visible_lyrics(
    show_translation: bool,
    original: &Arc<Lyrics>,
    translated: &Option<Arc<Lyrics>>,
) -> Arc<Lyrics> {
    if show_translation && let Some(translated) = translated {
        return Arc::clone(translated);
    }
    Arc::clone(original)
}

fn lines_pending(lyrics: &Lyrics) -> bool {
    matches!(lyrics, Lyrics::Synced { .. } | Lyrics::Plain(_))
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

    #[test]
    fn toggle_keeps_the_original_until_asked_again() {
        let original = Arc::new(Lyrics::Plain(vec!["hello".into()]));
        let translated = Some(Arc::new(Lyrics::Plain(vec!["hola".into()])));
        let text = |lyrics: &Lyrics| match lyrics {
            Lyrics::Plain(lines) => lines.clone(),
            _ => panic!("no es texto"),
        };
        assert_eq!(
            text(&visible_lyrics(true, &original, &translated)),
            ["hola"]
        );
        assert_eq!(
            text(&visible_lyrics(false, &original, &translated)),
            ["hello"]
        );
        assert!(Arc::ptr_eq(
            &visible_lyrics(true, &original, &None),
            &original
        ));
    }

    #[test]
    fn playback_rate_scales_the_local_clock() {
        let elapsed = Duration::from_secs(2);
        assert_eq!(position_at(1_000, true, 2.0, elapsed, 10_000), 5_000);
        assert_eq!(position_at(1_000, true, 1.0, elapsed, 10_000), 3_000);
        assert_eq!(position_at(1_000, false, 2.0, elapsed, 10_000), 1_000);
        assert_eq!(position_at(9_000, true, 2.0, elapsed, 10_000), 10_000);
    }
}
