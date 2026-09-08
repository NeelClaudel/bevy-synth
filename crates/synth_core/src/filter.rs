//! State-variable filter, topology-preserving transform (TPT) form.
//!
//! # Why one filter gives you low, high, mid and notch
//!
//! An SVF computes all its outputs from the same two integrator states. Lowpass,
//! bandpass and highpass fall out of the same core simultaneously; picking a
//! "mode" is picking which tap you listen to. Notch and peak are sums of those
//! taps. So the whole low/high/mid/notch knob is one `match`, not four filters.
//!
//! # Why TPT and not a biquad
//!
//! A direct-form biquad has its coefficients baked in at design time. Sweep the
//! cutoff of one and you hear zipper noise and, at high resonance, brief bursts
//! of instability, because the state left over from the old coefficients is
//! wrong for the new ones. The TPT form (Zavalishin's zero-delay-feedback
//! structure, as popularised by Andy Simper) keeps the integrator states in
//! physical units, so they stay meaningful when the cutoff moves. You can
//! modulate it at audio rate with an envelope and it stays stable and musical —
//! which is the entire point of a synth filter.

/// Which output tap of the state-variable filter to listen to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum SvfMode {
    #[default]
    Lowpass = 0,
    Highpass = 1,
    /// The "mid" tap: passes a band around the cutoff, rejects either side.
    Bandpass = 2,
    /// Rejects a band around the cutoff, passes either side.
    Notch = 3,
    /// Flat with a resonant bump at the cutoff.
    Peak = 4,
    Bypass = 5,
}

impl SvfMode {
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => SvfMode::Lowpass,
            1 => SvfMode::Highpass,
            2 => SvfMode::Bandpass,
            3 => SvfMode::Notch,
            4 => SvfMode::Peak,
            5 => SvfMode::Bypass,
            _ => SvfMode::Lowpass,
        }
    }

    pub const ALL: [SvfMode; 6] = [
        SvfMode::Lowpass,
        SvfMode::Highpass,
        SvfMode::Bandpass,
        SvfMode::Notch,
        SvfMode::Peak,
        SvfMode::Bypass,
    ];

    pub fn name(self) -> &'static str {
        match self {
            SvfMode::Lowpass => "Lowpass",
            SvfMode::Highpass => "Highpass",
            SvfMode::Bandpass => "Bandpass",
            SvfMode::Notch => "Notch",
            SvfMode::Peak => "Peak",
            SvfMode::Bypass => "Bypass",
        }
    }
}

/// A 12 dB/octave state-variable filter.
///
/// Cascade two for 24 dB/octave — see [`Filter`].
#[derive(Debug, Clone)]
pub struct Svf {
    sample_rate: f32,
    /// Prewarped cutoff: `tan(pi * fc / fs)`.
    g: f32,
    /// Damping, the reciprocal of Q. Small `k` means high resonance.
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    /// The two integrator states.
    ic1eq: f32,
    ic2eq: f32,
}

impl Svf {
    pub fn new(sample_rate: f32) -> Self {
        let mut f = Self {
            sample_rate,
            g: 0.0,
            k: 2.0,
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
            ic1eq: 0.0,
            ic2eq: 0.0,
        };
        f.set_params(1000.0, 0.0);
        f
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
    }

    /// Clears the integrator states. Call when a voice is reused, or the tail of
    /// the previous note bleeds into the attack of the next one.
    #[inline]
    pub fn reset(&mut self) {
        self.ic1eq = 0.0;
        self.ic2eq = 0.0;
    }

