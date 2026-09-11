//! The bassline voice: one oscillator, one lowpass, and an envelope aimed at
//! the cutoff.
//!
//! This is a peer of [`crate::voice::Voice`], not a variant of it. `Voice` is
//! polyphonic, has two oscillators, an LFO destination and a dozen parameters;
//! a 303 is one voice, one filter and eight knobs, and the two gestures that
//! make it an instrument — accent and slide — have no analogue in the
//! polyphonic voice at all. Sharing the code would mean growing `Voice` with
//! flags that are always false for every note it will ever play.

use crate::env::{Adsr, AdsrSettings};
use crate::filter::{Filter, Slope, SvfMode};
use crate::note::midi_to_hz;
use crate::osc::Oscillator;
use crate::params::BassParams;

/// Both envelopes attack in three milliseconds: fast enough to click like the
/// original, slow enough not to alias.
const ATTACK_S: f32 = 0.003;

/// The amp envelope's release. Short, because the sequencer's gate is what
/// actually decides note length here.
const AMP_RELEASE_S: f32 = 0.008;

/// One monophonic bassline voice.
#[derive(Debug)]
pub struct BassVoice {
    sample_rate: f32,
    osc: Oscillator,
    filter: Filter,
    amp_env: Adsr,
    filter_env: Adsr,
    /// The note the voice is playing, as MIDI.
    note: u8,
    gate: bool,
    /// Latched at note-on: editing the step mid-note must not change the note
    /// already sounding.
    accented: bool,
    /// Current pitch in semitones, which may sit between notes during a slide.
    pitch: f32,
    /// Where a slide is heading.
    target_pitch: f32,
    /// Per-sample one-pole coefficient for the slide. `0.0` means jump.
    glide_coef: f32,
    /// The frequency the oscillator was last set to, for tests and for the
    /// pitch update to compare against.
    current_hz: f32,
}

impl BassVoice {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        let mut filter = Filter::new(sample_rate);
        // Fixed, both of them. A 303's filter is a 24 dB lowpass and nothing
        // else; making either selectable would be a different instrument.
        filter.mode = SvfMode::Lowpass;
        filter.slope = Slope::Db24;

        let mut amp_env = Adsr::new(sample_rate);
        amp_env.set_settings(AdsrSettings {
            attack: ATTACK_S,
            decay: 0.0,
            sustain: 1.0,
            release: AMP_RELEASE_S,
        });

        let filter_env = Adsr::new(sample_rate);

        Self {
            sample_rate,
            osc: Oscillator::new(sample_rate, seed),
            filter,
            amp_env,
            filter_env,
            note: 40,
            gate: false,
            accented: false,
            pitch: 40.0,
            target_pitch: 40.0,
            glide_coef: 0.0,
            current_hz: midi_to_hz(40.0),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.osc.set_sample_rate(sample_rate);
        self.filter.set_sample_rate(sample_rate);
        self.amp_env.set_sample_rate(sample_rate);
        self.filter_env.set_sample_rate(sample_rate);
    }

    /// Starts a note.
    ///
    /// `accent` is latched here rather than read per sample, so editing the
    /// step mid-note cannot change what is already sounding.
    ///
    /// `slide` is the tie. A tied note keeps both envelopes exactly where they
    /// are and lets the glide walk the pitch to its new home — not
    /// retriggering is the whole point, because a retriggered note cannot
    /// produce the legato squelch the gesture exists for. A tie with nothing
    /// sounding has nothing to slide from, so it falls back to a normal
    /// note-on; that covers step 0 of a pattern and a tie after a rest.
    pub fn note_on(&mut self, note: u8, accent: bool, slide: bool) {
        self.note = note;
        self.gate = true;
        self.accented = accent;
        self.target_pitch = note as f32;

        if slide && self.is_active() {
            return;
        }

        self.pitch = self.target_pitch;
        self.amp_env.gate_on(true);
        self.filter_env.gate_on(true);
    }

    pub fn note_off(&mut self) {
        self.gate = false;
        self.amp_env.gate_off();
        self.filter_env.gate_off();
    }

    /// Cuts the voice dead, envelopes and filter state together. For the
    /// transport stopping and for panic.
    pub fn silence(&mut self) {
        self.gate = false;
        self.amp_env.hard_reset();
        self.filter_env.hard_reset();
        self.filter.reset();
    }

    pub fn is_active(&self) -> bool {
        self.amp_env.is_active()
    }

