use std::path::PathBuf;
use std::sync::OnceLock;

use crate::lyrics::Lyrics;
use crate::player::View;
use crate::render::{BOLD, FAINT, Frame, Rgb, text_width};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Rgb,
    pub attr: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    pub bar: Rgb,
    pub buttons: Rgb,
    /// Color del relleno del corazón cuando está en Me gusta (ej. rojo o acento)
    pub heart: Rgb,
    pub before: Style,
    pub current: Style,
    pub after: Style,
}

impl Theme {
    pub const DEFAULT: Self = Self {
        bar: Rgb(86, 95, 137),
        buttons: Rgb(0, 255, 255),
        heart: Rgb(255, 30, 60),
        before: Style {
            fg: Rgb(255, 255, 255),
            attr: FAINT,
        },
        current: Style {
            fg: Rgb(0, 255, 255),
            attr: BOLD,
        },
        after: Style {
            fg: Rgb(65, 72, 104),
            attr: FAINT,
        },
    };

    pub fn load() -> Self {
        for path in config_paths() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                return Self::parse(&content);
            }
        }
        Self::DEFAULT
    }

    pub fn parse(text: &str) -> Self {
        let mut theme = Self::DEFAULT;
        let mut custom_buttons = false;
        let mut custom_current = false;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with('#')
                || line.starts_with(';')
                || line.starts_with("//")
            {
                continue;
            }
            let (key, val) = match line.split_once(['=', ':']) {
                Some((k, v)) => (k.trim().to_lowercase(), v.trim()),
                None => continue,
            };
            let Some(color) = parse_color(val) else {
                continue;
            };
            match key.as_str() {
                "bar" | "bars" | "bar_color" => theme.bar = color,
                "button" | "buttons" | "btn" | "controls" => {
                    theme.buttons = color;
                    custom_buttons = true;
                }
                "heart" | "like" => theme.heart = color,
                "before" => theme.before.fg = color,
                "current" | "active" => {
                    theme.current.fg = color;
                    custom_current = true;
                }
                "after" => theme.after.fg = color,
                _ => {}
            }
        }

        if custom_current && !custom_buttons {
            theme.buttons = theme.current.fg;
        }

        theme
    }
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let user_dir = crate::spotify::config_dir();
    paths.push(user_dir.join("config.conf"));
    paths.push(user_dir.join("lyrics.conf"));
    paths.push(PathBuf::from("/etc/lyrics/config.conf"));
    paths.push(PathBuf::from("/etc/lyrics/lyrics.conf"));
    paths
}

pub fn parse_color(val: &str) -> Option<Rgb> {
    let s = val.trim().trim_matches(['"', '\'']).to_lowercase();
    match s.as_str() {
        "cyan" => return Some(Rgb(0, 255, 255)),
        "white" => return Some(Rgb(255, 255, 255)),
        "red" => return Some(Rgb(255, 0, 0)),
        "green" => return Some(Rgb(0, 255, 0)),
        "blue" => return Some(Rgb(0, 0, 255)),
        "yellow" => return Some(Rgb(255, 255, 0)),
        "magenta" | "purple" => return Some(Rgb(255, 0, 255)),
        "black" => return Some(Rgb(0, 0, 0)),
        "gray" | "grey" => return Some(Rgb(128, 128, 128)),
        _ => {}
    }

    let hex = s.strip_prefix('#').unwrap_or(&s);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Rgb(r, g, b));
    }
    if hex.len() == 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
        let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
        let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
        return Some(Rgb(r * 17, g * 17, b * 17));
    }

    let body = s
        .strip_prefix("rgb(")
        .and_then(|b| b.strip_suffix(')'))
        .unwrap_or(&s);

    let parts: Vec<&str> = body
        .split([',', ' '])
        .filter(|part| !part.trim().is_empty())
        .collect();
    if parts.len() == 3 {
        let r = parts[0].trim().parse::<u8>().ok()?;
        let g = parts[1].trim().parse::<u8>().ok()?;
        let b = parts[2].trim().parse::<u8>().ok()?;
        return Some(Rgb(r, g, b));
    }

    None
}

