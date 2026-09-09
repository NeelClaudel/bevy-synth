//! Eight synthesized percussion voices sharing one one-shot envelope shape.

use crate::filter::{Svf, SvfMode};
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

    /// Pads that cannot ring together. Closing a hi-hat stops the open one:
    /// this is what makes a hat part sound like one instrument rather than two
    /// overlapping ones. Nothing else on the kit chokes.
    pub fn choke_group(self) -> Option<u8> {
        match self {
            Pad::ClosedHat | Pad::OpenHat => Some(0),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wave {
    Sine,
    Triangle,
}

/// Everything that distinguishes one pad from another, as numbers. Keeping the
/// differences in data rather than in branches means `DrumVoice::next` is one
/// algorithm, and adding a pad is adding a row.
#[derive(Debug, Clone, Copy)]
struct Recipe {
    wave: Wave,
    /// Level of the body oscillator, 0.0 for the noise-only pads.
    body: f32,
    /// Second oscillator, as a ratio of the first. `partial` is its level.
    partial_ratio: f32,
    partial: f32,
    /// Pitch envelope depth: the body starts `1 + bend` times up.
    bend: f32,
    bend_time: f32,
    /// Level of the filtered noise, 0.0 for the pitched pads.
    noise: f32,
    /// Noise filter centre, as a ratio of the pad's tuned base frequency.
    noise_ratio: f32,
    noise_res: f32,
    noise_mode: SvfMode,
    /// Noise decay as a multiple of the pad's decay: a snare's rattle outlasts
    /// its body.
    noise_decay: f32,
    /// Level of the unfiltered 2 ms transient.
    click: f32,
    /// Envelope retriggers. 1 for everything but the clap.
    bursts: u8,
    burst_gap: f32,
}

const NO_RECIPE: Recipe = Recipe {
    wave: Wave::Sine,
    body: 0.0,
    partial_ratio: 1.0,
    partial: 0.0,
    bend: 0.0,
    bend_time: 0.01,
    noise: 0.0,
    noise_ratio: 1.0,
    noise_res: 0.2,
    noise_mode: SvfMode::Bandpass,
    noise_decay: 1.0,
    click: 0.0,
    bursts: 1,
    burst_gap: 0.010,
};

impl Pad {
    fn recipe(self) -> Recipe {
        match self {
            // A sine falling from four times its pitch, with a click on top.
            Pad::Kick => Recipe {
                body: 1.0,
                bend: 3.0,
                bend_time: 0.05,
                click: 0.5,
                ..NO_RECIPE
            },
            // Two detuned triangles for the body, noise through a bandpass an
            // octave and a bit above, ringing longer than the body.
            Pad::Snare => Recipe {
                wave: Wave::Triangle,
                body: 0.45,
                partial_ratio: 1.62,
                partial: 0.3,
                bend: 0.6,
                bend_time: 0.02,
                noise: 0.8,
                noise_ratio: 10.0,
                noise_res: 0.25,
                noise_decay: 1.3,
                ..NO_RECIPE
            },
            Pad::ClosedHat => Recipe {
                noise: 1.0,
                noise_mode: SvfMode::Highpass,
                noise_res: 0.1,
                ..NO_RECIPE
            },
            // Identical but for the decay, which lives in `base_decay`.
            Pad::OpenHat => Recipe {
                noise: 1.0,
                noise_mode: SvfMode::Highpass,
                noise_res: 0.1,
                ..NO_RECIPE
            },
            Pad::Clap => Recipe {
                noise: 1.0,
                noise_res: 0.35,
                bursts: 3,
                burst_gap: 0.010,
                ..NO_RECIPE
            },
            Pad::LowTom => Recipe {
                body: 1.0,
                bend: 1.2,
                bend_time: 0.08,
                noise: 0.05,
                noise_ratio: 8.0,
                noise_mode: SvfMode::Highpass,
                noise_decay: 0.2,
                ..NO_RECIPE
            },
            Pad::HighTom => Recipe {
                body: 1.0,
                bend: 1.4,
                bend_time: 0.06,
                noise: 0.05,
                noise_ratio: 8.0,
                noise_mode: SvfMode::Highpass,
                noise_decay: 0.2,
                ..NO_RECIPE
            },
            // One short bandpassed burst and nothing else.
            Pad::Rim => Recipe {
                noise: 1.0,
                noise_res: 0.5,
                ..NO_RECIPE
            },
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
    recipe: Recipe,
    rng: Rng,

    /// Body oscillators: phase in turns, so wrapping is a subtraction.
    phase: f32,
    partial_phase: f32,
    hz: f32,

    amp: Decay,
    /// Multiplies `hz` on its way to the oscillator: the downward thump.
    pitch: Decay,
    noise_env: Decay,
    /// The short unfiltered transient at the head of the hit.
    click: Decay,
    filter: Svf,

    /// Clap retriggering. `bursts_left` counts envelope restarts still owed.
    bursts_left: u8,
    burst_countdown: u32,
    /// Held so a retrigger can reuse the strike's level and length.
    gain: f32,
    decay_secs: f32,
}

impl DrumVoice {
    pub fn new(sample_rate: f32, seed: u64) -> DrumVoice {
        let sample_rate = sample_rate.max(1.0);
        DrumVoice {
            sample_rate,
            pad: Pad::Kick,
            recipe: Pad::Kick.recipe(),
            rng: Rng::new(seed),
            phase: 0.0,
            partial_phase: 0.0,
            hz: Pad::Kick.base_hz(),
            amp: Decay::new(),
            pitch: Decay::new(),
            noise_env: Decay::new(),
            click: Decay::new(),
            filter: Svf::new(sample_rate),
            bursts_left: 0,
            burst_countdown: 0,
            gain: 0.0,
            decay_secs: 0.0,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.filter.set_sample_rate(self.sample_rate);
        self.silence();
    }

    /// Fires the pad. `gain` is the strike level, `tune_semitones` shifts the
    /// pitch, and `decay_scale` stretches or shortens the tail.
    pub fn strike(&mut self, pad: Pad, gain: f32, tune_semitones: f32, decay_scale: f32) {
        self.pad = pad;
        self.recipe = pad.recipe();
        self.hz = pad.base_hz() * cents_to_ratio(tune_semitones * 100.0);
        self.phase = 0.0;
        self.partial_phase = 0.0;
        self.gain = gain;
        self.decay_secs = pad.base_decay() * decay_scale.clamp(0.1, 4.0);

        self.filter.set_params(
            self.hz * self.recipe.noise_ratio,
            self.recipe.noise_res,
        );

        self.amp.trigger(self.decay_secs, self.sample_rate, gain);
        self.pitch.trigger(self.recipe.bend_time, self.sample_rate, 1.0);
        self.click
            .trigger(0.002, self.sample_rate, gain * self.recipe.click);

        if self.recipe.bursts > 1 {
            // The clap's envelope is driven by the burst counter, starting on
            // the next sample rather than here.
            self.noise_env.silence();
            self.bursts_left = self.recipe.bursts;
            self.burst_countdown = 0;
        } else {
            self.bursts_left = 0;
            // A pad with no noise component (the kick) must not leave the
            // envelope sitting at a nonzero level forever: `next` never reads
            // it, so `is_active` would never see it fall and the voice would
            // never report itself silent.
            if self.recipe.noise > 0.0 {
                self.noise_env.trigger(
                    self.decay_secs * self.recipe.noise_decay,
                    self.sample_rate,
                    gain,
                );
            } else {
                self.noise_env.silence();
            }
        }
    }

    // Not `Iterator::next`, for the same reason as `Adsr::next`: this is a
    // signal generator, not a sequence that can run out.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> f32 {
        if !self.is_active() {
            return 0.0;
        }
        self.advance_bursts();

        let bend = 1.0 + self.pitch.next() * self.recipe.bend;
        let step = (self.hz * bend / self.sample_rate).clamp(0.0, 0.49);
        self.phase = wrap(self.phase + step);

        let amp = self.amp.next();
        let mut out = 0.0;
        if self.recipe.body > 0.0 {
            out += wave(self.recipe.wave, self.phase) * amp * self.recipe.body;
        }
        if self.recipe.partial > 0.0 {
            self.partial_phase = wrap(self.partial_phase + step * self.recipe.partial_ratio);
            out += wave(self.recipe.wave, self.partial_phase) * amp * self.recipe.partial;
        }
        if self.recipe.noise > 0.0 {
            let n = self.rng.next_bipolar() * self.noise_env.next();
            out += self.filter.process_mode(n, self.recipe.noise_mode) * self.recipe.noise;
        }
        out + self.rng.next_bipolar() * self.click.next()
    }

    /// Restarts the noise envelope `bursts` times at `burst_gap` intervals.
    /// Three short bursts and then a tail is a clap; one is everything else.
    fn advance_bursts(&mut self) {
        if self.bursts_left == 0 {
            return;
        }
        if self.burst_countdown > 0 {
            self.burst_countdown -= 1;
            return;
        }
        self.bursts_left -= 1;
        let last = self.bursts_left == 0;
        let seconds = if last {
            self.decay_secs
        } else {
            self.recipe.burst_gap * 0.8
        };
        self.noise_env
            .trigger(seconds, self.sample_rate, self.gain);
        self.burst_countdown = (self.recipe.burst_gap * self.sample_rate) as u32;
    }

    pub fn is_silent(&self) -> bool {
        !self.is_active()
    }

    pub fn pad(&self) -> Pad {
        self.pad
    }

    fn is_active(&self) -> bool {
        self.amp.is_active()
            || self.noise_env.is_active()
            || self.click.is_active()
            || self.bursts_left > 0
    }

    pub fn silence(&mut self) {
        self.amp.silence();
        self.pitch.silence();
        self.noise_env.silence();
        self.click.silence();
        self.filter.reset();
        self.phase = 0.0;
        self.partial_phase = 0.0;
        self.bursts_left = 0;
        self.burst_countdown = 0;
    }
}

fn wrap(phase: f32) -> f32 {
    if phase >= 1.0 {
        phase - 1.0
    } else {
        phase
    }
}

/// One cycle of the requested shape, from a phase in turns (0..1).
/// `synth_core` has no dependencies but is not `no_std`, so `sin` is free.
fn wave(shape: Wave, phase: f32) -> f32 {
    match shape {
        Wave::Sine => (phase * core::f32::consts::TAU).sin(),
        Wave::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
    }
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

    /// Every pad, not just the one that was written first.
    #[test]
    fn every_pad_sounds_and_then_stops() {
        for pad in Pad::ALL {
            let mut v = DrumVoice::new(48_000.0, 7);
            v.strike(pad, 1.0, 0.0, 1.0);

            let mut peak = 0.0f32;
            for _ in 0..24_000 {
                peak = peak.max(v.next().abs());
            }
            assert!(peak > 0.05, "{} is inaudible: {peak}", pad.name());

            for _ in 0..96_000 {
                v.next();
            }
            assert!(v.is_silent(), "{} never released", pad.name());
        }
    }

    /// The triple retrigger is the whole difference between a clap and a short
    /// snare. A plain decay only ever falls, so a rise proves a retrigger.
    #[test]
    fn the_clap_retriggers() {
        let mut v = DrumVoice::new(48_000.0, 3);
        v.strike(Pad::Clap, 1.0, 0.0, 1.0);

        // Peak amplitude in successive 4 ms windows across the first 60 ms.
        let mut windows = [0.0f32; 15];
        for w in windows.iter_mut() {
            for _ in 0..192 {
                *w = w.max(v.next().abs());
            }
        }
        let rises = windows.windows(2).filter(|p| p[1] > p[0] * 1.2).count();
        assert!(rises >= 2, "the clap does not retrigger: {windows:?}");
    }

    /// The spec's continuity claim, made checkable. A pad struck again while
    /// it still rings has to *restart* — the level comes back up and a full
    /// tail runs again — and that restart must not be a bigger jump than an
    /// ordinary strike from silence already makes. The open hat is the pad to
    /// measure it on: no body oscillator and no click, so the only thing a
    /// retrigger changes is the envelope.
    #[test]
    fn a_mid_decay_retrigger_restarts_without_a_click() {
        fn window(v: &mut DrumVoice, samples: usize) -> Vec<f32> {
            (0..samples).map(|_| v.next()).collect()
        }
        fn peak(w: &[f32]) -> f32 {
            w.iter().fold(0.0f32, |m, s| m.max(s.abs()))
        }
        fn biggest_jump(w: &[f32]) -> f32 {
            w.windows(2).fold(0.0f32, |m, p| m.max((p[1] - p[0]).abs()))
        }

        // A strike from silence, for the size of the jump it makes on its own.
        let mut fresh = DrumVoice::new(48_000.0, 5);
        fresh.strike(Pad::OpenHat, 1.0, 0.0, 1.0);
        let attack = biggest_jump(&window(&mut fresh, 480));

        // The same pad, struck again 200 ms into its 380 ms tail.
        let mut v = DrumVoice::new(48_000.0, 5);
        v.strike(Pad::OpenHat, 1.0, 0.0, 1.0);
        let _ = window(&mut v, 9_120);
        let tail = window(&mut v, 480);
        let last = *tail.last().unwrap();

        v.strike(Pad::OpenHat, 1.0, 0.0, 1.0);
        let restart = window(&mut v, 480);

        assert!(
            peak(&restart) > peak(&tail) * 5.0,
            "the retrigger did not restart the envelope: {} then {}",
            peak(&tail),
            peak(&restart)
        );
        assert!(
            (restart[0] - last).abs() <= attack,
            "the retrigger jumped further than an ordinary strike: {} vs {attack}",
            (restart[0] - last).abs()
        );

        // 500 ms further on. The old envelope would have been flushed to zero
        // by now; the new one must not be, or the retrigger inherited the
        // remainder of the old tail instead of starting a new one.
        let _ = window(&mut v, 24_000);
        assert!(!v.is_silent(), "the retrigger did not restart the tail");
    }

    /// The hats have to know they belong together; the rack acts on it later.
    #[test]
    fn the_hats_share_a_choke_group() {
        assert!(Pad::ClosedHat.choke_group().is_some());
        assert_eq!(Pad::ClosedHat.choke_group(), Pad::OpenHat.choke_group());
        assert_eq!(Pad::Kick.choke_group(), None);
        assert_eq!(Pad::Snare.choke_group(), None);
    }
}
