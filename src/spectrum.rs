use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::audio::{SAMPLE_RATE, WINDOW};

const LOW_HZ: f32 = 50.0;
const HIGH_HZ: f32 = 12_000.0;
/// Aceleración de caída, en alturas completas por segundo al cuadrado.
const GRAVITY: f32 = 4.0;
/// Constante de tiempo de subida, en segundos.
const RISE: f32 = 0.025;

struct Band {
    lo: usize,
    hi: usize,
    weight: f32,
}

/// FFT, bandas logarítmicas, sensibilidad automática y caída con gravedad.
pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    norm: f32,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    bands: Vec<Band>,
    raw: Vec<f32>,
    values: Vec<f32>,
    velocity: Vec<f32>,
    sens: f32,
}

impl Analyzer {
    pub fn new() -> Self {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(WINDOW);
        let window: Vec<f32> = (0..WINDOW)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * i as f32 / (WINDOW - 1) as f32;
                0.5 - 0.5 * phase.cos()
            })
            .collect();
        let norm = 2.0 / window.iter().sum::<f32>();
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        Self {
            fft,
            window,
            norm,
            buf: vec![Complex::default(); WINDOW],
            scratch,
            bands: Vec::new(),
            raw: Vec::new(),
            values: Vec::new(),
            velocity: Vec::new(),
            sens: 2.0,
        }
    }

    pub fn set_bars(&mut self, count: usize) {
        if count == self.bands.len() {
            return;
        }
        let bin_hz = SAMPLE_RATE as f32 / WINDOW as f32;
        let max_bin = WINDOW / 2;
        let span = (HIGH_HZ / LOW_HZ).ln();
        let mut lo = ((LOW_HZ / bin_hz) as usize).max(1);
        self.bands.clear();
        for i in 0..count {
            let edge = LOW_HZ * (span * (i + 1) as f32 / count as f32).exp();
            let hi = ((edge / bin_hz).round() as usize).max(lo + 1).min(max_bin);
            let start = lo.min(hi - 1);
            let center = (start + hi) as f32 * 0.5 * bin_hz;
            self.bands.push(Band {
                lo: start,
                hi,
                weight: (center / LOW_HZ).sqrt(),
            });
            lo = hi;
        }
        self.raw = vec![0.0; count];
        self.values = vec![0.0; count];
        self.velocity = vec![0.0; count];
    }

    /// Alturas entre 0 y 1, una por barra.
    pub fn process(&mut self, samples: &[f32], dt: f32) -> &[f32] {
        for ((slot, &sample), &w) in self.buf.iter_mut().zip(samples).zip(&self.window) {
            *slot = Complex::new(sample * w, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);

        let mut loudest = 0.0f32;
        for (band, raw) in self.bands.iter().zip(self.raw.iter_mut()) {
            let peak = self.buf[band.lo..band.hi]
                .iter()
                .map(|c| c.norm_sqr())
                .fold(0.0f32, f32::max);
            *raw = peak.sqrt() * self.norm * band.weight;
            loudest = loudest.max(*raw);
        }

        let scaled_peak = loudest * self.sens;
        if scaled_peak > 1.0 {
            self.sens *= (1.0 - 1.5 * dt).max(0.5);
        } else if loudest > 1e-5 {
            self.sens *= 1.0 + 0.3 * dt;
        }
        self.sens = self.sens.clamp(0.01, 1e5);

        let rise = 1.0 - (-dt / RISE).exp();
        for ((value, velocity), &raw) in self
            .values
            .iter_mut()
            .zip(self.velocity.iter_mut())
            .zip(&self.raw)
        {
            let target = (raw * self.sens).min(1.0);
            if target >= *value {
                *value += (target - *value) * rise;
                *velocity = 0.0;
            } else {
                *velocity += GRAVITY * dt;
                *value = (*value - *velocity * dt).max(target);
            }
        }
        &self.values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32) -> Vec<f32> {
        (0..WINDOW)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / SAMPLE_RATE as f32).sin() * 0.5)
            .collect()
    }

    #[test]
    fn bands_are_contiguous_and_non_empty() {
        let mut a = Analyzer::new();
        a.set_bars(60);
        for pair in a.bands.windows(2) {
            assert!(pair[0].hi <= pair[1].lo);
        }
        assert!(a.bands.iter().all(|b| b.hi > b.lo));
    }

    #[test]
    fn a_bass_tone_lights_the_left_bars() {
        let mut a = Analyzer::new();
        a.set_bars(40);
        let samples = tone(80.0);
        let mut values = Vec::new();
        for _ in 0..30 {
            values = a.process(&samples, 1.0 / 60.0).to_vec();
        }
        let left: f32 = values[..8].iter().cloned().fold(0.0, f32::max);
        let right: f32 = values[30..].iter().cloned().fold(0.0, f32::max);
        assert!(left > 0.5, "left {left}");
        assert!(right < 0.1, "right {right}");
    }

    #[test]
    fn bars_fall_back_down_in_silence() {
        let mut a = Analyzer::new();
        a.set_bars(20);
        let loud = tone(400.0);
        for _ in 0..20 {
            a.process(&loud, 1.0 / 60.0);
        }
        let silent = vec![0.0; WINDOW];
        for _ in 0..90 {
            a.process(&silent, 1.0 / 60.0);
        }
        assert!(a.values.iter().all(|&v| v == 0.0));
    }
}
