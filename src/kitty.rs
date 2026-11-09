use std::collections::VecDeque;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::fd::FromRawFd;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine;

use crate::render::Rgb;
use crate::scene::{BAR_WIDTH, BarLayout};

/// Dos ids alternados: la imagen nueva se coloca antes de borrar la anterior,
/// así nunca hay un frame sin barras.
const IDS: [u32; 2] = [0x4c59_0001, 0x4c59_0002];
/// Kitty borra cada objeto de memoria compartida al leerlo. Los que rechace
/// se liberan después de este tiempo.
const KEEP: Duration = Duration::from_secs(1);
const MIN_HEIGHT_PX: usize = 2;
/// Id de la consulta. No coincide con los de las barras.
const PROBE_ID: u32 = 31;
const PROBE_WAIT: Duration = Duration::from_millis(200);

/// Hay capas si la terminal carga un píxel por memoria compartida.
/// El resultado se guarda: al cambiar el tamaño no se vuelve a preguntar.
pub fn supported() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(probe_layers)
}

fn probe_layers() -> bool {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return false;
    }
    let name = format!("/lyrics-probe-{}", std::process::id());
    let Ok(c_name) = write_shm(&name, &[0, 0, 0, 0]) else {
        return false;
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
    let query = format!("\x1b_Gi={PROBE_ID},s=1,v=1,a=q,t=s,f=32;{encoded}\x1b\\\x1b[c");
    let mut out = io::stdout();
    let wrote = out.write_all(query.as_bytes()).and_then(|_| out.flush());
    let buf = if wrote.is_ok() {
        read_until_attributes(PROBE_WAIT)
    } else {
        Vec::new()
    };
    unlink_shm(&c_name);
    graphics_ok(&buf, PROBE_ID)
}

fn read_until_attributes(timeout: Duration) -> Vec<u8> {
    let mut buf = Vec::new();
    let start = Instant::now();
    while start.elapsed() < timeout {
        if device_attributes(&buf) {
            break;
        }
        let left = timeout.saturating_sub(start.elapsed()).as_millis();
        if left == 0 {
            break;
        }
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, left.min(i32::MAX as u128) as i32) };
        if ready == 0 {
            break;
        }
        if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if ready < 0 {
            break;
        }
        let mut tmp = [0u8; 256];
        let n = unsafe { libc::read(libc::STDIN_FILENO, tmp.as_mut_ptr().cast(), tmp.len()) };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if n < 0 {
            break;
        }
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    buf
}

/// La respuesta gráfica de `id` termina en `OK`. Un error o solo los
/// atributos del dispositivo dejan las capas apagadas.
fn graphics_ok(buf: &[u8], id: u32) -> bool {
    buf.windows(3).enumerate().any(|(at, window)| {
        if window != [0x1b, b'_', b'G'] {
            return false;
        }
        let control_at = at + 3;
        let Some(semi) = buf[control_at..].iter().position(|&byte| byte == b';') else {
            return false;
        };
        let control = &buf[control_at..control_at + semi];
        let payload_at = control_at + semi + 1;
        let Some(end) = buf[payload_at..]
            .windows(2)
            .position(|pair| pair == [0x1b, b'\\'])
        else {
            return false;
        };
        control_has_id(control, id) && &buf[payload_at..payload_at + end] == b"OK"
    })
}

fn control_has_id(control: &[u8], id: u32) -> bool {
    let want = format!("i={id}");
    control
        .split(|&byte| byte == b',')
        .any(|part| part == want.as_bytes())
}

/// `ESC [ c` o `ESC [ ? ... c`, el tope de la consulta.
fn device_attributes(buf: &[u8]) -> bool {
    let mut rest = buf;
    while let Some(at) = rest.windows(2).position(|pair| pair == [0x1b, b'[']) {
        let mut cursor = at + 2;
        if rest.get(cursor) == Some(&b'?') {
            cursor += 1;
        }
        while rest
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b';')
        {
            cursor += 1;
        }
        if rest.get(cursor) == Some(&b'c') {
            return true;
        }
        rest = &rest[at + 1..];
    }
    false
}

