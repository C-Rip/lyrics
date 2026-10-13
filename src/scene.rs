use crate::lyrics::Lyrics;
use crate::player::View;
use crate::render::{text_width, Frame, Rgb, BOLD, FAINT};

pub struct Style {
    pub fg: Rgb,
    pub attr: u8,
}

pub struct Theme {
    pub bar: Rgb,
    pub before: Style,
    pub current: Style,
    pub after: Style,
}

/// Colores de tu config de sptlrx y barras en el tono de cava detrás de la letra.
pub const THEME: Theme = Theme {
    bar: Rgb(86, 95, 137),
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
        let eighths = ((value.clamp(0.0, 1.0) * eighths_max as f32).round() as usize).clamp(1, eighths_max);
        let full = eighths / 8;
        let partial = eighths % 8;
        let x0 = layout.column(i);
        for x in x0..x0 + BAR_WIDTH {
            for level in 0..full {
                frame.fill_bg(x, frame.height - 1 - level, THEME.bar);
            }
            if partial > 0 && full < frame.height {
                frame.put_glyph(x, frame.height - 1 - full, BLOCKS[partial], THEME.bar);
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

type PaneKey = (usize, usize, usize, isize, usize, usize, Option<String>);

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
                let index = times.partition_point(|&t| t <= view.position_ms).saturating_sub(1);
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
            view.track.as_ref().map_or(0, |t| std::sync::Arc::as_ptr(t) as usize),
            index,
            self.scroll,
            frame.width,
            frame.height,
            if view.track.is_none() { view.message.clone() } else { None },
        );
        if self.key.as_ref() != Some(&key) {
            self.rows = build(view, index, self.scroll, frame.width, frame.height);
            self.key = Some(key);
        }
        for (y, row) in self.rows.iter().enumerate() {
            let Some((text, kind)) = row else { continue };
            let style = match kind {
                Kind::Before => &THEME.before,
                Kind::Current => &THEME.current,
                Kind::After => &THEME.after,
            };
            let x = frame.width.saturating_sub(text_width(text)) / 2;
            frame.put_text(x, y, text, style.fg, style.attr);
        }
    }
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
        Lyrics::NotFound => note("sin letra disponible"),
        Lyrics::Failed(e) => note(e),
    }
}

/// Como sptlrx: la línea `center` queda en el centro vertical, con las
/// anteriores arriba y las siguientes abajo. El estilo depende de su posición
/// respecto a `highlight`, la línea que está sonando.
pub fn layout(lines: &[String], center: usize, highlight: Option<usize>, cols: usize, rows: usize) -> Rows {
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
}
