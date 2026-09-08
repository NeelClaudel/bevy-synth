//! A single synthesizer voice: everything needed to sound one note.
//!
//! Two band-limited oscillators plus a sub and a noise source, into a filter,
//! shaped by two envelopes. Polyphony is just several of these summed, which is
//! why all the per-note state — envelope positions, filter memory, oscillator
//! phase — lives here rather than in the engine.
//!
//! # Block processing
//!
//! [`Voice::process_block`] fills a whole block at once rather than exposing a
//! per-sample `next()`. Envelopes and oscillators still run per sample, but the
//! filter coefficients (which need a `tan`) are computed once per block. At 32
//! samples that is a 1.5 kHz update rate for the cutoff: fast enough that even
//! a 1 ms filter envelope sounds continuous, and 32x cheaper.

use crate::env::{Adsr, EnvStage};
use crate::filter::Filter;
use crate::lfo::LfoTarget;
use crate::note::{cents_to_ratio, midi_to_hz};
use crate::osc::{Oscillator, Waveform};
use crate::params::Params;

/// How long a stolen voice takes to fade out before its replacement note
/// starts, in seconds. Long enough that the cut is inaudible, short enough that
/// the new note is not perceptibly late — 2 ms is roughly the threshold for
/// both.
const STEAL_FADE_SECONDS: f32 = 0.002;

/// One voice.
#[derive(Debug, Clone)]
pub struct Voice {
    sample_rate: f32,

    /// The MIDI note this voice is playing.
    pub note: u8,
    pub velocity: f32,
    /// True while the key is held. Distinct from "audible": a voice with the
    /// key released is still sounding through its release stage.
    pub gate: bool,
    /// Monotonically increasing at note-on. The engine steals the lowest.
    pub age: u64,

    osc1: Oscillator,
    osc2: Oscillator,
    sub: Oscillator,
    noise: Oscillator,

    filter: Filter,
    amp_env: Adsr,
    filter_env: Adsr,

    /// Current pitch in MIDI-note units, which may be between notes while
    /// gliding.
    pitch: f32,
    /// Where the glide is heading.
    target_pitch: f32,
    /// Per-sample one-pole coefficient for the glide. 0.0 means no glide.
    glide_coef: f32,

    /// Set while fading out to make room for a stolen note.
    steal_fade: Option<StealFade>,
}

#[derive(Debug, Clone, Copy)]
struct StealFade {
    gain: f32,
    step: f32,
    note: u8,
    velocity: f32,
}

impl Voice {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        Self {
            sample_rate,
            note: 60,
            velocity: 0.0,
            gate: false,
            age: 0,
            // Distinct seeds so the noise sources and random start phases of
            // different voices are not correlated — correlated noise across
            // voices sums into something tonal and strange.
            osc1: Oscillator::new(sample_rate, seed * 4 + 1),
            osc2: Oscillator::new(sample_rate, seed * 4 + 2),
            sub: Oscillator::new(sample_rate, seed * 4 + 3),
            noise: Oscillator::new(sample_rate, seed * 4 + 4),
            filter: Filter::new(sample_rate),
            amp_env: Adsr::new(sample_rate),
            filter_env: Adsr::new(sample_rate),
            pitch: 60.0,
            target_pitch: 60.0,
            glide_coef: 0.0,
            steal_fade: None,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.osc1.set_sample_rate(sample_rate);
        self.osc2.set_sample_rate(sample_rate);
        self.sub.set_sample_rate(sample_rate);
        self.noise.set_sample_rate(sample_rate);
        self.filter.set_sample_rate(sample_rate);
        self.amp_env.set_sample_rate(sample_rate);
        self.filter_env.set_sample_rate(sample_rate);
    }