    /// Whether the sounding note was started as an accent.
    pub fn is_accented(&self) -> bool {
        self.accented
    }

    /// Renders one block, overwriting `out`.
    pub fn process_block(&mut self, out: &mut [f32], p: &BassParams) {
        // Everything that arrives from the control side is bounded here as
        // well as in `SharedBass::snapshot`: `BassVoice` is public, so it
        // cannot assume it was called through the mirror. `sane_or` strips
        // NaN first, so by the time `clamp` runs the value is already finite
        // and `clamp` cannot mis-propagate one. `tune`'s bound of +/-12
        // semitones matches `SharedBass::snapshot` so the two cannot drift
        // apart, and it is not just cosmetic: an extreme-but-finite `tune`
        // would otherwise push `midi_to_hz` towards infinity below.
        let tune = sane_or(p.tune, 0.0).clamp(-12.0, 12.0);
        let base_cutoff = sane_or(p.cutoff, 300.0).clamp(20.0, 20_000.0);
        let resonance = sane_or(p.resonance, 0.7).clamp(0.0, 1.0);
        let mut env_mod = sane_or(p.env_mod, 3.0).clamp(0.0, 6.0);
        let decay = sane_or(p.decay, 0.3).clamp(0.02, 2.0);

        let accent_amount = sane_or(p.accent, 0.5).clamp(0.0, 1.0);
        // One knob, three destinations. All three are neutral on an
        // unaccented step: nothing is added and nothing is scaled.
        let (accent_level, accent_octaves, accent_depth) = if self.accented {
            (1.0 + accent_amount * 0.5, accent_amount * 1.5, 1.0 + accent_amount)
        } else {
            (1.0, 0.0, 1.0)
        };
        env_mod *= accent_depth;

        // Decay-only: sustain is zero, so the sweep finishes even under a held
        // gate. Release matches decay, so letting go mid-sweep sounds like the
        // sweep continuing rather than a second gesture.
        self.filter_env.set_settings(AdsrSettings {
            attack: ATTACK_S,
            decay,
            sustain: 0.0,
            release: decay,
        });

        let slide_time = sane_or(p.slide_time, 0.06).clamp(0.01, 0.5);
        self.set_glide(slide_time);

        for sample in out.iter_mut() {
            // Pitch first: a slide moves it every sample.
            if self.glide_coef > 0.0 {
                self.pitch = self.target_pitch + (self.pitch - self.target_pitch) * self.glide_coef;
            } else {
                self.pitch = self.target_pitch;
            }
            self.current_hz = midi_to_hz(self.pitch + tune);
            self.osc.set_freq(self.current_hz);

            let env = self.filter_env.next();
            let amp = self.amp_env.next();

            // Octaves rather than Hz: an envelope that adds 3 octaves sweeps
            // the same musical distance from 80 Hz as it does from 800.
            // `base_cutoff` and `env_mod` are already finite (see above) and
            // `env` never leaves `0.0..=1.0`, so `octaves.exp2()` cannot be
            // NaN either -- `clamp` is safe here too. `env_mod` already has
            // `accent_depth` folded into it above, so it caps at 12 rather
            // than 6, and `accent_octaves` adds at most 1.5 on top: 13.5
            // octaves, nowhere near overflowing the cutoff's clamp.
            let octaves = env_mod * env + accent_octaves;
            let cutoff = (base_cutoff * octaves.exp2()).clamp(20.0, 20_000.0);
            self.filter.set_params(cutoff, resonance);

            let raw = self.osc.next(p.wave);
            let filtered = self.filter.process(raw);
            let value = filtered * amp * accent_level;
            *sample = if value.is_finite() { value } else { 0.0 };
        }
    }

    /// The filter envelope's current level. Test-facing: the sweep finishing
    /// under a held gate is the behaviour that makes this a 303 and not a
    /// generic mono synth, so it is worth being able to assert on.
    pub fn filter_env_level(&self) -> f32 {
        self.filter_env.level()
    }

    /// The frequency the oscillator is currently running at.
    pub fn current_hz(&self) -> f32 {
        self.current_hz
    }

    fn set_glide(&mut self, seconds: f32) {
        // Same one-pole-in-semitone-space form `Voice::set_glide` uses, so the
        // two instruments slide with the same curve.
        self.glide_coef = if !seconds.is_finite() || seconds <= 0.0001 {
            0.0
        } else {
            (-1.0 / (seconds * self.sample_rate)).exp()
        };
    }
}

