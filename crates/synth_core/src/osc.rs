//! Band-limited oscillators.
//!
//! # Why not just `2.0 * phase - 1.0`?
//!
//! A naive saw or square has infinitely many harmonics. Every harmonic above
//! Nyquist folds back down into the audible band at a frequency that is *not*
//! musically related to the note, so the sound gets a metallic shimmer that
//! changes pitch in the wrong direction as you play up the keyboard. It is the
//! single most common reason a hand-written synth sounds cheap.
//!
//! # PolyBLEP
//!
//! The fix here is PolyBLEP: a polynomial approximation of a band-limited step.
//! A saw is a ramp with one discontinuity per cycle; a square has two. Around
//! each discontinuity we add a small correction shaped like the difference
//! between a hard step and a band-limited one. Two samples wide, a handful of
//! multiplies, and the worst of the aliasing is gone.
//!
//! It is not perfect — a wavetable with per-octave mipmaps is cleaner up high —
//! but it costs almost nothing, needs no tables, and handles arbitrary
//! modulation of frequency and pulse width, which wavetables make awkward.

use crate::rng::Rng;

/// Oscillator waveform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum Waveform {
    Sine = 0,
    Triangle = 1,
    #[default]
    Saw = 2,
    /// Square wave with variable pulse width (see [`Oscillator::set_pulse_width`]).
    Pulse = 3,
    /// White noise. Ignores frequency.
    Noise = 4,
}

impl Waveform {
    /// Round-trips a waveform through an atomic parameter. Unknown values clamp
    /// to [`Waveform::Saw`] rather than panicking, because this runs on the
    /// audio thread.
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => Waveform::Sine,
            1 => Waveform::Triangle,
            2 => Waveform::Saw,
            3 => Waveform::Pulse,
            4 => Waveform::Noise,
            _ => Waveform::Saw,
        }
    }

    pub const ALL: [Waveform; 5] = [
        Waveform::Sine,
        Waveform::Triangle,
        Waveform::Saw,
        Waveform::Pulse,
        Waveform::Noise,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Waveform::Sine => "Sine",
            Waveform::Triangle => "Triangle",
            Waveform::Saw => "Saw",
            Waveform::Pulse => "Pulse",
            Waveform::Noise => "Noise",
        }
    }
}

/// A single band-limited oscillator.
#[derive(Debug, Clone)]
pub struct Oscillator {
    phase: f32,
    /// Phase increment per sample, i.e. `freq / sample_rate`. Also doubles as
    /// the PolyBLEP width, which is why it is stored rather than recomputed.
    dt: f32,
    sample_rate: f32,
    pulse_width: f32,
    /// Leaky-integrator state for the triangle wave.
    tri: f32,
    rng: Rng,
}

impl Oscillator {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        Self {
            phase: 0.0,
            dt: 0.0,
            sample_rate,
            pulse_width: 0.5,
            tri: 0.0,
            rng: Rng::new(seed),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
    }

    /// Sets the oscillator frequency in Hz.
    ///
    /// Clamped below Nyquist: PolyBLEP correction assumes at least two samples
    /// per cycle, and letting `dt` reach 0.5 makes the correction windows
    /// overlap and the output blow up.
    #[inline]
    pub fn set_freq(&mut self, hz: f32) {
        let nyq = self.sample_rate * 0.5;
        let hz = hz.clamp(0.0, nyq * 0.98);
        self.dt = hz / self.sample_rate;
    }

    /// Pulse width for [`Waveform::Pulse`], as a duty cycle.
    ///
    /// Clamped away from 0 and 1: at the extremes the two step corrections
    /// collide and the wave collapses to silence with a click.
    #[inline]
    pub fn set_pulse_width(&mut self, pw: f32) {
        self.pulse_width = pw.clamp(0.05, 0.95);
    }

    /// Resets phase to the start of the cycle. Used for oscillator sync and for
    /// giving percussive patches a consistent attack transient.
    #[inline]
    pub fn reset_phase(&mut self) {
        self.phase = 0.0;
        self.tri = 0.0;
    }

    /// Offsets the starting phase, in cycles. Detuned oscillators that start in
    /// phase produce an audible whoosh as they drift apart; a random offset per
    /// voice avoids it.
    #[inline]
    pub fn set_phase(&mut self, phase: f32) {
        self.phase = phase.rem_euclid(1.0);
    }