    /// True while this voice is producing sound, including its release tail.
    /// The engine will not reuse a voice for which this is true unless it has
    /// to.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.amp_env.is_active() || self.steal_fade.is_some()
    }

    /// True while the key is held. These are the voices to steal last.
    #[inline]
    pub fn is_held(&self) -> bool {
        self.gate && self.amp_env.stage() != EnvStage::Release
    }

    /// Current amplitude envelope level, used to pick the quietest voice when
    /// stealing.
    #[inline]
    pub fn level(&self) -> f32 {
        self.amp_env.level()
    }

    /// Starts a note.
    ///
    /// `glide_from` gives the pitch to slide from, in MIDI-note units; `None`
    /// starts at the target pitch. `reset` forces the envelopes back to zero
    /// rather than continuing from where they are — see [`Adsr::gate_on`].
    pub fn note_on(&mut self, note: u8, velocity: f32, glide_from: Option<f32>, reset: bool, age: u64) {
        self.note = note;
        self.velocity = velocity.clamp(0.0, 1.0);
        self.gate = true;
        self.age = age;
        self.target_pitch = note as f32;

        match glide_from {
            Some(from) => self.pitch = from,
            None => self.pitch = self.target_pitch,
        }

        if reset {
            // Start the oscillators at scattered phases rather than all at
            // zero. Identical start phases across a chord make the attack
            // transient sum into an audible click.
            self.osc1.reset_phase();
            self.osc2.set_phase(0.37);
            self.sub.set_phase(0.11);
            self.filter.reset();
        }

        self.amp_env.gate_on(reset);
        self.filter_env.gate_on(reset);
    }

    /// Releases the key. The voice keeps sounding through its release stage.
    pub fn note_off(&mut self) {
        self.gate = false;
        self.amp_env.gate_off();
        self.filter_env.gate_off();
    }

    /// Silences the voice immediately. Only for panics — this clicks.
    pub fn kill(&mut self) {
        self.gate = false;
        self.amp_env.hard_reset();
        self.filter_env.hard_reset();
        self.filter.reset();
        self.steal_fade = None;
    }

    /// Takes this voice for a new note, fading the old one out first.
    ///
    /// The fade is the whole point. Cutting a sounding voice to start a new one
    /// leaves a step discontinuity — a click — and in a busy polyphonic passage
    /// voices get stolen constantly, so the clicks become a rattle. A couple of
    /// milliseconds of fade removes it entirely at no perceptible cost in
    /// latency.
    pub fn steal(&mut self, note: u8, velocity: f32, age: u64) {
        self.age = age;
        if !self.is_active() {
            // Nothing sounding, so nothing to fade.
            self.note_on(note, velocity, None, true, age);
            return;
        }
        self.steal_fade = Some(StealFade {
            gain: 1.0,
            step: 1.0 / (STEAL_FADE_SECONDS * self.sample_rate).max(1.0),
            note,
            velocity,
        });
    }

    /// Sets the glide time in seconds. Zero disables it.
    #[inline]
    pub fn set_glide(&mut self, seconds: f32) {
        self.glide_coef = if seconds <= 0.0001 {
            0.0
        } else {
            (-1.0 / (seconds * self.sample_rate)).exp()
        };
    }

    /// Retunes a sounding voice, gliding to the new pitch. Used by mono mode
    /// when a new note takes over the single voice.
    #[inline]
    pub fn set_target_note(&mut self, note: u8) {
        self.note = note;
        self.target_pitch = note as f32;
    }

    /// Renders one block and *adds* it into `out`.
    ///
    /// Additive rather than overwriting so the engine can sum voices without a
    /// scratch buffer per voice.
    pub fn process_block(&mut self, out: &mut [f32], p: &Params, lfo: f32, mod_depth: f32) {
        if !self.is_active() {
            return;
        }

        // --- Per-block setup ---

        self.amp_env.set_settings(p.amp_env);
        self.filter_env.set_settings(p.filter_env);
        self.set_glide(p.glide);
        self.filter.mode = p.filter_mode;
        self.filter.slope = p.filter_slope;

        // Modulation destinations. Only the selected target receives the LFO;
        // depth is the knob plus the mod wheel, so a patch can sit still until
        // the player asks for movement.
        let lfo_cutoff = if p.lfo_target == LfoTarget::Cutoff {
            lfo * mod_depth * 4.0
        } else {
            0.0
        };
        let lfo_pitch = if p.lfo_target == LfoTarget::Pitch {
            lfo * mod_depth * 12.0
        } else {
            0.0
        };
        let lfo_amp = if p.lfo_target == LfoTarget::Amplitude {
            1.0 - mod_depth * (0.5 - 0.5 * lfo)
        } else {
            1.0
        };
        let pulse_width = if p.lfo_target == LfoTarget::PulseWidth {
            (p.pulse_width + lfo * mod_depth * 0.45).clamp(0.05, 0.95)
        } else {
            p.pulse_width
        };

        self.osc1.set_pulse_width(pulse_width);
        self.osc2.set_pulse_width(pulse_width);
        self.sub.set_pulse_width(0.5);

        // Cutoff modulation is summed in octaves, then applied exponentially.
        // Octaves rather than Hz because pitch and brightness are both
        // logarithmic: +1 octave means the same thing to the ear at 200 Hz as
        // at 4 kHz, whereas "+2000 Hz" does not.
        let env_value = self.filter_env.level();
        let key_offset = (self.note as f32 - 60.0) / 12.0;
        let octaves = p.filter_env_amount * env_value
            + p.filter_key_track * key_offset
            + p.filter_velocity * self.velocity * 2.0
            + lfo_cutoff;
        let cutoff = (p.cutoff * octaves.exp2()).clamp(20.0, 20000.0);
        self.filter.set_params(cutoff, p.resonance);

        let osc1_ratio = cents_to_ratio(p.osc1_detune);
        let osc2_ratio = cents_to_ratio(p.osc2_detune);
        let pitch_offset = p.pitch_bend + lfo_pitch;

        // --- Per-sample loop ---

        for slot in out.iter_mut() {
            // Glide. One pole toward the target, in semitone space so the slide
            // is linear in pitch rather than in frequency.
            if self.glide_coef > 0.0 {
                self.pitch = self.target_pitch + (self.pitch - self.target_pitch) * self.glide_coef;
            } else {
                self.pitch = self.target_pitch;
            }

            let base = self.pitch + pitch_offset;
            self.osc1
                .set_freq(midi_to_hz(base + p.osc1_semitones) * osc1_ratio);
            self.osc2
                .set_freq(midi_to_hz(base + p.osc2_semitones) * osc2_ratio);
            self.sub.set_freq(midi_to_hz(base - 12.0));

            let mut sample = 0.0;
            if p.osc1_level > 0.0 {
                sample += self.osc1.next(p.osc1_wave) * p.osc1_level;
            }
            if p.osc2_level > 0.0 {
                sample += self.osc2.next(p.osc2_wave) * p.osc2_level;
            }
            if p.sub_level > 0.0 {
                sample += self.sub.next(Waveform::Pulse) * p.sub_level;
            }
            if p.noise_level > 0.0 {
                sample += self.noise.next(Waveform::Noise) * p.noise_level;
            }

            // The filter envelope runs per sample even though its effect on the
            // cutoff is applied per block: it must stay in step with the
            // amplitude envelope, or a short filter envelope would drift.
            self.filter_env.next();

            let filtered = self.filter.process(sample);
            let amp = self.amp_env.next() * self.velocity * lfo_amp;
            let mut value = filtered * amp;

            // Steal fade, if this voice is on its way out.
            if let Some(fade) = &mut self.steal_fade {
                fade.gain -= fade.step;
                if fade.gain <= 0.0 {
                    let (note, velocity) = (fade.note, fade.velocity);
                    let age = self.age;
                    self.steal_fade = None;
                    self.kill();
                    self.note_on(note, velocity, None, true, age);
                    // The rest of this block belongs to the new note; it starts
                    // from silence anyway, so contributing nothing here is
                    // correct and costs at most 32 samples of delay.
                    value = 0.0;
                } else {
                    value *= fade.gain;
                }
            }

            *slot += value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::Params;

    fn render(voice: &mut Voice, p: &Params, blocks: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut block = [0.0f32; crate::BLOCK];
        for _ in 0..blocks {
            block.fill(0.0);
            voice.process_block(&mut block, p, 0.0, 0.0);
            out.extend_from_slice(&block);
        }
        out
    }

    #[test]
    fn silent_until_a_note_arrives() {
        let mut v = Voice::new(48000.0, 0);
        let p = Params::default();
        assert!(!v.is_active());
        let out = render(&mut v, &p, 100);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn makes_sound_after_note_on() {
        let mut v = Voice::new(48000.0, 0);
        let p = Params::default();
        v.note_on(60, 1.0, None, true, 1);
        let out = render(&mut v, &p, 200);
        let peak = out.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(peak > 0.05, "voice was near-silent, peak {peak}");
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn goes_quiet_after_release() {
        let mut v = Voice::new(48000.0, 0);
        let mut p = Params::default();
        p.amp_env.release = 0.05;
        v.note_on(60, 1.0, None, true, 1);
        render(&mut v, &p, 200);
        v.note_off();
        // Release is 50 ms; give it a generous margin.
        render(&mut v, &p, 1000);
        assert!(!v.is_active(), "voice never finished releasing");
    }

    #[test]
    fn output_stays_finite_across_every_waveform_and_note() {
        for wave in Waveform::ALL {
            let mut p = Params::default();
            p.osc1_wave = wave;
            p.osc2_wave = wave;
            p.resonance = 0.95;
            for note in [24u8, 60, 100, 127] {
                let mut v = Voice::new(48000.0, note as u64);
                v.note_on(note, 1.0, None, true, 1);
                let out = render(&mut v, &p, 300);
                for (i, s) in out.iter().enumerate() {
                    assert!(s.is_finite(), "{wave:?} note {note} sample {i} = {s}");
                    assert!(s.abs() < 20.0, "{wave:?} note {note} peaked at {s}");
                }
            }
        }
    }

    #[test]
    fn glide_slides_between_pitches() {
        let mut v = Voice::new(48000.0, 0);
        let mut p = Params::default();
        p.glide = 0.2;
        v.note_on(48, 1.0, None, true, 1);
        render(&mut v, &p, 10);
        v.set_target_note(72);
        // Immediately after retargeting, the pitch should still be near the old
        // note, not snapped to the new one.
        render(&mut v, &p, 2);
        assert!(v.pitch < 55.0, "glide snapped instantly to {}", v.pitch);
        render(&mut v, &p, 2000);
        assert!((v.pitch - 72.0).abs() < 0.5, "glide never arrived: {}", v.pitch);
    }

    /// Stealing must not produce a discontinuity. We check that no
    /// sample-to-sample jump exceeds what the waveform itself can produce.
    #[test]
    fn stealing_does_not_click() {
        let mut v = Voice::new(48000.0, 0);
        let mut p = Params::default();
        p.osc1_wave = Waveform::Sine;
        p.osc2_level = 0.0;
        p.resonance = 0.0;
        p.cutoff = 18000.0;

        v.note_on(60, 1.0, None, true, 1);
        let before = render(&mut v, &p, 100);
        v.steal(67, 1.0, 2);
        let after = render(&mut v, &p, 100);

        let all: Vec<f32> = before.iter().chain(after.iter()).copied().collect();
        let mut worst: f32 = 0.0;
        for w in all.windows(2) {
            worst = worst.max((w[1] - w[0]).abs());
        }
        // A 60-100 Hz sine at this amplitude moves far less than this per
        // sample; a hard cut would jump by the full amplitude at once.
        assert!(worst < 0.1, "steal produced a jump of {worst}");
    }

    #[test]
    fn velocity_scales_output() {
        let peak_at = |vel: f32| {
            let mut v = Voice::new(48000.0, 0);
            let p = Params::default();
            v.note_on(60, vel, None, true, 1);
            render(&mut v, &p, 200)
                .iter()
                .fold(0.0f32, |a, &b| a.max(b.abs()))
        };
        assert!(peak_at(1.0) > peak_at(0.3) * 2.0);
    }
}
