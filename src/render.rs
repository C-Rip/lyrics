use std::fmt::Write as _;

use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const BOLD: u8 = 1;
pub const FAINT: u8 = 2;

/// Mitad derecha de un carácter ancho. La terminal la pinta al escribir la izquierda.
const CONTINUATION: char = '\0';

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub attr: u8,
}

const BLANK: Cell = Cell {
    ch: ' ',
    fg: None,
    bg: None,
    attr: 0,
};

pub struct Frame {
    pub width: usize,
    pub height: usize,
    cells: Vec<Cell>,
}

impl Frame {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![BLANK; width * height],
        }
    }

    pub fn clear(&mut self) {
        self.cells.fill(BLANK);
    }

    fn get(&self, x: usize, y: usize) -> Cell {
        self.cells[y * self.width + x]
    }

    pub fn fill_bg(&mut self, x: usize, y: usize, bg: Rgb) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = Cell {
                bg: Some(bg),
                ..BLANK
            };
        }
    }

    pub fn put_glyph(&mut self, x: usize, y: usize, ch: char, fg: Rgb) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = Cell {
                ch,
                fg: Some(fg),
                ..BLANK
            };
        }
    }

    /// Escribe texto encima de lo que haya. Conserva el fondo de las barras
    /// llenas y reemplaza los bloques parciales.
    pub fn put_text(&mut self, x: usize, y: usize, text: &str, fg: Rgb, attr: u8) {
        if y >= self.height {
            return;
        }
        let row = y * self.width;
        let mut col = x;
        for ch in text.chars() {
            let width = ch.width().unwrap_or(0);
            if width == 0 {
                continue;
            }
            if col + width > self.width {
                break;
            }
            for offset in 0..width {
                let cell = &mut self.cells[row + col + offset];
                *cell = Cell {
                    ch: if offset == 0 { ch } else { CONTINUATION },
                    fg: Some(fg),
                    bg: cell.bg,
                    attr,
                };
            }
            col += width;
        }
    }
}

pub fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Recuerda lo que hay en pantalla y solo emite las celdas que cambiaron.
pub struct Screen {
    prev: Option<Frame>,
}

impl Screen {
    pub fn new() -> Self {
        Self { prev: None }
    }

    pub fn invalidate(&mut self) {
        self.prev = None;
    }

    /// Añade a `out` las celdas que cambiaron. Devuelve true si añadió algo.
    pub fn draw(&mut self, frame: &Frame, out: &mut String) -> bool {
        let full = self
            .prev
            .as_ref()
            .is_none_or(|p| p.width != frame.width || p.height != frame.height);

        let header = out.len();
        if full {
            out.push_str("\x1b[0m\x1b[2J");
        }

        let mut pen: Option<(Option<Rgb>, Option<Rgb>, u8)> =
            if full { Some((None, None, 0)) } else { None };
        let mut cursor: Option<(usize, usize)> = None;

        for y in 0..frame.height {
            let mut x = 0;
            while x < frame.width {
                let cell = frame.get(x, y);
                if cell.ch == CONTINUATION {
                    x += 1;
                    continue;
                }
                let wide = x + 1 < frame.width && frame.get(x + 1, y).ch == CONTINUATION;
                let span = if wide { 2 } else { 1 };
                let changed = match &self.prev {
                    Some(prev) if !full => {
                        prev.get(x, y) != cell
                            || (wide && prev.get(x + 1, y) != frame.get(x + 1, y))
                    }
                    _ => cell != BLANK,
                };
                if changed {
                    if cursor != Some((x, y)) {
                        let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                    }
                    let style = (cell.fg, cell.bg, cell.attr);
                    if pen != Some(style) {
                        push_sgr(out, cell);
                        pen = Some(style);
                    }
                    out.push(cell.ch);
                    cursor = Some((x + span, y));
                }
                x += span;
            }
        }

        match &mut self.prev {
            Some(prev) if !full => prev.cells.copy_from_slice(&frame.cells),
            _ => {
                self.prev = Some(Frame {
                    width: frame.width,
                    height: frame.height,
                    cells: frame.cells.clone(),
                })
            }
        }
        out.len() > header
    }
}

fn push_sgr(out: &mut String, cell: Cell) {
    out.push_str("\x1b[0");
    if cell.attr & BOLD != 0 {
        out.push_str(";1");
    }
    if cell.attr & FAINT != 0 {
        out.push_str(";2");
    }
    if let Some(Rgb(r, g, b)) = cell.fg {
        let _ = write!(out, ";38;2;{r};{g};{b}");
    }
    if let Some(Rgb(r, g, b)) = cell.bg {
        let _ = write!(out, ";48;2;{r};{g};{b}");
    }
    out.push('m');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_changed_cells_are_written() {
        let mut screen = Screen::new();
        let mut frame = Frame::new(10, 3);
        let mut out = String::new();
        screen.draw(&frame, &mut out);

        out.clear();
        assert!(!screen.draw(&frame, &mut out));
        assert!(out.is_empty());

        frame.fill_bg(4, 2, Rgb(1, 2, 3));
        screen.draw(&frame, &mut out);
        assert!(out.contains("\x1b[3;5H"));
        assert_eq!(out.matches('H').count(), 1);
    }

    #[test]
    fn a_bar_that_shrinks_is_erased() {
        let mut screen = Screen::new();
        let mut frame = Frame::new(4, 2);
        let mut out = String::new();
        frame.fill_bg(0, 0, Rgb(9, 9, 9));
        screen.draw(&frame, &mut out);

        frame.clear();
        out.clear();
        assert!(screen.draw(&frame, &mut out));
        assert!(out.contains("\x1b[1;1H\x1b[0m "));
    }

    #[test]
    fn text_keeps_the_bar_behind_it() {
        let mut frame = Frame::new(6, 1);
        frame.fill_bg(1, 0, Rgb(5, 5, 5));
        frame.put_text(0, 0, "hey", Rgb(0, 255, 255), BOLD);
        assert_eq!(frame.get(1, 0).ch, 'e');
        assert_eq!(frame.get(1, 0).bg, Some(Rgb(5, 5, 5)));
        assert_eq!(frame.get(0, 0).bg, None);
    }
}