/// `params::sane` is private to that module; this is the same idea, local.
fn sane_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Renders `seconds` of audio and hands back the peak absolute sample.
    fn peak(v: &mut BassVoice, p: &BassParams, seconds: f32) -> f32 {
        let mut block = [0.0f32; 64];
        let blocks = (seconds * SR / 64.0) as usize;
        let mut worst = 0.0f32;
        for _ in 0..blocks {
            v.process_block(&mut block, p);
            for s in block {
                assert!(s.is_finite(), "the bass produced a non-finite sample");
                worst = worst.max(s.abs());
            }
        }
        worst
    }

    #[test]
    fn a_gated_note_makes_sound_and_silence_stops_it() {
        let mut v = BassVoice::new(SR, 0xB455_0001);
        let p = BassParams::default();
        assert!(!v.is_active(), "a fresh voice is silent");

        v.note_on(40, false, false);
        assert!(v.is_active());
        assert!(peak(&mut v, &p, 0.05) > 0.01, "a gated note should sound");

        v.silence();
        assert!(!v.is_active());
        assert!(peak(&mut v, &p, 0.05) < 1e-6, "silence must be silent");
    }

    #[test]
    fn the_filter_envelope_decays_to_zero_while_the_gate_is_held() {
        // Sustain is zero by design: the cutoff sweep is the instrument, and
        // it has to finish even on a long note.
        let mut v = BassVoice::new(SR, 0xB455_0002);
        let p = BassParams { decay: 0.05, ..BassParams::default() };
        v.note_on(40, false, false);
        let mut block = [0.0f32; 64];
        for _ in 0..(0.4 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        assert!(
            v.filter_env_level() < 0.01,
            "the filter envelope should be spent while the gate is still held"
        );
        assert!(v.is_active(), "the amp envelope is still holding the note");
    }

    #[test]
    fn env_mod_opens_the_filter() {
        // More envelope depth means more high-frequency energy early in the
        // note, which shows up as a bigger peak through a resonant lowpass.
        let mut closed = BassVoice::new(SR, 0xB455_0003);
        let mut open = BassVoice::new(SR, 0xB455_0003);
        let shut = BassParams { env_mod: 0.0, cutoff: 120.0, ..BassParams::default() };
        let wide = BassParams { env_mod: 5.0, cutoff: 120.0, ..BassParams::default() };
        closed.note_on(40, false, false);
        open.note_on(40, false, false);
        let a = peak(&mut closed, &shut, 0.1);
        let b = peak(&mut open, &wide, 0.1);
        assert!(b > a * 1.5, "env_mod 5.0 ({b}) should be far brighter than 0.0 ({a})");
    }

    #[test]
    fn tune_transposes_the_note() {
        let mut v = BassVoice::new(SR, 0xB455_0004);
        v.note_on(45, false, false);
        let plain = BassParams { tune: 0.0, ..BassParams::default() };
        let up = BassParams { tune: 12.0, ..BassParams::default() };
        let mut block = [0.0f32; 64];
        v.process_block(&mut block, &plain);
        let low = v.current_hz();
        v.process_block(&mut block, &up);
        let high = v.current_hz();
        assert!(
            (high / low - 2.0).abs() < 0.01,
            "twelve semitones should double the frequency: {low} -> {high}"
        );
    }

    #[test]
    fn nonsense_parameters_never_produce_nonsense_audio() {
        // `BassParams` reaching `process_block` has already been through
        // `SharedBass::snapshot`, but the voice is a public type and must not
        // rely on that.
        let mut v = BassVoice::new(SR, 0xB455_0005);
        let p = BassParams {
            cutoff: f32::NAN,
            resonance: f32::NAN,
            env_mod: f32::NAN,
            tune: f32::NAN,
            ..BassParams::default()
        };
        v.note_on(40, false, false);
        let mut block = [0.0f32; 64];
        for _ in 0..200 {
            v.process_block(&mut block, &p);
            for s in block {
                assert!(s.is_finite(), "NaN parameters leaked into the audio");
            }
        }
    }

    #[test]
    fn an_absurd_tune_cannot_run_the_pitch_away() {
        let p = BassParams { tune: 1e30, ..BassParams::default() };
        let mut v = BassVoice::new(SR, 0xB455_0006);
        v.note_on(40, false, false);
        // `peak` already asserts every sample is finite.
        let _ = peak(&mut v, &p, 0.05);
        assert!(
            v.current_hz().is_finite() && v.current_hz() <= midi_to_hz(52.0),
            "tune must be bounded to +/-12 semitones, got {} Hz",
            v.current_hz()
        );
    }

    #[test]
    fn an_accented_note_is_louder_and_brighter() {
        let p = BassParams { accent: 1.0, cutoff: 150.0, ..BassParams::default() };
        let mut plain = BassVoice::new(SR, 0xB455_0011);
        let mut loud = BassVoice::new(SR, 0xB455_0011);
        plain.note_on(40, false, false);
        loud.note_on(40, true, false);
        let a = peak(&mut plain, &p, 0.08);
        let b = peak(&mut loud, &p, 0.08);
        assert!(b > a * 1.2, "an accented note ({b}) should top an unaccented one ({a})");
    }

    #[test]
    fn accent_is_neutral_when_the_knob_is_at_zero() {
        let p = BassParams { accent: 0.0, ..BassParams::default() };
        let mut plain = BassVoice::new(SR, 0xB455_0012);
        let mut marked = BassVoice::new(SR, 0xB455_0012);
        plain.note_on(40, false, false);
        marked.note_on(40, true, false);
        let a = peak(&mut plain, &p, 0.08);
        let b = peak(&mut marked, &p, 0.08);
        assert!((a - b).abs() < 1e-6, "accent 0.0 should change nothing: {a} vs {b}");
    }

    #[test]
    fn accent_is_latched_at_note_on() {
        // The accent belongs to the note, not to the knob: turning the knob
        // mid-note changes how loud the *next* accented note is, not this one.
        let mut v = BassVoice::new(SR, 0xB455_0013);
        v.note_on(40, true, false);
        assert!(v.is_accented());
        v.note_on(40, false, false);
        assert!(!v.is_accented());
    }

    #[test]
    fn a_tie_glides_and_does_not_retrigger() {
        let p = BassParams { slide_time: 0.1, ..BassParams::default() };
        let mut v = BassVoice::new(SR, 0xB455_0014);
        let mut block = [0.0f32; 64];

        v.note_on(40, false, false);
        for _ in 0..(0.2 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        let settled = v.filter_env_level();

        // The tie: same voice, new note, no retrigger.
        v.note_on(52, false, true);
        v.process_block(&mut block, &p);
        assert!(
            v.filter_env_level() <= settled,
            "a tie must not restart the filter envelope"
        );
        let start_hz = v.current_hz();

        // Partway through the glide the pitch is between the two notes.
        for _ in 0..(0.03 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        let mid_hz = v.current_hz();
        assert!(
            mid_hz > start_hz && mid_hz < midi_to_hz(52.0),
            "pitch should be mid-glide: {start_hz} -> {mid_hz} -> {}",
            midi_to_hz(52.0)
        );

        // And it gets there.
        for _ in 0..(0.6 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        assert!((v.current_hz() / midi_to_hz(52.0) - 1.0).abs() < 0.01);
    }

    #[test]
    fn an_untied_note_jumps_and_retriggers() {
        let p = BassParams { slide_time: 0.1, ..BassParams::default() };
        let mut v = BassVoice::new(SR, 0xB455_0015);
        let mut block = [0.0f32; 64];

        v.note_on(40, false, false);
        for _ in 0..(0.5 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        assert!(v.filter_env_level() < 0.01, "the envelope should be spent");

        v.note_on(52, false, false);
        v.process_block(&mut block, &p);
        assert!(v.filter_env_level() > 0.01, "an untied note restarts the envelope");
        assert!(
            (v.current_hz() / midi_to_hz(52.0) - 1.0).abs() < 0.01,
            "an untied note jumps straight to pitch"
        );
    }

    #[test]
    fn a_tie_from_silence_retriggers_instead() {
        // Nothing to slide from. Falling back to a normal note-on is the only
        // sensible reading, and it is what step 0 of a pattern needs.
        let p = BassParams::default();
        let mut v = BassVoice::new(SR, 0xB455_0016);
        let mut block = [0.0f32; 64];
        v.note_on(40, false, true);
        v.process_block(&mut block, &p);
        assert!(v.is_active(), "a tie with nothing sounding must still start a note");
        assert!(v.filter_env_level() > 0.0, "and must trigger its envelope");
        assert!((v.current_hz() / midi_to_hz(40.0) - 1.0).abs() < 0.01);
    }
}
