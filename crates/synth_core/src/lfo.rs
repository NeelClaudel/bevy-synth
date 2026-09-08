//! Low-frequency oscillator.
//!
//! Structurally similar to the audio oscillators but deliberately a separate
//! type: an LFO runs below 20 Hz where aliasing is irrelevant, so it skips
//! PolyBLEP entirely, and it needs shapes an audio oscillator does not have
//! (sample-and-hold, smoothed random).

use crate::rng::Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum LfoWave {
    #[default]
    Sine = 0,
    Triangle = 1,
    Saw = 2,
    Square = 3,
    /// Holds a new random value for a whole cycle. Steppy, classic.
    SampleHold = 4,
    /// Interpolates between random values. Drifting, organic.
    SmoothRandom = 5,
}

impl LfoWave {
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => LfoWave::Sine,
            1 => LfoWave::Triangle,
            2 => LfoWave::Saw,
            3 => LfoWave::Square,
            4 => LfoWave::SampleHold,
            5 => LfoWave::SmoothRandom,
            _ => LfoWave::Sine,
        }
    }

    pub const ALL: [LfoWave; 6] = [
        LfoWave::Sine,
        LfoWave::Triangle,
        LfoWave::Saw,
        LfoWave::Square,
        LfoWave::SampleHold,
        LfoWave::SmoothRandom,
    ];

    pub fn name(self) -> &'static str {
        match self {
            LfoWave::Sine => "Sine",
            LfoWave::Triangle => "Triangle",
            LfoWave::Saw => "Saw",
            LfoWave::Square => "Square",
            LfoWave::SampleHold => "S&H",
            LfoWave::SmoothRandom => "Random",
        }
    }
}

/// Where the LFO's output is routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum LfoTarget {
    #[default]
    None = 0,
    /// Filter cutoff, in octaves. The most useful destination by far.
    Cutoff = 1,
    /// Pitch, in semitones. Small depths give vibrato, large ones give sirens.
    Pitch = 2,
    /// Output level. Tremolo.
    Amplitude = 3,
    /// Pulse width of the square oscillators. The classic PWM string sound.
    PulseWidth = 4,
}

impl LfoTarget {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => LfoTarget::Cutoff,
            2 => LfoTarget::Pitch,
            3 => LfoTarget::Amplitude,
            4 => LfoTarget::PulseWidth,
            _ => LfoTarget::None,
        }
    }

    pub const ALL: [LfoTarget; 5] = [
        LfoTarget::None,
        LfoTarget::Cutoff,
        LfoTarget::Pitch,
        LfoTarget::Amplitude,
        LfoTarget::PulseWidth,
    ];

    pub fn name(self) -> &'static str {
        match self {
            LfoTarget::None => "Off",
            LfoTarget::Cutoff => "Cutoff",
            LfoTarget::Pitch => "Pitch",
            LfoTarget::Amplitude => "Amplitude",
            LfoTarget::PulseWidth => "Pulse Width",
        }
    }
}

/// A low-frequency oscillator producing bipolar output in `-1.0..=1.0`.
#[derive(Debug, Clone)]
pub struct Lfo {
    /// Phase in `[0, 1)`. `f64` for the same reason the clock uses it: an `f32`
    /// phase advanced 48000 times a second drifts audibly within seconds, and a
    /// tempo-synced LFO that drifts is worse than useless.
    phase: f64,
    dt: f64,
    sample_rate: f32,
    /// Current and previous random values, for S&H and smoothed random.
    current_random: f32,
    previous_random: f32,
    rng: Rng,
}

impl Lfo {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let a = rng.next_bipolar();
        let b = rng.next_bipolar();
        Self {
            phase: 0.0,
            dt: 0.0,
            sample_rate,
            current_random: a,
            previous_random: b,
            rng,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
    }

    /// Sets the LFO rate in Hz. Clamped to a musically sensible range: below
    /// 0.01 Hz it is indistinguishable from a static offset, and above 40 Hz it
    /// stops being a modulator and starts being an audio-rate oscillator that
    /// this code is not band-limited for.
    #[inline]
    pub fn set_rate(&mut self, hz: f32) {
        self.dt = (hz.clamp(0.01, 40.0) / self.sample_rate) as f64;
    }

