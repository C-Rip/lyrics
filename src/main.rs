mod audio;
mod kitty;
mod lyr;
mod lyrics;
mod player;
mod render;
mod scene;
mod spectrum;
mod spotify;
mod translate;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::{cursor, execute, terminal};

use audio::{AudioTap, SAMPLE_RATE, WINDOW};
use kitty::BarLayer;
use lyrics::Lyrics;
use player::{Player, View};
use render::{Frame, Screen};
use scene::{LyricsPane, THEME, bar_layout, draw_bars};
use spectrum::Analyzer;
use spotify::Track;

const ACTIVE_FRAME: Duration = Duration::from_micros(16_667);
const IDLE_FRAME: Duration = Duration::from_millis(100);
/// Frames seguidos sin cambios antes de bajar a la frecuencia de reposo.
const IDLE_AFTER: u32 = 30;
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

const USAGE: &str = "\
Uso:
  lyrics                               letras de Spotify con el espectro de audio detrás
  lyrics --demo                        audio y letra de prueba
  lyrics --translate <idioma>          traduce la letra (vale junto con --demo)
  lyrics login <client_id> <secret> [puerto]
                                       inicia sesión en Spotify (puerto por defecto 8888)
  lyrics deepl <clave>                 guarda la clave de DeepL

Idiomas: es, en, pt, fr, de, it, ja, ko, zh, ru
         (también spanish, español, english, …)

Teclas:
  q, Esc, Ctrl+C       salir
  flechas, j/k, rueda  recorrer la letra en pausa (vuelve sola al dar play)
  RePág/AvPág          media pantalla
  Inicio               volver a la línea que suena
  espacio              pausa (solo en --demo)
  t                    original o traducción (con --translate)";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = dispatch(&args) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn dispatch(args: &[String]) -> Result<(), String> {
    if let Some(cmd) = args.first().map(String::as_str) {
        match cmd {
            "login" => return login(&args[1..]),
            "deepl" => return save_deepl(&args[1..]),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => {}
        }
    }
    let mut demo = false;
    let mut lang = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--demo" => demo = true,
            "--translate" => {
                let name = rest
                    .next()
                    .ok_or_else(|| format!("falta el idioma después de --translate\n\n{USAGE}"))?;
                lang = Some(
                    translate::parse_lang(name)
                        .ok_or_else(|| format!("idioma desconocido: {name}\n\n{USAGE}"))?,
                );
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("argumento desconocido: {other}\n\n{USAGE}")),
        }
    }
    run(demo, lang)
}

fn save_deepl(args: &[String]) -> Result<(), String> {
    let Some(key) = args.first() else {
        return Err(USAGE.into());
    };
    if args.len() != 1 {
        return Err(USAGE.into());
    }
    translate::save_key(key)
}

fn login(args: &[String]) -> Result<(), String> {
    let (Some(id), Some(secret)) = (args.first(), args.get(1)) else {
        return Err(USAGE.into());
    };
    let port = match args.get(2) {
        Some(p) => p.parse().map_err(|_| format!("puerto inválido: {p}"))?,
        None => 8888,
    };
    spotify::login(id, secret, port)
}

enum Source {
    Live {
        audio: AudioTap,
        player: Player,
    },
    Demo {
        started: Instant,
        paused_at: Option<Instant>,
        original: Arc<Lyrics>,
        translated: Option<Arc<Lyrics>>,
        show_translation: bool,
        track: Arc<Track>,
    },
}

impl Source {
    fn live(lang: Option<translate::Lang>) -> Self {
        Source::Live {
            audio: AudioTap::start(),
            player: Player::start(lang),
        }
    }