    /// Produces one sample and advances the phase.
    #[inline]
    pub fn next(&mut self, wave: Waveform) -> f32 {
        // Noise has no phase to advance and no aliasing to correct.
        if wave == Waveform::Noise {
            return self.rng.next_bipolar();
        }

        let t = self.phase;
        let dt = self.dt;

        let out = match wave {
            Waveform::Sine => (t * core::f32::consts::TAU).sin(),

            Waveform::Saw => {
                // Naive ramp, minus a band-limited step at the wrap point.
                let naive = 2.0 * t - 1.0;
                naive - blep_at(t, 0.0, dt)
            }

            Waveform::Pulse => {
                let pw = self.pulse_width;
                let naive = if t < pw { 1.0 } else { -1.0 };
                // Two discontinuities per cycle: a rising edge at phase 0 and a
                // falling edge at `pw`. Narrow the correction so the two can
                // never overlap, which would let them sum past full scale at
                // extreme frequencies or duty cycles.
                let width = dt.min(pw * 0.5).min((1.0 - pw) * 0.5);
                naive + blep_at(t, 0.0, width) - blep_at(t, pw, width)
            }

            Waveform::Triangle => {
                // A triangle is the integral of a square. Integrating the
                // *band-limited* square inherits its band limiting for free,
                // and the leak term stops DC offset from accumulating.
                let naive = if t < 0.5 { 1.0 } else { -1.0 };
                let width = dt.min(0.25);
                let sq = naive + blep_at(t, 0.0, width) - blep_at(t, 0.5, width);
                self.tri = dt * sq + (1.0 - dt) * self.tri;
                // The integrator's output swings +/-0.25 for a unit square.
                self.tri * 4.0
            }

            Waveform::Noise => unreachable!(),
        };

        self.phase += dt;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }

        out
    }
}

/// Polynomial band-limited step, positioned relative to a discontinuity.
///
/// `t` is the current phase in `[0, 1)`, `edge` the phase at which the waveform
/// jumps, and `dt` the phase increment (which is also the correction's width).
/// The result is non-zero only within one `dt` either side of the edge.
///
/// # Why this takes an edge instead of a pre-wrapped phase
///
/// The obvious formulation computes `(t + 1.0 - pw) % 1.0` and feeds that to a
/// blep defined on `[0, 1)`. It has a nasty failure mode: when `t` is a hair
/// below `pw`, the sum rounds up to exactly `1.0` in `f32`, the modulo sends it
/// to `0.0`, and the correction is applied to the *wrong side* of the step —
/// yielding a sample of +2.0 in the middle of a wave that should never leave
/// +/-1. Working from a signed distance instead keeps the subtraction exact
/// (Sterbenz: `t - edge` is exact when they are within a factor of two) and the
/// branch on the correct side by construction.
#[inline]
fn blep_at(t: f32, edge: f32, dt: f32) -> f32 {
    if dt <= 0.0 {
        return 0.0;
    }

    // Signed distance to the edge, wrapped into [-0.5, 0.5).
    let mut d = t - edge;
    if d < -0.5 {
        d += 1.0;
    } else if d >= 0.5 {
        d -= 1.0;
    }

    if d >= 0.0 {
        if d < dt {
            // Just after the step.
            let x = d / dt;
            x + x - x * x - 1.0
        } else {
            0.0
        }
    } else if d > -dt {
        // Just before the step.
        let x = d / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every waveform must stay inside a sane range. A broken PolyBLEP shows up
    /// here immediately as an explosion at high frequencies.
    #[test]
    fn waveforms_stay_bounded() {
        for wave in Waveform::ALL {
            for &freq in &[20.0, 440.0, 4000.0, 12000.0, 20000.0] {
                let mut osc = Oscillator::new(48000.0, 1);
                osc.set_freq(freq);
                for _ in 0..48000 {
                    let s = osc.next(wave);
                    assert!(
                        s.is_finite() && s.abs() < 2.0,
                        "{wave:?} at {freq} Hz produced {s}"
                    );
                }
            }
        }
    }

    /// The point of PolyBLEP: a high saw should have far less energy above
    /// Nyquist-fold than a naive one. We measure it indirectly by comparing
    /// sample-to-sample jumps, which is where aliasing energy lives.
    #[test]
    fn polyblep_softens_the_discontinuity() {
        let mut osc = Oscillator::new(48000.0, 1);
        osc.set_freq(2000.0);
        let mut worst_blep: f32 = 0.0;
        let mut prev = osc.next(Waveform::Saw);
        for _ in 0..4800 {
            let s = osc.next(Waveform::Saw);
            worst_blep = worst_blep.max((s - prev).abs());
            prev = s;
        }
        // A naive saw at 2 kHz jumps the full 2.0 range in one sample.
        assert!(worst_blep < 1.6, "jump was {worst_blep}, blep not applied");
    }

    #[test]
    fn triangle_has_no_dc_drift() {
        let mut osc = Oscillator::new(48000.0, 1);
        osc.set_freq(110.0);
        // Discard the integrator's start-up transient.
        for _ in 0..4800 {
            osc.next(Waveform::Triangle);
        }
        let mut sum = 0.0f64;
        for _ in 0..48000 {
            sum += osc.next(Waveform::Triangle) as f64;
        }
        assert!((sum / 48000.0).abs() < 0.01, "DC offset {}", sum / 48000.0);
    }
}