    /// Restarts the cycle. Used when a patch retriggers the LFO per note, so
    /// every note gets the same modulation shape rather than catching the free
    /// running LFO at a random point.
    #[inline]
    pub fn retrigger(&mut self) {
        self.phase = 0.0;
    }

    #[inline]
    pub fn next(&mut self, wave: LfoWave) -> f32 {
        let t = self.phase as f32;
        let out = match wave {
            LfoWave::Sine => (t * core::f32::consts::TAU).sin(),
            LfoWave::Triangle => {
                // Up for the first half, down for the second.
                if t < 0.5 {
                    4.0 * t - 1.0
                } else {
                    3.0 - 4.0 * t
                }
            }
            LfoWave::Saw => 2.0 * t - 1.0,
            LfoWave::Square => {
                if t < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            LfoWave::SampleHold => self.current_random,
            LfoWave::SmoothRandom => {
                // Cosine interpolation between the last two random values:
                // smooth at both ends, unlike a linear ramp which corners.
                let s = 0.5 - 0.5 * (t * core::f32::consts::PI).cos();
                self.previous_random + (self.current_random - self.previous_random) * s
            }
        };

        self.phase += self.dt;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            // A cycle ended: draw the next random value for the random shapes.
            self.previous_random = self.current_random;
            self.current_random = self.rng.next_bipolar();
        }

        out
    }

    /// Advances the LFO by a whole block, returning the value at the block's
    /// start.
    ///
    /// Modulation is applied once per control block, so computing all 32
    /// intermediate values and throwing 31 away would be pure waste. The phase
    /// still advances by exactly the same amount, so this is not an
    /// approximation — the LFO runs at precisely the same rate either way.
    #[inline]
    pub fn next_block(&mut self, wave: LfoWave, samples: usize) -> f32 {
        let value = self.peek(wave);

        let advance = self.dt * samples as f64;
        self.phase += advance;
        // A block can span several cycles if the rate is high and the block
        // long, so draw a new random value for each wrap rather than only one.
        let mut wraps = 0;
        while self.phase >= 1.0 && wraps < 64 {
            self.phase -= 1.0;
            self.previous_random = self.current_random;
            self.current_random = self.rng.next_bipolar();
            wraps += 1;
        }
        if self.phase >= 1.0 {
            self.phase = self.phase.fract();
        }

        value
    }

    /// The current value, without advancing.
    #[inline]
    pub fn peek(&self, wave: LfoWave) -> f32 {
        let t = self.phase as f32;
        match wave {
            LfoWave::Sine => (t * core::f32::consts::TAU).sin(),
            LfoWave::Triangle => {
                if t < 0.5 {
                    4.0 * t - 1.0
                } else {
                    3.0 - 4.0 * t
                }
            }
            LfoWave::Saw => 2.0 * t - 1.0,
            LfoWave::Square => {
                if t < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            LfoWave::SampleHold => self.current_random,
            LfoWave::SmoothRandom => {
                let s = 0.5 - 0.5 * (t * core::f32::consts::PI).cos();
                self.previous_random + (self.current_random - self.previous_random) * s
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_is_bipolar_and_bounded() {
        for wave in LfoWave::ALL {
            let mut lfo = Lfo::new(48000.0, 11);
            lfo.set_rate(5.0);
            let mut min = f32::MAX;
            let mut max = f32::MIN;
            for _ in 0..48000 {
                let v = lfo.next(wave);
                assert!(v.is_finite());
                min = min.min(v);
                max = max.max(v);
            }
            assert!(min >= -1.001 && max <= 1.001, "{wave:?} ranged {min}..{max}");
            assert!(max - min > 0.5, "{wave:?} barely moved");
        }
    }

    #[test]
    fn rate_controls_cycles_per_second() {
        let mut lfo = Lfo::new(48000.0, 1);
        lfo.set_rate(4.0);
        let mut crossings = 0;
        let mut prev = lfo.next(LfoWave::Saw);
        // One sample past a full second. The fourth cycle completes exactly at
        // sample 48000, and a wrap is only observable on the sample *after* it
        // happens, so stopping at 48000 would legitimately see three.
        for _ in 0..48001 {
            let v = lfo.next(LfoWave::Saw);
            // A saw wraps once per cycle.
            if v < prev {
                crossings += 1;
            }
            prev = v;
        }
        assert_eq!(crossings, 4);
    }
}
