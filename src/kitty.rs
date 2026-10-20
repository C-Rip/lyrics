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

/// Capa de barras en píxeles debajo del texto (protocolo gráfico de Kitty, z=-1).
pub struct BarLayer {
    width: usize,
    height: usize,
    cell_w: usize,
    pixels: Vec<u8>,
    heights: Vec<usize>,
    shown: Option<usize>,
    dirty: bool,
    seq: u64,
    pending: VecDeque<(Instant, CString)>,
}

impl BarLayer {
    pub fn new(cols: usize, rows: usize, width_px: usize, height_px: usize) -> Option<Self> {
        let mut layer = Self {
            width: 0,
            height: 0,
            cell_w: 0,
            pixels: Vec::new(),
            heights: Vec::new(),
            shown: None,
            dirty: true,
            seq: 0,
            pending: VecDeque::new(),
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
        self.width = self.cell_w * cols;
        self.height = (height_px / rows) * rows;
        self.pixels = vec![0; self.width * self.height * 4];
        self.heights.clear();
        self.dirty = true;
        true
    }

    pub fn update(&mut self, layout: &BarLayout, values: &[f32], color: Rgb) {
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
        if !changed && !self.dirty {
            return;
        }

        self.pixels.fill(0);
        let Rgb(r, g, b) = color;
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
