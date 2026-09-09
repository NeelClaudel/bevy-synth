//! Eight synthesized percussion voices sharing one one-shot envelope shape.

use crate::note::cents_to_ratio;
use crate::rng::Rng;

/// Pads in the rack. Fixed at eight: the grid publishes one pad per bit
/// nibble, and eight pads fit a `u32` column with room to spare.
pub const PAD_COUNT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    Kick,
    Snare,
    ClosedHat,
    OpenHat,
    Clap,
    LowTom,
    HighTom,
    Rim,
}

impl Pad {
    pub const ALL: [Pad; PAD_COUNT] = [
        Pad::Kick,
        Pad::Snare,
        Pad::ClosedHat,
        Pad::OpenHat,
        Pad::Clap,
        Pad::LowTom,
        Pad::HighTom,
        Pad::Rim,
    ];

    /// Recovers a pad from its index. Out-of-range values fall back to the
    /// kick rather than panicking — this runs on the audio thread, where a
    /// wrong drum is survivable and a panic is not.
    pub fn from_u32(index: u32) -> Pad {
        Pad::ALL.get(index as usize).copied().unwrap_or(Pad::Kick)
    }

    pub fn name(self) -> &'static str {
        match self {
            Pad::Kick => "KICK",
            Pad::Snare => "SNARE",
            Pad::ClosedHat => "HAT",
            Pad::OpenHat => "OPEN",
            Pad::Clap => "CLAP",
            Pad::LowTom => "TOM L",
            Pad::HighTom => "TOM H",
            Pad::Rim => "RIM",
        }
    }

    /// The pad's untuned centre frequency. Noise-based pads read this as the
    /// centre of their band-pass rather than a pitch.
    pub fn base_hz(self) -> f32 {
        match self {
            Pad::Kick => 50.0,
            Pad::Snare => 180.0,
            Pad::ClosedHat => 8_000.0,
            Pad::OpenHat => 8_000.0,
            Pad::Clap => 1_200.0,
            Pad::LowTom => 100.0,
            Pad::HighTom => 180.0,
            Pad::Rim => 800.0,
        }
    }

    /// Untuned decay in seconds, before the per-pad decay scale.
    pub fn base_decay(self) -> f32 {
        match self {
            Pad::Kick => 0.34,
            Pad::Snare => 0.19,
            Pad::ClosedHat => 0.045,
            Pad::OpenHat => 0.38,
            Pad::Clap => 0.22,
            Pad::LowTom => 0.40,
            Pad::HighTom => 0.30,
            Pad::Rim => 0.028,
        }
    }
}

/// A one-shot exponential fall. `Adsr` waits for a note-off; a drum has none,
/// so this starts at `level` and runs to silence on its own.
#[derive(Debug, Clone, Copy)]
pub struct Decay {
    value: f32,
    coef: f32,
}

impl Decay {
    pub fn new() -> Decay {
        Decay {
            value: 0.0,
            coef: 0.0,
        }
    }

    /// Restarts the fall. `seconds` is the time to reach -60 dB, the usual
    /// definition of a decay time; a strike during a ring simply replaces the
    /// old value, which is what a re-hit sounds like.
    pub fn trigger(&mut self, seconds: f32, sample_rate: f32, level: f32) {
        let samples = (seconds.max(0.001) * sample_rate).max(1.0);
        // ln(0.001) — one thousandth of the peak, i.e. -60 dB.
        self.coef = (-6.907_755 / samples).exp();
        self.value = level;
    }

    /// Returns the current level, then falls. Below -100 dB the value is
    /// flushed to exactly zero so a finished voice reports itself silent and
    /// stops feeding denormals into the mix.
    // Not `Iterator::next`, for the same reason as `Adsr::next`: this is a
    // signal generator, not a sequence that can run out.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> f32 {
        let out = self.value;
        self.value *= self.coef;
        if self.value < 1.0e-5 {
            self.value = 0.0;
        }
        out
    }

    pub fn is_active(&self) -> bool {
        self.value > 0.0
    }

    pub fn silence(&mut self) {
        self.value = 0.0;
    }
}

impl Default for Decay {
    fn default() -> Decay {
        Decay::new()
    }
}

/// One pad's worth of state. A voice is monophonic: striking it while it
/// rings restarts it, which is exactly how a real drum machine behaves.
#[derive(Debug, Clone)]
pub struct DrumVoice {
    sample_rate: f32,
    pad: Pad,
    rng: Rng,

    /// Body oscillator: phase in turns, so wrapping is a subtraction.
    phase: f32,
    hz: f32,

    /// Amplitude of the body tone.
    amp: Decay,
    /// Pitch envelope: multiplies `hz` on its way to the oscillator, giving
    /// the kick its downward thump.
    pitch: Decay,
    /// The short noise transient at the head of the hit.
    click: Decay,
}