    fn demo(lang: Option<translate::Lang>) -> Result<Self, String> {
        let lines = [
            "Could fill an hour",
            "They call me iron man",
            "I got the power",
            "I turn it up",
            "Tan bed and speedo",
            "Chicago crowd",
            "Going loud",
            "For the greatest guido",
            "I'm so addictive",
            "Call me MSG",
            "Yeah, I'm a Pay-Per-View",
            "But you can roll with me",
            "This is my only chance",
            "I ain't about to throw it",
        ];
        let original = Lyrics::Synced {
            times: (0..lines.len() as u64).map(|i| i * 2000).collect(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
        };
        let translated = lang
            .map(|lang| translate::blocking("demo", &original, &lang).map(Arc::new))
            .transpose()?;
        Ok(Source::Demo {
            started: Instant::now(),
            paused_at: None,
            show_translation: translated.is_some(),
            original: Arc::new(original),
            translated,
            track: Arc::new(Track {
                id: "demo".into(),
                title: "Demo".into(),
                artist: "lyrics".into(),
                main_artist: "lyrics".into(),
                duration_ms: 28_000,
            }),
        })
    }

    /// Solo en el demo: la pausa de Spotify se controla desde Spotify.
    fn toggle_pause(&mut self) {
        if let Source::Demo {
            started, paused_at, ..
        } = self
        {
            match paused_at.take() {
                Some(at) => *started += at.elapsed(),
                None => *paused_at = Some(Instant::now()),
            }
        }
    }

    /// Con `--translate`, alterna entre la letra traducida y la original.
    fn toggle_translation(&mut self) {
        match self {
            Source::Live { player, .. } => player.toggle_translation(),
            Source::Demo {
                translated: Some(_),
                show_translation,
                ..
            } => *show_translation = !*show_translation,
            Source::Demo { .. } => {}
        }
    }

    fn samples(&self, out: &mut [f32]) {
        match self {
            Source::Live { audio, .. } => {
                audio.latest(out);
            }
            Source::Demo {
                paused_at: Some(_), ..
            } => out.fill(0.0),
            Source::Demo { started, .. } => {
                let t = started.elapsed().as_secs_f32();
                let beat = (1.0 - (t * 2.0).fract()).powi(3);
                let tones = [
                    (60.0, 0.6 * beat),
                    (220.0, 0.25),
                    (880.0, 0.12),
                    (3500.0, 0.05),
                ];
                for (i, sample) in out.iter_mut().enumerate() {
                    let s = t + i as f32 / SAMPLE_RATE as f32;
                    *sample = tones
                        .iter()
                        .map(|&(hz, amp)| {
                            let wobble = 1.0 + 0.5 * (t * 0.7 + hz).sin();
                            amp * wobble * (2.0 * std::f32::consts::PI * hz * s).sin()
                        })
                        .sum();
                }
            }
        }
    }

    fn view(&self) -> View {
        match self {
            Source::Live { player, .. } => player.view(),
            Source::Demo {
                started,
                paused_at,
                original,
                translated,
                show_translation,
                track,
            } => {
                let elapsed = paused_at
                    .unwrap_or_else(Instant::now)
                    .duration_since(*started);
                View {
                    track: Some(Arc::clone(track)),
                    lyrics: player::visible_lyrics(*show_translation, original, translated),
                    position_ms: elapsed.as_millis() as u64 % track.duration_ms,
                    playing: paused_at.is_none(),
                    message: None,
                    notice: None,
                }
            }
        }
    }
}

fn run(demo: bool, lang: Option<translate::Lang>) -> Result<(), String> {
    if lang.is_some() {
        translate::api_key()?;
    }
    let mut source = if demo {
        Source::demo(lang)?
    } else {
        Source::live(lang)
    };

    let mut stdout = io::stdout();
    terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        cursor::Hide,
        terminal::DisableLineWrap
    )
    .map_err(|e| e.to_string())?;
    // Solo clics y rueda (sin seguimiento de movimiento), en formato SGR.
    let _ = write!(stdout, "{MOUSE_ON}");
    let _ = stdout.flush();

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous_hook(info);
    }));

    let result = event_loop(&mut source, &mut stdout).map_err(|e| e.to_string());
    restore();
    result
}

