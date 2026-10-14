use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

pub const SAMPLE_RATE: u32 = 44_100;
pub const WINDOW: usize = 2048;

const RING: usize = 4096;
const CHUNK: usize = 512;
/// PipeWire deja de entregar muestras cuando la salida se suspende, así que
/// datos más viejos que esto se tratan como silencio.
const STALE: Duration = Duration::from_millis(150);

struct Ring {
    data: Vec<f32>,
    pos: usize,
    last: Option<Instant>,
}

#[derive(Clone)]
pub struct AudioTap {
    ring: Arc<Mutex<Ring>>,
}

impl AudioTap {
    pub fn start() -> Self {
        let ring = Arc::new(Mutex::new(Ring {
            data: vec![0.0; RING],
            pos: 0,
            last: None,
        }));
        let shared = Arc::clone(&ring);
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || capture_loop(&shared))
            .expect("no se pudo crear el hilo de audio");
        Self { ring }
    }

    /// Copia las últimas `out.len()` muestras. Devuelve false si no hay audio reciente.
    pub fn latest(&self, out: &mut [f32]) -> bool {
        let ring = self.ring.lock().unwrap();
        let fresh = ring.last.is_some_and(|t| t.elapsed() < STALE);
        if !fresh {
            out.fill(0.0);
            return false;
        }
        let n = out.len().min(RING);
        let start = ring.pos + RING - n;
        for (i, sample) in out.iter_mut().take(n).enumerate() {
            *sample = ring.data[(start + i) % RING];
        }
        true
    }
}

fn capture_loop(ring: &Mutex<Ring>) {
    loop {
        // Solo vuelve si la conexión se cae. Se reintenta para seguir al
        // dispositivo por defecto si cambia (por ejemplo, audífonos Bluetooth).
        let _ = capture(ring);
        thread::sleep(Duration::from_secs(1));
    }
}

fn capture(ring: &Mutex<Ring>) -> Result<(), String> {
    let spec = Spec {
        format: Format::F32le,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: (CHUNK * 4) as u32,
    };
    let stream = Simple::new(
        None,
        "lyrics",
        Direction::Record,
        Some("@DEFAULT_MONITOR@"),
        "spectrum",
        &spec,
        None,
        Some(&attr),
    )
    .map_err(|e| format!("PulseAudio: {e:?}"))?;

    let mut bytes = vec![0u8; CHUNK * 4];
    loop {
        stream
            .read(&mut bytes)
            .map_err(|e| format!("PulseAudio: {e:?}"))?;
        let mut ring = ring.lock().unwrap();
        for raw in bytes.as_chunks::<4>().0 {
            let pos = ring.pos;
            ring.data[pos] = f32::from_le_bytes(*raw);
            ring.pos = (pos + 1) % RING;
        }
        ring.last = Some(Instant::now());
    }
}