impl DrumVoice {
    pub fn new(sample_rate: f32, seed: u64) -> DrumVoice {
        DrumVoice {
            sample_rate: sample_rate.max(1.0),
            pad: Pad::Kick,
            rng: Rng::new(seed),
            phase: 0.0,
            hz: Pad::Kick.base_hz(),
            amp: Decay::new(),
            pitch: Decay::new(),
            click: Decay::new(),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.silence();
    }

    /// Fires the pad. `gain` is the strike level, `tune_semitones` shifts the
    /// pitch, and `decay_scale` stretches or shortens the tail.
    pub fn strike(&mut self, pad: Pad, gain: f32, tune_semitones: f32, decay_scale: f32) {
        self.pad = pad;
        self.hz = pad.base_hz() * cents_to_ratio(tune_semitones * 100.0);
        self.phase = 0.0;

        let decay = pad.base_decay() * decay_scale.clamp(0.1, 4.0);
        self.amp.trigger(decay, self.sample_rate, gain);
        // 50 ms of pitch bend: the drop is what makes it read as a kick
        // rather than a bass note.
        self.pitch.trigger(0.05, self.sample_rate, 1.0);
        self.click.trigger(0.002, self.sample_rate, gain * 0.5);
    }

    // Not `Iterator::next`, for the same reason as `Adsr::next`: this is a
    // signal generator, not a sequence that can run out.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> f32 {
        if !self.is_active() {
            return 0.0;
        }

        // The pitch envelope runs 1 -> 0, and the body starts four times up.
        let bend = 1.0 + self.pitch.next() * 3.0;
        let step = (self.hz * bend / self.sample_rate).clamp(0.0, 0.49);
        self.phase += step;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }

        let body = sine_turns(self.phase) * self.amp.next();
        let click = self.rng.next_bipolar() * self.click.next();
        body + click
    }

    pub fn is_silent(&self) -> bool {
        !self.is_active()
    }

    fn is_active(&self) -> bool {
        self.amp.is_active() || self.click.is_active()
    }

    pub fn silence(&mut self) {
        self.amp.silence();
        self.pitch.silence();
        self.click.silence();
        self.phase = 0.0;
    }
}

/// Sine of a phase expressed in turns (0..1), via the standard library.
/// `synth_core` has no dependencies but is not `no_std`, so this is free.
fn sine_turns(phase: f32) -> f32 {
    (phase * core::f32::consts::TAU).sin()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drum is an event, not a note: it has to start loud on its own and
    /// stop on its own, with nothing sending it a note-off.
    #[test]
    fn a_struck_kick_sounds_and_then_stops() {
        let mut v = DrumVoice::new(48_000.0, 1);
        assert!(v.is_silent());

        v.strike(Pad::Kick, 1.0, 0.0, 1.0);
        assert!(!v.is_silent());

        let mut peak = 0.0f32;
        for _ in 0..4_800 {
            peak = peak.max(v.next().abs());
        }
        assert!(peak > 0.2, "a kick that quiet is not a kick: {peak}");

        // The longest pad decays well inside a second; two is generous.
        for _ in 0..96_000 {
            v.next();
        }
        assert!(v.is_silent(), "the voice never released");
        assert_eq!(v.next(), 0.0);
    }

    /// Tuning has to reach the oscillator, not just the parameter store.
    #[test]
    fn tuning_moves_the_pitch() {
        let mut low = DrumVoice::new(48_000.0, 1);
        let mut high = DrumVoice::new(48_000.0, 1);
        low.strike(Pad::Kick, 1.0, 0.0, 1.0);
        high.strike(Pad::Kick, 1.0, 12.0, 1.0);

        // An octave up crosses zero twice as often. Counting sign changes
        // avoids an FFT and is exactly the property that matters.
        let mut low_crossings = 0;
        let mut high_crossings = 0;
        let (mut last_low, mut last_high) = (0.0f32, 0.0f32);
        for _ in 0..4_800 {
            let (l, h) = (low.next(), high.next());
            if l * last_low < 0.0 {
                low_crossings += 1;
            }
            if h * last_high < 0.0 {
                high_crossings += 1;
            }
            last_low = l;
            last_high = h;
        }
        assert!(
            high_crossings > low_crossings,
            "tuning up did not raise the pitch: {high_crossings} vs {low_crossings}"
        );
    }

    /// The drop is the kick. Its body starts four times up and falls to base
    /// over 50 ms, so an early window of the same voice has to cross zero more
    /// often than a late one. Counting crossings measures the pitch envelope
    /// without an FFT, exactly as the tuning test does.
    #[test]
    fn the_kick_pitch_falls_over_its_first_fifty_milliseconds() {
        fn crossings(v: &mut DrumVoice, samples: usize) -> usize {
            let mut count = 0;
            let mut last = 0.0f32;
            for _ in 0..samples {
                let s = v.next();
                if s * last < 0.0 {
                    count += 1;
                }
                last = s;
            }
            count
        }

        let mut v = DrumVoice::new(48_000.0, 1);
        v.strike(Pad::Kick, 1.0, 0.0, 1.0);

        // The first 10 ms, then the 40-50 ms window, with the stretch between
        // them rendered and thrown away.
        let early = crossings(&mut v, 480);
        let _ = crossings(&mut v, 1_440);
        let late = crossings(&mut v, 480);

        assert!(
            early > late,
            "the kick's pitch did not fall: {early} crossings then {late}"
        );
    }
}