static THEME_INSTANCE: OnceLock<Theme> = OnceLock::new();

pub fn theme() -> &'static Theme {
    THEME_INSTANCE.get_or_init(Theme::load)
}

pub const BAR_WIDTH: usize = 2;
const BAR_GAP: usize = 1;
const BLOCKS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];

pub struct BarLayout {
    pub count: usize,
    left: usize,
}

impl BarLayout {
    /// Primera columna de la barra `i`.
    pub fn column(&self, i: usize) -> usize {
        self.left + i * (BAR_WIDTH + BAR_GAP)
    }
}

pub fn bar_layout(cols: usize) -> BarLayout {
    let unit = BAR_WIDTH + BAR_GAP;
    if cols < BAR_WIDTH {
        return BarLayout { count: 0, left: 0 };
    }
    let count = (cols + BAR_GAP) / unit;
    let used = count * unit - BAR_GAP;
    BarLayout {
        count,
        left: (cols - used) / 2,
    }
}

/// Las barras se dibujan como fondo de celda, con octavos de bloque en la punta.
pub fn draw_bars(frame: &mut Frame, layout: &BarLayout, values: &[f32]) {
    let eighths_max = frame.height * 8;
    for (i, &value) in values.iter().take(layout.count).enumerate() {
        let eighths =
            ((value.clamp(0.0, 1.0) * eighths_max as f32).round() as usize).clamp(1, eighths_max);
        let full = eighths / 8;
        let partial = eighths % 8;
        let x0 = layout.column(i);
        for x in x0..x0 + BAR_WIDTH {
            for level in 0..full {
                frame.fill_bg(x, frame.height - 1 - level, theme().bar);
            }
            if partial > 0 && full < frame.height {
                frame.put_glyph(x, frame.height - 1 - full, BLOCKS[partial], theme().bar);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Before,
    Current,
    After,
}

type Rows = Vec<Option<(String, Kind)>>;

type PaneKey = (
    usize,
    usize,
    usize,
    isize,
    usize,
    usize,
    Option<String>,
    Option<String>,
);

/// Guarda la última disposición de la letra para no recalcularla en cada frame.
pub struct LyricsPane {
    key: Option<PaneKey>,
    rows: Rows,
    /// Líneas de desplazamiento respecto a la que suena. Solo se respeta en
    /// pausa (o en letras sin tiempos); al reanudar vuelve a 0.
    scroll: isize,
    lyrics_id: usize,
}

impl LyricsPane {
    pub fn new() -> Self {
        Self {
            key: None,
            rows: Vec::new(),
            scroll: 0,
            lyrics_id: 0,
        }
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add(lines);
    }

    pub fn scroll_reset(&mut self) {
        self.scroll = 0;
    }

    pub fn draw(&mut self, frame: &mut Frame, view: &View) {
        let lyrics_id = std::sync::Arc::as_ptr(&view.lyrics) as *const () as usize;
        if lyrics_id != self.lyrics_id {
            self.lyrics_id = lyrics_id;
            self.scroll = 0;
        }
        let index = match &*view.lyrics {
            Lyrics::Synced { times, lines } => {
                if view.playing {
                    self.scroll = 0;
                }
                let index = times
                    .partition_point(|&t| t <= view.position_ms)
                    .saturating_sub(1);
                let last = lines.len().saturating_sub(1) as isize;
                self.scroll = self.scroll.clamp(-(index as isize), last - index as isize);
                index
            }
            Lyrics::Plain(lines) => {
                self.scroll = self.scroll.clamp(0, lines.len().saturating_sub(1) as isize);
                0
            }
            _ => {
                self.scroll = 0;
                usize::MAX
            }
        };
        let key = (
            lyrics_id,
            view.track
                .as_ref()
                .map_or(0, |t| std::sync::Arc::as_ptr(t) as usize),
            index,
            self.scroll,
            frame.width,
            frame.height,
            if view.track.is_none() {
                view.message.clone()
            } else {
                None
            },
            view.notice.clone(),
        );
        if self.key.as_ref() != Some(&key) {
            self.rows = build(view, index, self.scroll, frame.width, frame.height);
            if let Some(notice) = &view.notice
                && let Some(row) = self.rows.last_mut()
            {
                let text = wrap(notice, frame.width)
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                *row = Some((text, Kind::After));
            }
            self.key = Some(key);
        }
        for (y, row) in self.rows.iter().enumerate() {
            let Some((text, kind)) = row else { continue };
            let style = match kind {
                Kind::Before => &theme().before,
                Kind::Current => &theme().current,
                Kind::After => &theme().after,
            };
            let x = frame.width.saturating_sub(text_width(text)) / 2;
            frame.put_text(x, y, text, style.fg, style.attr);
        }
    }
}

/// Título de la ventana: artista y canción, o el aviso si no hay pista.
/// Kitty lo muestra en la pestaña. Los controles se quitan para no cortar la secuencia.
pub fn window_title(view: &View) -> String {
    let raw = match &view.track {
        Some(track) if track.artist.is_empty() => track.title.clone(),
        Some(track) => format!("{} - {}", track.artist, track.title),
        None => view.message.clone().unwrap_or_default(),
    };
    let clean: String = raw.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        "lyrics".into()
    } else {
        clean
    }
}

/// Sin letra: el título limpio en el centro y, debajo, la app que suena.
fn missing(track: &crate::spotify::Track, cols: usize, rows: usize) -> Rows {
    let title = if track.artist.is_empty() {
        track.title.clone()
    } else {
        format!("{} - {}", track.artist, track.title)
    };
    let lines = if track.source.is_empty() {
        vec![title]
    } else {
        vec![title, track.source.clone()]
    };
    layout(&lines, 0, Some(0), cols, rows)
}

fn build(view: &View, index: usize, scroll: isize, cols: usize, rows: usize) -> Rows {
    let Some(track) = &view.track else {
        let message = view.message.clone().unwrap_or_default();
        return layout(&[message], 0, Some(0), cols, rows);
    };
    let heading = format!("{} - {}", track.artist, track.title);
    let note = |text: &str| layout(&[heading.clone(), text.to_string()], 0, Some(0), cols, rows);
    match &*view.lyrics {
        Lyrics::Synced { lines, .. } => {
            let center = (index as isize + scroll).max(0) as usize;
            layout(lines, center, Some(index), cols, rows)
        }
        Lyrics::Plain(lines) => layout_from_top(lines, scroll.max(0) as usize, cols, rows),
        Lyrics::Loading => note("buscando letra"),
        Lyrics::Instrumental => note("instrumental"),
        Lyrics::NotFound => missing(track, cols, rows),
        Lyrics::Retrying(_) => note("lrclib saturado, reintentando"),
        Lyrics::Failed(e) => note(e),
    }
}

/// Como sptlrx: la línea `center` queda en el centro vertical, con las
/// anteriores arriba y las siguientes abajo. El estilo depende de su posición
/// respecto a `highlight`, la línea que está sonando.
pub fn layout(
    lines: &[String],
    center: usize,
    highlight: Option<usize>,
    cols: usize,
    rows: usize,
) -> Rows {
    let mut out: Rows = vec![None; rows];
    if lines.is_empty() || rows == 0 || cols == 0 {
        return out;
    }
    let center = center.min(lines.len() - 1);
    let kind = |i: usize| match highlight {
        Some(h) if i < h => Kind::Before,
        Some(h) if i == h => Kind::Current,
        Some(_) => Kind::After,
        None => Kind::Before,
    };
    let parts = |i: usize| {
        let blank_current = highlight == Some(i) && lines[i].trim().is_empty();
        wrap(if blank_current { "♪" } else { &lines[i] }, cols)
    };

    let mut middle = parts(center);
    middle.truncate(rows);
    let top = (rows - middle.len()) / 2;
    let middle_len = middle.len();
    for (i, part) in middle.into_iter().enumerate() {
        out[top + i] = Some((part, kind(center)));
    }

    let mut y = top;
    'above: for i in (0..center).rev() {
        for part in parts(i).into_iter().rev() {
            if y == 0 {
                break 'above;
            }
            y -= 1;
            out[y] = Some((part, kind(i)));
        }
    }

    let mut y = top + middle_len;
    'below: for i in center + 1..lines.len() {
        for part in parts(i) {
            if y >= rows {
                break 'below;
            }
            out[y] = Some((part, kind(i)));
            y += 1;
        }
    }
    out
}