const HEART_01_PNG: &[u8] = include_bytes!("../img/heart_01.png");
const NEXT_PNG: &[u8] = include_bytes!("../img/next.png");
const PAUSE_PNG: &[u8] = include_bytes!("../img/pause.png");
const PLAY1_PNG: &[u8] = include_bytes!("../img/play1.png");

fn decode_png_18x18(bytes: &[u8]) -> [u8; 18 * 18 * 4] {
    let mut idat = Vec::new();
    let mut idx = 8;
    while idx + 8 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[idx..idx + 4].try_into().unwrap_or_default()) as usize;
        let tag = &bytes[idx + 4..idx + 8];
        if tag == b"IDAT" && idx + 8 + length <= bytes.len() {
            idat.extend_from_slice(&bytes[idx + 8..idx + 8 + length]);
        }
        idx += 8 + length + 4;
    }
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).unwrap_or_default();
    let bpp = 4;
    let stride = 18 * bpp;
    let mut out = [0u8; 18 * 18 * 4];
    for y in 0..18 {
        if (y + 1) * (stride + 1) > raw.len() {
            break;
        }
        let filter_type = raw[y * (stride + 1)];
        let row_in = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let curr = y * stride;
        let prev = y.saturating_sub(1) * stride;
        for x in 0..stride {
            let a = if x >= bpp { out[curr + x - bpp] } else { 0 };
            let b = if y > 0 { out[prev + x] } else { 0 };
            let c = if y > 0 && x >= bpp { out[prev + x - bpp] } else { 0 };
            let val = row_in[x];
            let recon = match filter_type {
                0 => val,
                1 => val.wrapping_add(a),
                2 => val.wrapping_add(b),
                3 => val.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => {
                    let p = a as i16 + b as i16 - c as i16;
                    let pa = (p - a as i16).abs();
                    let pb = (p - b as i16).abs();
                    let pc = (p - c as i16).abs();
                    let pred = if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c };
                    val.wrapping_add(pred)
                }
                _ => val,
            };
            out[curr + x] = recon;
        }
    }
    out
}

fn load_sprite(path: &str, fallback: &[u8]) -> [u8; 18 * 18 * 4] {
    let bytes = std::fs::read(path).unwrap_or_else(|_| fallback.to_vec());
    decode_png_18x18(&bytes)
}

fn mirror_sprite_horizontal(sprite: &[u8; 18 * 18 * 4]) -> [u8; 18 * 18 * 4] {
    let mut out = [0u8; 18 * 18 * 4];
    for y in 0..18 {
        for x in 0..18 {
            let src_idx = (y * 18 + (17 - x)) * 4;
            let dst_idx = (y * 18 + x) * 4;
            out[dst_idx..dst_idx + 4].copy_from_slice(&sprite[src_idx..src_idx + 4]);
        }
    }
    out
}

