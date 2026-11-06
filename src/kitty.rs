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

fn load_heart_empty() -> [u8; 18 * 18 * 4] {
    let empty_bytes = std::fs::read("img/heart_01.png").unwrap_or_else(|_| HEART_01_PNG.to_vec());
    decode_png_18x18(&empty_bytes)
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

fn blit_sprite_scaled(
    pixels: &mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    start_x: usize,
    start_y: usize,
    sprite: &[u8; 18 * 18 * 4],
    target_size: usize,
) {
    if target_size == 0 {
        return;
    }
    for ty in 0..target_size {
        let sy = (ty * 18) / target_size;
        let py = start_y + ty;
        if py >= height {
            continue;
        }
        for tx in 0..target_size {
            let sx = (tx * 18) / target_size;
            let px = start_x + tx;
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
                if out_a > 0 {
                    for c in 0..3 {
                        let src_c = pixel[c] as u32;
                        let dst_c = pixels[p_idx + c] as u32;
                        let out_c =
                            (src_c * src_a + dst_c * dst_a * (255 - src_a) / 255) / out_a;
                        pixels[p_idx + c] = out_c as u8;
                    }
                    pixels[p_idx + 3] = out_a as u8;
                }
            }
        }
    }
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
    last_trigger: Instant,
    smooth_bass: f32,
    last_beat: f32,
    heart_empty: [u8; 18 * 18 * 4],
    heart_mask: [bool; 18 * 18],
}

impl BarLayer {
    pub fn new(cols: usize, rows: usize, width_px: usize, height_px: usize) -> Option<Self> {
        let heart_empty = load_heart_empty();
        let heart_mask = compute_interior_mask(&heart_empty);
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
            last_trigger: Instant::now(),
            smooth_bass: 0.0,
            last_beat: 0.0,
            heart_empty,
            heart_mask,
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

    pub fn update(
        &mut self,
        layout: &BarLayout,
        values: &[f32],
        bar_color: Rgb,
        heart_color: Rgb,
        liked: Option<bool>,
    ) {
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
        let mids = values.iter().skip(4).take(14).copied().fold(0.0f32, f32::max) * 0.75;
        let energy = bass.max(mids);
        let rise = energy - self.smooth_bass;
        self.smooth_bass = self.smooth_bass * 0.82 + energy * 0.18;

        // Detecta el ataque rítmico (bombo o guitarras/caja) o pulso regular en calma
        if (rise > 0.08 && dt > 0.16) || dt > 0.55 {
            self.last_trigger = Instant::now();
        }

        let pulse_t = self.last_trigger.elapsed().as_secs_f32();
        // Latido en 2 etapas ágil y enérgico:
        // Etapa 1: golpe principal rápido y nítido (0.0..0.09s)
        // Etapa 2: rebote rítmico secundario (0.09..0.20s)
        let beat = if liked.is_some() {
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
        if self.last_liked != liked || self.last_color != Some(heart_color) {
            self.last_liked = liked;
            self.last_color = Some(heart_color);
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

        if let Some(liked) = liked {
            let mut sprite = self.heart_empty;
            if liked {
                let brighten = |c: u8| -> u8 {
                    let boost = (255 - c) as f32 * 0.28 * beat;
                    (c as f32 + boost).min(255.0) as u8
                };
                let hr = brighten(heart_color.0);
                let hg = brighten(heart_color.1);
                let hb = brighten(heart_color.2);
                for (idx, &is_interior) in self.heart_mask.iter().enumerate() {
                    if is_interior {
                        let p = idx * 4;
                        sprite[p] = hr;
                        sprite[p + 1] = hg;
                        sprite[p + 2] = hb;
                        sprite[p + 3] = 255;
                    }
                }
            }
            let scale = (self.cell_h * 2 / 18).max(1);
            let base_size = 18 * scale;
            let max_extra = (scale as f32 * 2.2).max(4.0);
            let extra = (max_extra * beat).round() as usize;
            let target_size = base_size + extra;
            let margin_x = self.cell_w;
            let margin_y = self.cell_h.saturating_sub(1);
            let base_x = self.width.saturating_sub(base_size + margin_x);
            let base_y = self.height.saturating_sub(base_size + margin_y);
            let start_x = base_x.saturating_sub(extra / 2);
            let start_y = base_y.saturating_sub(extra / 2);
            blit_sprite_scaled(
                &mut self.pixels,
                stride,
                self.width,
                self.height,
                start_x,
                start_y,
                &sprite,
                target_size,
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
        layer.update(&layout, &[1.0, 0.0], Rgb(1, 2, 3));
        let alpha = |l: &BarLayer, x: usize, y: usize| l.pixels[(y * l.width + x) * 4 + 3];
        assert_eq!(alpha(&layer, 0, 0), 255);
        assert_eq!(alpha(&layer, 3, 7), 255);
        assert_eq!(alpha(&layer, 4, 7), 0);
        assert_eq!(alpha(&layer, 6, 5), 0);
        assert_eq!(alpha(&layer, 6, 6), 255);

        let mut out = String::new();
        assert!(layer.emit(&mut out).unwrap());
        layer.update(&layout, &[0.0, 0.0], Rgb(1, 2, 3));
        assert_eq!(alpha(&layer, 0, 0), 0);
        assert!(layer.emit(&mut out).unwrap());
        assert!(out.contains("z=-1"));
        assert!(out.contains(&format!("a=d,d=I,i={}", IDS[0])));

        out.clear();
        layer.update(&layout, &[0.0, 0.0], Rgb(1, 2, 3));
        assert!(!layer.emit(&mut out).unwrap());
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