fn layout_from_top(lines: &[String], start: usize, cols: usize, rows: usize) -> Rows {
    let mut out: Rows = vec![None; rows];
    let mut y = 0;
    for line in lines.iter().skip(start) {
        for part in wrap(line, cols) {
            if y >= rows {
                return out;
            }
            out[y] = Some((part, Kind::Before));
            y += 1;
        }
    }
    out
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let word_width = text_width(word);
        if used > 0 && used + 1 + word_width > width {
            rows.push(std::mem::take(&mut current));
            used = 0;
        }
        if used > 0 {
            current.push(' ');
            used += 1;
        }
        if word_width <= width {
            current.push_str(word);
            used += word_width;
            continue;
        }
        for ch in word.chars() {
            let w = text_width(ch.encode_utf8(&mut [0; 4]));
            if used + w > width && used > 0 {
                rows.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(ch);
            used += w;
        }
    }
    rows.push(current);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn current_line_is_centered() {
        let rows = layout(&lines(5), 2, Some(2), 40, 11);
        assert_eq!(rows[5], Some(("line 2".into(), Kind::Current)));
        assert_eq!(rows[4], Some(("line 1".into(), Kind::Before)));
        assert_eq!(rows[6], Some(("line 3".into(), Kind::After)));
        assert_eq!(rows[0], None);
    }

    #[test]
    fn scrolled_view_keeps_the_playing_line_highlighted() {
        let rows = layout(&lines(10), 6, Some(2), 40, 11);
        assert_eq!(rows[5], Some(("line 6".into(), Kind::After)));
        assert_eq!(rows[1], Some(("line 2".into(), Kind::Current)));
        assert_eq!(rows[0], Some(("line 1".into(), Kind::Before)));
    }

    fn view(playing: bool, position_ms: u64) -> View {
        View {
            track: None,
            lyrics: std::sync::Arc::new(Lyrics::Synced {
                times: (0..10).map(|i| i * 1000).collect(),
                lines: lines(10),
            }),
            position_ms,
            playing,
            message: None,
            notice: None,
            liked: None,
        }
    }

    #[test]
    fn scrolling_only_sticks_while_paused() {
        let mut frame = Frame::new(40, 11);
        let mut pane = LyricsPane::new();
        let paused = view(false, 2500);
        pane.draw(&mut frame, &paused);
        pane.scroll_by(3);
        pane.draw(&mut frame, &paused);
        assert_eq!(pane.scroll, 3);
        pane.scroll_by(100);
        pane.draw(&mut frame, &paused);
        assert_eq!(pane.scroll, 7);

        let mut resumed = paused;
        resumed.playing = true;
        pane.draw(&mut frame, &resumed);
        assert_eq!(pane.scroll, 0);
    }

    #[test]
    fn missing_lyrics_center_the_title_and_the_app() {
        let mut frame = Frame::new(40, 11);
        let mut pane = LyricsPane::new();
        let title = "Artista - Canción";
        let source = "YouTube | Chrome";
        pane.draw(
            &mut frame,
            &View {
                track: Some(std::sync::Arc::new(crate::spotify::Track {
                    id: "yt:abc".into(),
                    title: "Canción".into(),
                    artist: "Artista".into(),
                    main_artist: "Artista".into(),
                    duration_ms: 1_000,
                    source: source.into(),
                    spotify_id: None,
                })),
                lyrics: std::sync::Arc::new(Lyrics::NotFound),
                position_ms: 0,
                playing: true,
                message: None,
                notice: None,
                liked: None,
            },
        );
        let mut out = String::new();
        let mut screen = crate::render::Screen::new();
        screen.draw(&frame, &mut out);
        let title_col = (40 - text_width(title)) / 2 + 1;
        let source_col = (40 - text_width(source)) / 2 + 1;
        assert!(out.contains(&format!("\x1b[6;{title_col}H")), "{out}");
        assert!(out.contains(title));
        assert!(out.contains(&format!("\x1b[7;{source_col}H")), "{out}");
        assert!(out.contains(source));
    }

    fn titled(artist: &str, title: &str) -> View {
        let mut sample = view(true, 0);
        sample.track = Some(std::sync::Arc::new(crate::spotify::Track {
            id: "1".into(),
            title: title.into(),
            artist: artist.into(),
            main_artist: artist.into(),
            duration_ms: 1_000,
            source: String::new(),
            spotify_id: None,
        }));
        sample
    }

    #[test]
    fn window_title_names_the_song_and_drops_controls() {
        assert_eq!(window_title(&titled("Artista", "Canción\u{1b}")), "Artista - Canción");
        assert_eq!(window_title(&titled("", "Solo")), "Solo");

        let mut waiting = view(false, 0);
        waiting.message = Some("Buscando reproductor".into());
        assert_eq!(window_title(&waiting), "Buscando reproductor");
        waiting.message = None;
        assert_eq!(window_title(&waiting), "lyrics");
    }

    #[test]
    fn long_lines_wrap_on_words() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn bars_fill_from_the_bottom() {
        let mut frame = Frame::new(5, 4);
        let layout = bar_layout(5);
        assert_eq!(layout.count, 2);
        draw_bars(&mut frame, &layout, &[0.5, 0.0]);
        let mut out = String::new();
        let mut screen = crate::render::Screen::new();
        screen.draw(&frame, &mut out);
        assert!(out.contains("48;2;86;95;137"));
        assert!(out.contains('▁'));
    }

    #[test]
    fn parse_colors_supports_hex_rgb_and_names() {
        assert_eq!(parse_color("#ff0000"), Some(Rgb(255, 0, 0)));
        assert_eq!(parse_color("00ff00"), Some(Rgb(0, 255, 0)));
        assert_eq!(parse_color("#00f"), Some(Rgb(0, 0, 255)));
        assert_eq!(parse_color("rgb(10, 20, 30)"), Some(Rgb(10, 20, 30)));
        assert_eq!(parse_color("123, 45, 67"), Some(Rgb(123, 45, 67)));
        assert_eq!(parse_color("cyan"), Some(Rgb(0, 255, 255)));
        assert_eq!(parse_color("invalid"), None);
    }

    #[test]
    fn theme_parse_reads_config_and_handles_button_inheritance() {
        let conf = r##"
            # Mi tema personalizado
            bar = "#123456"
            before = "rgb(100, 100, 100)"
            current = "#ff00ff"
            after = "50, 60, 70"
            heart = "#e02020"
        "##;
        let theme = Theme::parse(conf);
        assert_eq!(theme.bar, Rgb(0x12, 0x34, 0x56));
        assert_eq!(theme.before.fg, Rgb(100, 100, 100));
        assert_eq!(theme.current.fg, Rgb(255, 0, 255));
        // Sin `buttons` explícito, hereda `current.fg`
        assert_eq!(theme.buttons, Rgb(255, 0, 255));
        assert_eq!(theme.after.fg, Rgb(50, 60, 70));
        assert_eq!(theme.heart, Rgb(0xe0, 0x20, 0x20));

        // Con `buttons` explícito
        let conf2 = "current = cyan\nbuttons = yellow";
        let theme2 = Theme::parse(conf2);
        assert_eq!(theme2.current.fg, Rgb(0, 255, 255));
        assert_eq!(theme2.buttons, Rgb(255, 255, 0));
    }
}
