//! Input level meter (SPEC 7.1 receive side, 13.5): RMS about the mean,
//! peak, DC offset and clipping, accumulated off the audio thread.

/// A sample at or above this magnitude counts as clipped (an i16 input
/// reaches 32767/32768; the simulator's bench uses the same 0.999).
pub const CLIP_LEVEL: f32 = 0.999;

/// Accumulates statistics until [`LevelMeter::take`].
#[derive(Clone, Debug, Default)]
pub struct LevelMeter {
    n: u64,
    sum: f64,
    sumsq: f64,
    peak: f32,
    clipped: u64,
}

/// One window's levels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Levels {
    /// Samples measured.
    pub samples: u64,
    /// RMS about the mean, dBFS (full-scale sine is -3 dBFS).
    pub rms_dbfs: f64,
    /// RMS including the DC offset, dBFS.
    pub rms_total_dbfs: f64,
    /// Largest magnitude, dBFS.
    pub peak_dbfs: f64,
    /// Mean, full scale 1.
    pub dc: f64,
    /// Samples at or above [`CLIP_LEVEL`].
    pub clipped: u64,
}

/// Amplitude to dBFS (floored at -200).
pub fn dbfs(a: f64) -> f64 {
    20.0 * a.max(1e-10).log10()
}

impl Levels {
    /// The receive path clipped (SPEC 7.1: flag it to the operator).
    pub fn clipping(&self) -> bool {
        self.clipped > 0
    }

    /// Peaks above -3 dBFS, the top of the receive range (SPEC 7.1, 13.5),
    /// with 0.1 dB for 16-bit rounding of a waveform peaking at exactly -3.
    pub fn hot(&self) -> bool {
        self.peak_dbfs > -2.9
    }

    /// A short status word: CLIP, HOT or ok.
    pub fn status(&self) -> &'static str {
        if self.clipping() {
            "CLIP"
        } else if self.hot() {
            "HOT"
        } else {
            "ok"
        }
    }
}

impl LevelMeter {
    /// An empty meter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add samples (full scale 1).
    pub fn push(&mut self, x: &[f32]) {
        for &v in x {
            let a = v.abs();
            self.peak = self.peak.max(a);
            if a >= CLIP_LEVEL {
                self.clipped += 1;
            }
            self.sum += v as f64;
            self.sumsq += (v as f64) * (v as f64);
        }
        self.n += x.len() as u64;
    }

    /// Add f64 samples.
    pub fn push_f64(&mut self, x: &[f64]) {
        for &v in x {
            let a = v.abs() as f32;
            self.peak = self.peak.max(a);
            if a >= CLIP_LEVEL {
                self.clipped += 1;
            }
            self.sum += v;
            self.sumsq += v * v;
        }
        self.n += x.len() as u64;
    }

    /// Samples since the last take.
    pub fn samples(&self) -> u64 {
        self.n
    }

    /// The levels so far, and reset.
    pub fn take(&mut self) -> Levels {
        let n = self.n.max(1) as f64;
        let dc = self.sum / n;
        let var = (self.sumsq / n - dc * dc).max(0.0);
        let l = Levels {
            samples: self.n,
            rms_dbfs: dbfs(var.sqrt()),
            rms_total_dbfs: dbfs((self.sumsq / n).sqrt()),
            peak_dbfs: dbfs(self.peak as f64),
            dc,
            clipped: self.clipped,
        };
        *self = Self::default();
        l
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_levels() {
        let mut m = LevelMeter::new();
        let x: Vec<f32> = (0..48_000).map(|i| 0.1 + 0.5 * (i as f32 * 0.1309).sin()).collect();
        m.push(&x);
        let l = m.take();
        assert!((l.rms_dbfs - dbfs(0.5 / 2f64.sqrt())).abs() < 0.05, "{l:?}");
        assert!((l.peak_dbfs - dbfs(0.6)).abs() < 0.05);
        assert!((l.dc - 0.1).abs() < 1e-3);
        assert_eq!(l.status(), "ok");
        m.push(&[1.0, -1.0, 0.0]);
        let l = m.take();
        assert_eq!(l.clipped, 2);
        assert_eq!(l.status(), "CLIP");
        assert_eq!(m.samples(), 0);
    }
}