    /// Sets cutoff in Hz and resonance in `0.0..=1.0`.
    ///
    /// Resonance maps to damping as `k = 2 - 2*res`, so 0.0 is maximally damped
    /// (Q = 0.5, no peak at all) and 1.0 approaches self-oscillation. `k` is
    /// clamped just above zero: at exactly zero the filter is a pure oscillator
    /// with no loss and any DC in the signal path grows without bound.
    #[inline]
    pub fn set_params(&mut self, cutoff_hz: f32, resonance: f32) {
        // Prewarping blows up as the cutoff approaches Nyquist, so stop short.
        let nyq = self.sample_rate * 0.5;
        let fc = cutoff_hz.clamp(10.0, nyq * 0.98);
        let g = (core::f32::consts::PI * fc / self.sample_rate).tan();
        let k = (2.0 - 2.0 * resonance.clamp(0.0, 1.0)).max(0.025);

        self.g = g;
        self.k = k;
        self.a1 = 1.0 / (1.0 + g * (g + k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    /// Processes one sample and returns every tap at once.
    #[inline]
    pub fn process(&mut self, input: f32) -> SvfOutputs {
        let v3 = input - self.ic2eq;
        let v1 = self.a1 * self.ic1eq + self.a2 * v3;
        let v2 = self.ic2eq + self.a2 * self.ic1eq + self.a3 * v3;

        self.ic1eq = 2.0 * v1 - self.ic1eq;
        self.ic2eq = 2.0 * v2 - self.ic2eq;

        // Denormals in the integrator states cost 100x on some CPUs, and a
        // decaying resonant filter produces them constantly. Flush to zero.
        if self.ic1eq.abs() < 1e-20 {
            self.ic1eq = 0.0;
        }
        if self.ic2eq.abs() < 1e-20 {
            self.ic2eq = 0.0;
        }

        let low = v2;
        let band = v1;
        let high = input - self.k * v1 - v2;

        SvfOutputs {
            low,
            band,
            high,
            notch: high + low,
            peak: high - low,
            input,
        }
    }

    /// Processes one sample and returns only the selected tap.
    #[inline]
    pub fn process_mode(&mut self, input: f32, mode: SvfMode) -> f32 {
        if mode == SvfMode::Bypass {
            return input;
        }
        self.process(input).select(mode)
    }
}

/// Every simultaneous output of a state-variable filter.
#[derive(Debug, Clone, Copy)]
pub struct SvfOutputs {
    pub low: f32,
    pub band: f32,
    pub high: f32,
    pub notch: f32,
    pub peak: f32,
    input: f32,
}

impl SvfOutputs {
    #[inline]
    pub fn select(self, mode: SvfMode) -> f32 {
        match mode {
            SvfMode::Lowpass => self.low,
            SvfMode::Highpass => self.high,
            SvfMode::Bandpass => self.band,
            SvfMode::Notch => self.notch,
            SvfMode::Peak => self.peak,
            SvfMode::Bypass => self.input,
        }
    }
}

/// Filter slope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum Slope {
    /// One SVF. Gentle, vocal, the classic "state variable" character.
    #[default]
    Db12 = 0,
    /// Two SVFs in series. Steeper and darker, closer to a ladder filter.
    Db24 = 1,
}

impl Slope {
    pub fn from_u32(v: u32) -> Self {
        if v == 1 {
            Slope::Db24
        } else {
            Slope::Db12
        }
    }
}

/// The filter as a voice actually uses it: a mode, a slope and two cascaded SVFs.
#[derive(Debug, Clone)]
pub struct Filter {
    stage1: Svf,
    stage2: Svf,
    pub mode: SvfMode,
    pub slope: Slope,
}

impl Filter {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            stage1: Svf::new(sample_rate),
            stage2: Svf::new(sample_rate),
            mode: SvfMode::Lowpass,
            slope: Slope::Db12,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.stage1.set_sample_rate(sample_rate);
        self.stage2.set_sample_rate(sample_rate);
    }

    pub fn reset(&mut self) {
        self.stage1.reset();
        self.stage2.reset();
    }

    #[inline]
    pub fn set_params(&mut self, cutoff_hz: f32, resonance: f32) {
        self.stage1.set_params(cutoff_hz, resonance);
        // The second stage gets less resonance. Two resonant stages in series
        // multiply their peaks, which turns a pleasant emphasis into a howl at
        // the same knob position.
        self.stage2.set_params(cutoff_hz, resonance * 0.5);
    }

    #[inline]
    pub fn process(&mut self, input: f32) -> f32 {
        let out = self.stage1.process_mode(input, self.mode);
        match self.slope {
            Slope::Db12 => out,
            Slope::Db24 => self.stage2.process_mode(out, self.mode),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measures the filter's gain at a given frequency by running a sine
    /// through it and taking the peak of the steady state.
    fn gain_at(mode: SvfMode, cutoff: f32, res: f32, probe_hz: f32) -> f32 {
        let sr = 48000.0;
        let mut f = Svf::new(sr);
        f.set_params(cutoff, res);
        let mut phase = 0.0f32;
        let inc = probe_hz / sr;
        let mut peak: f32 = 0.0;
        // Settle first, then measure.
        for i in 0..24000 {
            let x = (phase * core::f32::consts::TAU).sin();
            phase = (phase + inc) % 1.0;
            let y = f.process_mode(x, mode);
            if i > 12000 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn lowpass_passes_low_and_stops_high() {
        assert!(gain_at(SvfMode::Lowpass, 1000.0, 0.0, 100.0) > 0.9);
        assert!(gain_at(SvfMode::Lowpass, 1000.0, 0.0, 10000.0) < 0.1);
    }

    #[test]
    fn highpass_is_the_mirror_image() {
        assert!(gain_at(SvfMode::Highpass, 1000.0, 0.0, 100.0) < 0.1);
        assert!(gain_at(SvfMode::Highpass, 1000.0, 0.0, 10000.0) > 0.9);
    }

    #[test]
    fn bandpass_peaks_at_cutoff() {
        let at_center = gain_at(SvfMode::Bandpass, 1000.0, 0.7, 1000.0);
        assert!(at_center > gain_at(SvfMode::Bandpass, 1000.0, 0.7, 100.0));
        assert!(at_center > gain_at(SvfMode::Bandpass, 1000.0, 0.7, 10000.0));
    }

    #[test]
    fn notch_rejects_at_cutoff() {
        assert!(gain_at(SvfMode::Notch, 1000.0, 0.0, 1000.0) < 0.2);
        assert!(gain_at(SvfMode::Notch, 1000.0, 0.0, 100.0) > 0.8);
        assert!(gain_at(SvfMode::Notch, 1000.0, 0.0, 10000.0) > 0.8);
    }

    #[test]
    fn resonance_lifts_the_corner() {
        let flat = gain_at(SvfMode::Lowpass, 1000.0, 0.0, 1000.0);
        let resonant = gain_at(SvfMode::Lowpass, 1000.0, 0.9, 1000.0);
        assert!(resonant > flat * 3.0, "flat {flat}, resonant {resonant}");
    }

    /// The whole reason for choosing TPT over a biquad: sweeping the cutoff
    /// fast at high resonance must not blow up.
    #[test]
    fn survives_violent_cutoff_modulation() {
        let sr = 48000.0;
        let mut f = Svf::new(sr);
        let mut osc_phase = 0.0f32;
        let mut peak: f32 = 0.0;
        for i in 0..480_000 {
            // Sweep the cutoff across the whole range every few milliseconds.
            let t = i as f32 / sr;
            let cutoff = 200.0 + 8000.0 * (t * 200.0).sin().abs();
            f.set_params(cutoff, 0.98);
            let x = (osc_phase * core::f32::consts::TAU).sin();
            osc_phase = (osc_phase + 220.0 / sr) % 1.0;
            let y = f.process_mode(x, SvfMode::Lowpass);
            assert!(y.is_finite(), "filter produced {y} at sample {i}");
            peak = peak.max(y.abs());
        }
        assert!(peak < 50.0, "filter rang up to {peak}");
    }

    #[test]
    fn twenty_four_db_is_steeper_than_twelve() {
        let sr = 48000.0;
        let measure = |slope: Slope| {
            let mut f = Filter::new(sr);
            f.slope = slope;
            f.set_params(1000.0, 0.0);
            let mut phase = 0.0f32;
            let mut peak: f32 = 0.0;
            for i in 0..24000 {
                let x = (phase * core::f32::consts::TAU).sin();
                phase = (phase + 4000.0 / sr) % 1.0;
                let y = f.process(x);
                if i > 12000 {
                    peak = peak.max(y.abs());
                }
            }
            peak
        };
        assert!(measure(Slope::Db24) < measure(Slope::Db12));
    }
}