fn compute_interior_mask(sprite: &[u8; 18 * 18 * 4]) -> [bool; 18 * 18] {
    let mut visited = [false; 18 * 18];
    let mut queue = Vec::with_capacity(18 * 18);
    for y in 0..18 {
        for x in 0..18 {
            if x == 0 || x == 17 || y == 0 || y == 17 {
                let a = sprite[(y * 18 + x) * 4 + 3];
                if a == 0 {
                    visited[y * 18 + x] = true;
                    queue.push((x, y));
                }
            }
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let (cx, cy) = queue[head];
        head += 1;
        let neighbors = [
            (cx.wrapping_sub(1), cy),
            (cx + 1, cy),
            (cx, cy.wrapping_sub(1)),
            (cx, cy + 1),
        ];
        for (nx, ny) in neighbors {
            if nx < 18 && ny < 18 {
                let idx = ny * 18 + nx;
                let a = sprite[idx * 4 + 3];
                if a == 0 && !visited[idx] {
                    visited[idx] = true;
                    queue.push((nx, ny));
                }
            }
        }
    }
    let mut mask = [false; 18 * 18];
    for y in 0..18 {
        for x in 0..18 {
            let idx = y * 18 + x;
            let a = sprite[idx * 4 + 3];
            if a == 0 && !visited[idx] {
                mask[idx] = true;
            }
        }
    }
    mask
}

fn compute_solid_interior_mask(sprite: &[u8; 18 * 18 * 4]) -> [bool; 18 * 18] {
    let mut visited = [false; 18 * 18];
    let mut queue = Vec::with_capacity(18 * 18);
    for y in 0..18 {
        for x in 0..18 {
            if x == 0 || x == 17 || y == 0 || y == 17 {
                let a = sprite[(y * 18 + x) * 4 + 3];
                if a == 0 {
                    visited[y * 18 + x] = true;
                    queue.push((x, y));
                }
            }
        }
    }
    let mut head = 0;
    while head < queue.len() {
        let (cx, cy) = queue[head];
        head += 1;
        let neighbors = [
            (cx.wrapping_sub(1), cy),
            (cx + 1, cy),
            (cx, cy.wrapping_sub(1)),
            (cx, cy + 1),
        ];
        for (nx, ny) in neighbors {
            if nx < 18 && ny < 18 {
                let idx = ny * 18 + nx;
                let a = sprite[idx * 4 + 3];
                if a == 0 && !visited[idx] {
                    visited[idx] = true;
                    queue.push((nx, ny));
                }
            }
        }
    }
    let mut mask = [false; 18 * 18];
    for y in 0..18 {
        for x in 0..18 {
            let idx = y * 18 + x;
            let a = sprite[idx * 4 + 3];
            let is_black = a > 0
                && sprite[idx * 4] == 0
                && sprite[idx * 4 + 1] == 0
                && sprite[idx * 4 + 2] == 0;
            if !visited[idx] && !is_black {
                mask[idx] = true;
            }
        }
    }
    mask
}

#[derive(Copy, Clone)]
pub struct LayerColors {
    pub bar: Rgb,
    pub heart: Rgb,
    pub btn: Rgb,
}

#[derive(Copy, Clone)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

fn blit_sprite(
    pixels: &mut [u8],
    stride: usize,
    bounds: (usize, usize),
    dst: Rect,
    sprite: &[u8; 18 * 18 * 4],
) {
    if dst.w == 0 || dst.h == 0 {
        return;
    }
    let (width, height) = bounds;
    for ty in 0..dst.h {
        let sy = (ty * 18) / dst.h;
        let py = dst.y + ty;
        if py >= height {
            continue;
        }
        for tx in 0..dst.w {
            let sx = (tx * 18) / dst.w;
            let px = dst.x + tx;
            if px >= width {
                continue;
            }
            let offset = (sy * 18 + sx) * 4;
            let a = sprite[offset + 3];
            if a == 0 {
                continue;
            }
            let pixel = [sprite[offset], sprite[offset + 1], sprite[offset + 2], a];
            let p_idx = py * stride + px * 4;
            if a == 255 {
                pixels[p_idx..p_idx + 4].copy_from_slice(&pixel);
            } else {
                let dst_a = pixels[p_idx + 3] as u32;
                let src_a = a as u32;
                let out_a = src_a + dst_a * (255 - src_a) / 255;
                if let Some(out_a_nz) = std::num::NonZeroU32::new(out_a) {
                    for c in 0..3 {
                        let src_c = pixel[c] as u32;
                        let dst_c = pixels[p_idx + c] as u32;
                        let out_c =
                            (src_c * src_a + dst_c * dst_a * (255 - src_a) / 255) / out_a_nz.get();
                        pixels[p_idx + c] = out_c as u8;
                    }
                    pixels[p_idx + 3] = out_a as u8;
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MediaButton {
    Previous,
    Next,
    PlayPause,
    Like,
}

/// Capa de barras en píxeles debajo del texto (protocolo gráfico de Kitty, z=-1).
pub struct BarLayer {
    width: usize,
    height: usize,
    cell_w: usize,
    cell_h: usize,
    pixels: Vec<u8>,
    heights: Vec<usize>,
    shown: Option<usize>,
    dirty: bool,
    seq: u64,
    pending: VecDeque<(Instant, CString)>,
    last_liked: Option<bool>,
    last_color: Option<Rgb>,
    last_btn_color: Option<Rgb>,
    last_playing: bool,
    last_trigger: Instant,
    smooth_bass: f32,
    last_beat: f32,
    heart_empty: [u8; 18 * 18 * 4],
    heart_mask: [bool; 18 * 18],
    sprite_prev: [u8; 18 * 18 * 4],
    sprite_next: [u8; 18 * 18 * 4],
    sprite_pause: [u8; 18 * 18 * 4],
    sprite_play: [u8; 18 * 18 * 4],
    mask_prev: [bool; 18 * 18],
    mask_next: [bool; 18 * 18],
    mask_pause: [bool; 18 * 18],
    mask_play: [bool; 18 * 18],
}

impl BarLayer {
    pub fn new(cols: usize, rows: usize, width_px: usize, height_px: usize) -> Option<Self> {
        let heart_empty = load_sprite("img/heart_01.png", HEART_01_PNG);
        let heart_mask = compute_interior_mask(&heart_empty);
        let sprite_next = load_sprite("img/next.png", NEXT_PNG);
        let sprite_prev = mirror_sprite_horizontal(&sprite_next);
        let sprite_pause = load_sprite("img/pause.png", PAUSE_PNG);
        let sprite_play = load_sprite("img/play1.png", PLAY1_PNG);
        let mask_next = compute_solid_interior_mask(&sprite_next);
        let mask_prev = compute_solid_interior_mask(&sprite_prev);
        let mask_pause = compute_solid_interior_mask(&sprite_pause);
        let mask_play = compute_solid_interior_mask(&sprite_play);
        let mut layer = Self {
            width: 0,
            height: 0,
            cell_w: 0,
            cell_h: 0,
            pixels: Vec::new(),
            heights: Vec::new(),
            shown: None,
            dirty: true,
            seq: 0,
            pending: VecDeque::new(),
            last_liked: None,
            last_color: None,
            last_btn_color: None,
            last_playing: false,
            last_trigger: Instant::now(),
            smooth_bass: 0.0,
            last_beat: 0.0,
            heart_empty,
            heart_mask,
            sprite_prev,
            sprite_next,
            sprite_pause,
            sprite_play,
            mask_prev,
            mask_next,
            mask_pause,
            mask_play,
        };
        layer
            .resize(cols, rows, width_px, height_px)
            .then_some(layer)
    }

    pub fn resize(&mut self, cols: usize, rows: usize, width_px: usize, height_px: usize) -> bool {
        if cols == 0 || rows == 0 || width_px < cols || height_px < rows {
            return false;
        }
        self.cell_w = width_px / cols;
        self.cell_h = height_px / rows;
        self.width = self.cell_w * cols;
        self.height = self.cell_h * rows;
        self.pixels = vec![0; self.width * self.height * 4];
        self.heights.clear();
        self.dirty = true;
        true
    }

    pub fn hit_test(&self, col: u16, row: u16, _cols: u16, rows: u16) -> Option<MediaButton> {
        let bottom_rows = ((self.cell_h * 2 + 17) / self.cell_h.max(1) + 2) as u16;
        if row < rows.saturating_sub(bottom_rows) {
            return None;
        }
        let scale = (self.cell_h * 2 / 18).max(1);
        let base_size = 18 * scale;
        let margin_x = self.cell_w;
        let gap = (self.cell_w * 3 / 2).max(scale * 4);

        let col_px = col as usize * self.cell_w + self.cell_w / 2;

        let heart_x = self.width.saturating_sub(base_size + margin_x);
        if col_px >= heart_x.saturating_sub(self.cell_w) {
            return Some(MediaButton::Like);
        }

        let x0 = margin_x;
        let x0_end = x0 + base_size;
        if col_px >= x0.saturating_sub(self.cell_w / 2) && col_px < x0_end + gap / 2 {
            return Some(MediaButton::Previous);
        }

        let x1 = x0 + base_size + gap;
        let x1_end = x1 + base_size;
        if col_px >= x1 - gap / 2 && col_px < x1_end + gap / 2 {
            return Some(MediaButton::Next);
        }

        let x2 = x1 + base_size + gap;
        let x2_end = x2 + base_size;
        if col_px >= x2 - gap / 2 && col_px < x2_end + gap {
            return Some(MediaButton::PlayPause);
        }

        None
    }

    pub fn update(
        &mut self,
        layout: &BarLayout,
        values: &[f32],
        colors: LayerColors,
        liked: Option<bool>,
        playing: bool,
    ) {
        let bar_color = colors.bar;
        let heart_color = colors.heart;
        let btn_color = colors.btn;
        let count = values.len().min(layout.count);
        let mut changed = self.heights.len() != count;
        self.heights.resize(count, usize::MAX);
        for (slot, &value) in self.heights.iter_mut().zip(values) {
            let px = ((value.clamp(0.0, 1.0) * self.height as f32).round() as usize)
                .clamp(MIN_HEIGHT_PX, self.height);
            if *slot != px {
                *slot = px;
                changed = true;
            }
        }
        let dt = self.last_trigger.elapsed().as_secs_f32();
        let bass = values.iter().take(4).copied().fold(0.0f32, f32::max);
        let mids = values.iter().skip(4).take(14).copied().fold(0.0f32, f32::max);
        let highs = values.iter().skip(18).take(14).copied().fold(0.0f32, f32::max);
        let energy = (bass * 0.5 + mids * 0.7 + highs * 0.5).min(1.0);
        let beat_energy = bass.max(mids * 0.75);
        let rise = beat_energy - self.smooth_bass;
        self.smooth_bass = self.smooth_bass * 0.82 + beat_energy * 0.18;

        // Detecta el ataque rítmico (bombo o guitarras/caja) o pulso regular en calma solo si está sonando
        if playing && ((rise > 0.08 && dt > 0.16) || dt > 0.55) {
            self.last_trigger = Instant::now();
        }

        let pulse_t = self.last_trigger.elapsed().as_secs_f32();
        // Latido en 2 etapas ágil y enérgico:
        // Solo late cuando la canción se está reproduciendo (playing == true)
        let beat = if playing && liked.is_some() {
            if pulse_t < 0.09 {
                (std::f32::consts::PI * pulse_t / 0.09).sin()
            } else if (0.09..0.20).contains(&pulse_t) {
                0.45 * (std::f32::consts::PI * (pulse_t - 0.09) / 0.11).sin()
            } else {
                0.0
            }
        } else {
            0.0
        };
        if (beat - self.last_beat).abs() > 0.01 || (beat == 0.0 && self.last_beat > 0.0) {
            self.last_beat = beat;
            changed = true;
        }
        if self.last_liked != liked
            || self.last_color != Some(heart_color)
            || self.last_btn_color != Some(btn_color)
            || self.last_playing != playing
        {
            self.last_liked = liked;
            self.last_color = Some(heart_color);
            self.last_btn_color = Some(btn_color);
            self.last_playing = playing;
            changed = true;
        }
        if playing && energy > 0.01 {
            changed = true;
        }
        if !changed && !self.dirty {
            return;
        }

        self.pixels.fill(0);
        let Rgb(r, g, b) = bar_color;
        let pixel = [r, g, b, 255];
        let bar_px = BAR_WIDTH * self.cell_w;
        let stride = self.width * 4;
        for (i, &h) in self.heights.iter().enumerate() {
            let x0 = layout.column(i) * self.cell_w;
            let x1 = (x0 + bar_px).min(self.width);
            if x0 >= x1 {
                continue;
            }
            let first = &mut self.pixels[(self.height - h) * stride..][x0 * 4..x1 * 4];
            for chunk in first.as_chunks_mut::<4>().0 {
                *chunk = pixel;
            }
            let template = (self.height - h) * stride + x0 * 4;
            for y in self.height - h + 1..self.height {
                let start = y * stride + x0 * 4;
                self.pixels
                    .copy_within(template..template + (x1 - x0) * 4, start);
            }
        }

        let scale = (self.cell_h * 2 / 18).max(1);
        let base_size = 18 * scale;
        let margin_x = self.cell_w;
        let margin_y = self.cell_h.saturating_sub(1);
        let base_y = self.height.saturating_sub(base_size + margin_y);
        let gap = (self.cell_w * 3 / 2).max(scale * 4);

        // Tono de los botones: color de la letra activa (THEME.current.fg) con destello al ritmo
        let btn_brighten = |c: u8| -> u8 {
            let boost = (255 - c) as f32 * (0.20 * beat);
            (c as f32 + boost).min(255.0) as u8
        };
        let btn_tint = [
            btn_brighten(btn_color.0),
            btn_brighten(btn_color.1),
            btn_brighten(btn_color.2),
            255,
        ];

        let fill_sprite = |base: &[u8; 18 * 18 * 4], mask: &[bool; 18 * 18]| -> [u8; 18 * 18 * 4] {
            let mut s = *base;
            for (idx, &is_interior) in mask.iter().enumerate() {
                if is_interior {
                    let p = idx * 4;
                    s[p..p + 4].copy_from_slice(&btn_tint);
                }
            }
            s
        };

        let sprite_prev_tinted = fill_sprite(&self.sprite_prev, &self.mask_prev);
        let sprite_next_tinted = fill_sprite(&self.sprite_next, &self.mask_next);
        let sprite_pp_tinted = if playing {
            fill_sprite(&self.sprite_pause, &self.mask_pause)
        } else {
            fill_sprite(&self.sprite_play, &self.mask_play)
        };

        // 1 y 2. Salto vertical normalizado y sincronizado al ritmo para los botones
        let jump = if playing {
            beat * (scale as f32 * 4.0)
        } else {
            0.0
        };
        let y_btn = (base_y as f32 - jump).max(0.0) as usize;

        let bounds = (self.width, self.height);

        // Botón Anterior (<<): espejo de next.png
        let x_prev = margin_x;
        blit_sprite(
            &mut self.pixels,
            stride,
            bounds,
            Rect {
                x: x_prev,
                y: y_btn,
                w: base_size,
                h: base_size,
            },
            &sprite_prev_tinted,
        );

        // Botón Siguiente (>>): next.png
        let x_next = x_prev + base_size + gap;
        blit_sprite(
            &mut self.pixels,
            stride,
            bounds,
            Rect {
                x: x_next,
                y: y_btn,
                w: base_size,
                h: base_size,
            },
            &sprite_next_tinted,
        );

        // 3. Botón Play/Pausa:
        // Usa la misma animación de salto sincronizado al ritmo (en pausa queda quieto)
        let x_pp = x_next + base_size + gap;
        blit_sprite(
            &mut self.pixels,
            stride,
            bounds,
            Rect {
                x: x_pp,
                y: y_btn,
                w: base_size,
                h: base_size,
            },
            &sprite_pp_tinted,
        );

        // 4. Corazón (Me gusta en Spotify) en la esquina inferior derecha
        if let Some(liked) = liked {
            let mut sprite = self.heart_empty;
            if liked {
                let heart_brighten = |c: u8| -> u8 {
                    let boost = (255 - c) as f32 * 0.28 * beat;
                    (c as f32 + boost).min(255.0) as u8
                };
                let heart_tint = [
                    heart_brighten(heart_color.0),
                    heart_brighten(heart_color.1),
                    heart_brighten(heart_color.2),
                    255,
                ];
                for (idx, &is_interior) in self.heart_mask.iter().enumerate() {
                    if is_interior {
                        let p = idx * 4;
                        sprite[p..p + 4].copy_from_slice(&heart_tint);
                    }
                }
            }
            let max_extra = (scale as f32 * 2.2).max(4.0);
            let extra = if playing {
                (max_extra * beat).round() as usize
            } else {
                0
            };
            let target_size = base_size + extra;
            let base_x = self.width.saturating_sub(base_size + margin_x);
            let start_x = base_x.saturating_sub(extra / 2);
            let start_y = base_y.saturating_sub(extra / 2);
            blit_sprite(
                &mut self.pixels,
                stride,
                bounds,
                Rect {
                    x: start_x,
                    y: start_y,
                    w: target_size,
                    h: target_size,
                },
                &sprite,
            );
        }

        self.dirty = true;
    }

    /// Añade a `out` la imagen nueva si algo cambió.
    pub fn emit(&mut self, out: &mut String) -> io::Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        let name = self.store()?;
        let next = self.shown.map_or(0, |i| 1 - i);
        let encoded = base64::engine::general_purpose::STANDARD.encode(name);
        let _ = write!(
            out,
            "\x1b[H\x1b_Ga=T,f=32,t=s,s={},v={},i={},p=1,z=-1,C=1,q=2;{encoded}\x1b\\",
            self.width, self.height, IDS[next]
        );
        if let Some(old) = self.shown {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", IDS[old]);
        }
        self.shown = Some(next);
        self.dirty = false;
        Ok(true)
    }

    pub fn shutdown(&mut self, out: &mut String) {
        for id in IDS {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        }
        self.shown = None;
    }

    fn store(&mut self) -> io::Result<String> {
        while self
            .pending
            .front()
            .is_some_and(|(t, _)| t.elapsed() > KEEP)
        {
            let (_, name) = self.pending.pop_front().unwrap();
            unlink_shm(&name);
        }
        self.seq += 1;
        let name = format!("/lyrics-{}-{}", std::process::id(), self.seq);
        let c_name = write_shm(&name, &self.pixels)?;
        self.pending.push_back((Instant::now(), c_name));
        Ok(name)
    }
}

fn write_shm(name: &str, bytes: &[u8]) -> io::Result<CString> {
    let c_name =
        CString::new(name).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let fd = unsafe {
        libc::shm_open(
            c_name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    if let Err(err) = file.write_all(bytes) {
        unlink_shm(&c_name);
        return Err(err);
    }
    Ok(c_name)
}

fn unlink_shm(name: &CString) {
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

impl Drop for BarLayer {
    fn drop(&mut self) {
        for (_, name) in self.pending.drain(..) {
            unlink_shm(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::bar_layout;

    #[test]
    fn bars_are_drawn_from_the_bottom_and_erased_when_they_drop() {
        let mut layer = BarLayer::new(5, 4, 10, 8).unwrap();
        let layout = bar_layout(5);
        let colors = LayerColors {
            bar: Rgb(1, 2, 3),
            heart: Rgb(255, 30, 60),
            btn: Rgb(0, 255, 255),
        };
        layer.update(&layout, &[1.0, 0.0], colors, None, false);
        let alpha = |l: &BarLayer, x: usize, y: usize| l.pixels[(y * l.width + x) * 4 + 3];
        assert_eq!(alpha(&layer, 0, 0), 255);
        assert_eq!(alpha(&layer, 3, 7), 255);
        assert_eq!(alpha(&layer, 4, 7), 0);
        assert_eq!(alpha(&layer, 6, 5), 0);
        assert_eq!(alpha(&layer, 6, 6), 255);

        let mut out = String::new();
        assert!(layer.emit(&mut out).unwrap());
        layer.update(&layout, &[0.0, 0.0], colors, None, false);
        assert_eq!(alpha(&layer, 0, 0), 0);
        assert!(layer.emit(&mut out).unwrap());
        assert!(out.contains("z=-1"));
        assert!(out.contains(&format!("a=d,d=I,i={}", IDS[0])));

        out.clear();
        layer.update(&layout, &[0.0, 0.0], colors, None, false);
        assert!(!layer.emit(&mut out).unwrap());
    }

    #[test]
    fn mirror_sprite_flips_x() {
        let mut sprite = [0u8; 18 * 18 * 4];
        sprite[0] = 42; // (0, 0)
        let mirrored = mirror_sprite_horizontal(&sprite);
        assert_eq!(mirrored[17 * 4], 42); // (17, 0)
    }

    #[test]
    fn hit_test_locates_buttons() {
        let layer = BarLayer::new(80, 24, 800, 480).unwrap();
        // bottom row
        assert_eq!(layer.hit_test(1, 23, 80, 24), Some(MediaButton::Previous));
        assert_eq!(layer.hit_test(79, 23, 80, 24), Some(MediaButton::Like));
    }

    #[test]
    fn graphics_ok_enables_layers() {
        assert!(graphics_ok(b"\x1b_Gi=31;OK\x1b\\", 31));
        assert!(graphics_ok(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c", 31));
    }

    #[test]
    fn graphics_error_disables_layers() {
        assert!(!graphics_ok(
            b"\x1b_Gi=31;EBADF:Failed to read image file\x1b\\",
            31
        ));
    }

    #[test]
    fn device_attributes_without_graphics_disable_layers() {
        assert!(!graphics_ok(b"\x1b[?62;22c", 31));
        assert!(!graphics_ok(b"\x1b[c", 31));
        assert!(device_attributes(b"\x1b[?62;22c"));
        assert!(device_attributes(b"\x1b[c"));
    }
}