fn restore() {
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b[0m{MOUSE_OFF}");
    let _ = execute!(
        stdout,
        terminal::EnableLineWrap,
        cursor::Show,
        terminal::LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}

/// En Kitty las barras van en una capa de imagen debajo del texto. En otras
/// terminales se dibujan como fondo de celda.
fn bar_layer(cols: u16, rows: u16) -> Option<BarLayer> {
    if !kitty::supported() {
        return None;
    }
    let size = terminal::window_size().ok()?;
    BarLayer::new(
        cols as usize,
        rows as usize,
        size.width as usize,
        size.height as usize,
    )
}

fn event_loop(source: &mut Source, stdout: &mut io::Stdout) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let mut frame = Frame::new(cols as usize, rows as usize);
    let mut layer = bar_layer(cols, rows);
    let mut screen = Screen::new();
    let mut pane = LyricsPane::new();
    let mut analyzer = Analyzer::new();
    let mut samples = vec![0.0f32; WINDOW];
    let mut out = String::with_capacity(64 * 1024);

    let mut last = Instant::now();
    let mut next = last;
    let mut idle_frames = 0u32;

    loop {
        let timeout = next.saturating_duration_since(Instant::now());
        if event::poll(timeout)? {
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        let ctrl_c = key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                            if let Some(layer) = &mut layer {
                                out.clear();
                                layer.shutdown(&mut out);
                                stdout.write_all(out.as_bytes())?;
                                stdout.flush()?;
                            }
                            return Ok(());
                        }
                        let page = (frame.height / 2).max(1) as isize;
                        match key.code {
                            KeyCode::Up | KeyCode::Char('k') => pane.scroll_by(-1),
                            KeyCode::Down | KeyCode::Char('j') => pane.scroll_by(1),
                            KeyCode::PageUp => pane.scroll_by(-page),
                            KeyCode::PageDown => pane.scroll_by(page),
                            KeyCode::Home => pane.scroll_reset(),
                            KeyCode::Char(' ') => source.toggle_pause(),
                            KeyCode::Char('t') => source.toggle_translation(),
                            _ => {}
                        }
                        idle_frames = 0;
                        next = Instant::now();
                    }
                    Event::Mouse(mouse) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => pane.scroll_by(-1),
                            MouseEventKind::ScrollDown => pane.scroll_by(1),
                            _ => {}
                        }
                        idle_frames = 0;
                        next = Instant::now();
                    }
                    Event::Resize(w, h) => {
                        frame = Frame::new(w as usize, h as usize);
                        layer = bar_layer(w, h);
                        screen.invalidate();
                        idle_frames = 0;
                    }
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            if Instant::now() < next {
                continue;
            }
        }

        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32().min(0.25);
        last = now;

        let layout = bar_layout(frame.width);
        analyzer.set_bars(layout.count);
        source.samples(&mut samples);
        let values = analyzer.process(&samples, dt);

        frame.clear();
        if frame.height > 0 {
            match &mut layer {
                Some(layer) => layer.update(&layout, values, THEME.bar),
                None => draw_bars(&mut frame, &layout, values),
            }
            pane.draw(&mut frame, &source.view());
        }

        out.clear();
        out.push_str("\x1b[?2026h");
        // El texto va primero: un redibujado completo borra la pantalla y,
        // con ella, la imagen de barras, que entonces se vuelve a colocar.
        let mut wrote = screen.draw(&frame, &mut out);
        if let Some(layer) = &mut layer {
            wrote |= layer.emit(&mut out)?;
        }
        if wrote {
            out.push_str("\x1b[0m\x1b[?2026l");
            stdout.write_all(out.as_bytes())?;
            stdout.flush()?;
            idle_frames = 0;
        } else {
            idle_frames = idle_frames.saturating_add(1);
        }
        let step = if idle_frames > IDLE_AFTER {
            IDLE_FRAME
        } else {
            ACTIVE_FRAME
        };
        next = (next + step).max(now);
    }
}
