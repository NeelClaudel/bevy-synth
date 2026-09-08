//! Step sequencer with a generative pattern writer.
//!
//! # Making random sound musical
//!
//! Constraining pitches to a scale ([`crate::scale`]) removes the wrong notes,
//! but a uniform random walk over a scale still sounds like a random walk: it
//! wanders, never settles, and has no shape. Three further constraints do most
//! of the work of making it sound composed:
//!
//! 1. **Proximity.** Melodies mostly move by step. Weighting nearby degrees
//!    much higher than distant ones turns a scatter into a line, while still
//!    leaving room for the occasional leap that keeps it interesting.
//! 2. **Chord tones on strong beats.** Landing on the root, third or fifth at
//!    the start of a bar establishes a key centre; passing tones in between
//!    then sound like passing tones rather than mistakes.
//! 3. **Rests.** Continuous notes sound mechanical. Gaps create phrasing, and a
//!    phrase is what the ear remembers.
//!
//! # Why a pattern, and not a note at a time
//!
//! The generator writes a whole pattern and then loops it. Generating each note
//! fresh as it plays sounds worse — repetition is what lets a listener
//! recognise a figure, and without it even well-chosen notes sound aimless.
//! Call [`Sequencer::regenerate`] when you want a new one.

use crate::clock::Clock;
use crate::params::{ClockSource, Params};
use crate::rng::Rng;
use crate::scale::ScaleQuantizer;

/// Maximum pattern length.
pub const MAX_STEPS: usize = 64;

/// One step of the pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// False means a rest.
    pub active: bool,
    pub note: u8,
    pub velocity: f32,
    /// Marks a step the generator treated as a downbeat. Useful for driving
    /// visuals in time with the music.
    pub accent: bool,
}

impl Default for Step {
    fn default() -> Self {
        Self {
            active: false,
            note: 60,
            velocity: 0.8,
            accent: false,
        }
    }
}

/// A whole pattern, as a plain value.
///
/// Returned by copy rather than by reference: the real pattern lives on the
/// audio thread, and handing out a reference to it would need a lock. At eight
/// bytes a step this is 512 bytes on the stack — far cheaper than the
/// synchronisation would be, and it gives the UI a snapshot that cannot change
/// underneath it mid-frame.
#[derive(Debug, Clone, Copy)]
pub struct Pattern {
    steps: [Step; MAX_STEPS],
    len: usize,
}

impl Pattern {
    pub fn new(steps: [Step; MAX_STEPS], len: usize) -> Self {
        Self {
            steps,
            len: len.min(MAX_STEPS),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl core::ops::Deref for Pattern {
    type Target = [Step];
    fn deref(&self) -> &[Step] {
        &self.steps[..self.len]
    }
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            steps: [Step::default(); MAX_STEPS],
            len: 0,
        }
    }
}

/// The knobs the pattern generator reads. A view of the `gen_*` fields of
/// [`Params`], gathered for convenience.
#[derive(Debug, Clone, Copy)]
pub struct GenerativeSettings {
    pub root: u8,
    pub scale: crate::scale::Scale,
    pub octave: i32,
    pub range: u32,
    pub density: f32,
    pub max_jump: f32,
    pub chord_bias: f32,
    pub length: usize,
}

impl GenerativeSettings {
    pub fn from_params(p: &Params) -> Self {
        Self {
            root: p.gen_root,
            scale: p.gen_scale,
            octave: p.gen_octave,
            range: p.gen_range,
            density: p.gen_density,
            max_jump: p.gen_max_jump,
            chord_bias: p.gen_chord_bias,
            length: p.seq_length,
        }
    }
}

/// What the sequencer wants the voice allocator to do this block.
///
/// At most one of each: a block is under a millisecond, and two steps cannot
/// fall inside one at any sane tempo.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SeqOutput {
    pub note_on: Option<(u8, f32)>,
    pub note_off: Option<u8>,
    /// True on the block where the sequencer moved to a new step, whether or
    /// not that step sounded. Good for syncing animation.
    pub stepped: bool,
}

/// A note waiting out its swing delay.
#[derive(Debug, Clone, Copy)]
struct Pending {
    note: u8,
    velocity: f32,
    samples_remaining: f32,
}

/// The sequencer.
#[derive(Debug, Clone)]
pub struct Sequencer {
    pub clock: Clock,
    pattern: [Step; MAX_STEPS],
    length: usize,
    position: usize,
    rng: Rng,

    /// The note currently sounding from the sequencer, if any.
    sounding: Option<u8>,
    /// Countdown to releasing it, from the gate length.
    samples_until_off: f32,
    pending: Option<Pending>,
}

impl Sequencer {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        Self {
            clock: Clock::new(sample_rate),
            pattern: [Step::default(); MAX_STEPS],
            length: 16,
            // Start before step 0 so the first advance lands on it.
            position: usize::MAX,
            rng: Rng::new(seed),
            sounding: None,
            samples_until_off: 0.0,
            pending: None,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.clock.set_sample_rate(sample_rate);
    }

    /// The step index currently playing.
    pub fn position(&self) -> usize {
        if self.position == usize::MAX {
            0
        } else {
            self.position
        }
    }

    pub fn pattern(&self) -> &[Step] {
        &self.pattern[..self.length]
    }

    pub fn set_step(&mut self, index: usize, step: Step) {
        if index < MAX_STEPS {
            self.pattern[index] = step;
        }
    }

    pub fn set_seed(&mut self, seed: u64) {
        self.rng = Rng::new(seed);
    }

    /// Rewinds to the start of the pattern.
    pub fn rewind(&mut self) {
        self.position = usize::MAX;
        self.pending = None;
    }

    /// Writes a fresh pattern.
    ///
    /// Deterministic given the RNG state, so the same seed always produces the
    /// same melody — which is what makes "that one was good, give it back"
    /// possible.
    pub fn regenerate(&mut self, gen: &GenerativeSettings) {
        let length = gen.length.clamp(1, MAX_STEPS);
        self.length = length;

        let quantizer = ScaleQuantizer::new(gen.root, gen.scale);
        let scale_len = gen.scale.len() as i32;
        // `range` counts octaves; the highest degree is one below the top of
        // the last one.
        let max_degree = (scale_len * gen.range as i32 - 1).max(0);
        let jump = gen.max_jump.round().clamp(1.0, 12.0) as i32;

        // Per-degree weights within one octave of the scale.
        let mut degree_weights = [0.0f32; 12];
        // Weights over the candidate degrees reachable from where we are.
        let mut candidate_weights = [0.0f32; 32];

        // Start on the root: a melody that starts on the tonic establishes the
        // key immediately.
        let mut degree: i32 = 0;

        for i in 0..length {
            // Every fourth step is a downbeat, assuming four steps to the bar.
            let strong = i % 4 == 0;

            // Step 0 always sounds, whatever the density: a pattern that starts
            // with a rest has no audible downbeat to lock onto.
            let sounds = i == 0 || self.rng.chance(gen.density);
            if !sounds {
                self.pattern[i] = Step {
                    active: false,
                    ..Default::default()
                };
                continue;
            }

            quantizer.degree_weights(gen.chord_bias, strong, &mut degree_weights);

            let low = (degree - jump).max(0);
            let high = (degree + jump).min(max_degree);
            let count = ((high - low + 1).max(1) as usize).min(candidate_weights.len());

            for (k, weight) in candidate_weights.iter_mut().enumerate().take(count) {
                let candidate = low + k as i32;
                let degree_index = candidate.rem_euclid(scale_len) as usize;

                // Melodies move by step far more often than they leap.
                let distance = (candidate - degree).abs() as f32;
                let proximity = 1.0 / (1.0 + distance * 0.6);

                // Discourage, but do not forbid, repeating the same note.
                let repetition = if candidate == degree { 0.35 } else { 1.0 };

                *weight = degree_weights[degree_index] * proximity * repetition;
            }

            let pick = self.rng.weighted(&candidate_weights[..count]);
            degree = low + pick as i32;

            let note = quantizer.degree_to_midi(degree, gen.octave);
            // Downbeats louder than offbeats. Dynamics are half of what makes a
            // line sound played rather than sequenced.
            let velocity = if strong {
                0.85 + self.rng.next_f32() * 0.15
            } else {
                0.5 + self.rng.next_f32() * 0.25
            };

            self.pattern[i] = Step {
                active: true,
                note,
                velocity,
                accent: strong,
            };
        }
    }

    /// Advances by a block of samples.
    pub fn advance(&mut self, samples: usize, p: &Params) -> SeqOutput {
        let mut out = SeqOutput::default();

        if p.clock_source == ClockSource::Internal {
            self.clock.set_tempo(p.tempo, p.steps_per_beat);
        }

        if self.length != p.seq_length.clamp(1, MAX_STEPS) {
            self.length = p.seq_length.clamp(1, MAX_STEPS);
            if self.position != usize::MAX && self.position >= self.length {
                self.position = 0;
            }
        }

        let samples_f = samples as f32;

        // Release the sounding note when its gate expires.
        if let Some(note) = self.sounding {
            self.samples_until_off -= samples_f;
            if self.samples_until_off <= 0.0 {
                out.note_off = Some(note);
                self.sounding = None;
            }
        }

        // A swung note waiting to start.
        if let Some(pending) = &mut self.pending {
            pending.samples_remaining -= samples_f;
            if pending.samples_remaining <= 0.0 {
                let pending = self.pending.take().expect("just checked");
                self.trigger(pending.note, pending.velocity, p, &mut out);
            }
        }

        let advance = self.clock.advance(samples, p.clock_source);
        for _ in 0..advance.steps {
            self.step(p, &mut out);
        }

        out
    }

    /// Called by the engine when an external MIDI clock tick arrives.
    pub fn on_midi_tick(&mut self, p: &Params) -> SeqOutput {
        let mut out = SeqOutput::default();
        if p.clock_source == ClockSource::ExternalMidi
            && self.clock.is_running()
            && self.clock.on_midi_tick(p.steps_per_beat)
        {
            self.step(p, &mut out);
        }
        out
    }

    /// Moves to the next step and triggers whatever is there.
    fn step(&mut self, p: &Params, out: &mut SeqOutput) {
        self.position = if self.position == usize::MAX {
            0
        } else {
            (self.position + 1) % self.length
        };
        out.stepped = true;

        let step = self.pattern[self.position];
        if !step.active {
            return;
        }

        // Swing delays every second step. It is what separates a groove from a
        // metronome, and a little goes a long way — 0.1 to 0.2 is most of the
        // useful range.
        let swing_delay = if self.position % 2 == 1 && p.seq_swing > 0.0 {
            p.seq_swing * self.clock.samples_per_step()
        } else {
            0.0
        };

        if swing_delay > 0.0 {
            self.pending = Some(Pending {
                note: step.note,
                velocity: step.velocity,
                samples_remaining: swing_delay,
            });
        } else {
            self.trigger(step.note, step.velocity, p, out);
        }
    }

    fn trigger(&mut self, note: u8, velocity: f32, p: &Params, out: &mut SeqOutput) {
        // Release whatever is sounding first, so the voice allocator sees a
        // clean note-off before the note-on. Without this, a gate above 1.0
        // would leak voices.
        if let Some(previous) = self.sounding.take() {
            out.note_off = Some(previous);
        }

        out.note_on = Some((note, velocity));
        self.sounding = Some(note);
        self.samples_until_off = p.seq_gate * self.clock.samples_per_step();
    }

    /// Releases anything the sequencer is holding. Call when the transport
    /// stops, or a note hangs forever.
    pub fn release_all(&mut self) -> Option<u8> {
        self.pending = None;
        self.sounding.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scale::Scale;
    use crate::BLOCK;

    fn settings() -> GenerativeSettings {
        GenerativeSettings {
            root: 0,
            scale: Scale::MinorPentatonic,
            octave: 3,
            range: 2,
            density: 0.8,
            max_jump: 3.0,
            chord_bias: 0.6,
            length: 16,
        }
    }

    #[test]
    fn every_generated_note_is_in_key() {
        for scale in Scale::ALL {
            for root in 0..12u8 {
                let mut s = Sequencer::new(48000.0, 1234);
                let mut gen = settings();
                gen.scale = scale;
                gen.root = root;
                s.regenerate(&gen);

                let quantizer = ScaleQuantizer::new(root, scale);
                for step in s.pattern() {
                    if step.active {
                        assert!(
                            quantizer.contains(step.note),
                            "{scale:?} root {root}: note {} is out of key",
                            step.note
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn generated_notes_stay_in_the_requested_range() {
        let mut s = Sequencer::new(48000.0, 99);
        let mut gen = settings();
        gen.octave = 4;
        gen.range = 2;
        s.regenerate(&gen);

        let low = (gen.octave + 1) * 12; // C4 = 60
        let high = low + 24;
        for step in s.pattern() {
            if step.active {
                assert!(
                    (step.note as i32) >= low && (step.note as i32) <= high,
                    "note {} outside octaves {}..{}",
                    step.note,
                    gen.octave,
                    gen.octave + gen.range as i32
                );
            }
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_melody() {
        let gen = settings();
        let mut a = Sequencer::new(48000.0, 777);
        let mut b = Sequencer::new(48000.0, 777);
        a.regenerate(&gen);
        b.regenerate(&gen);
        assert_eq!(a.pattern(), b.pattern());

        let mut c = Sequencer::new(48000.0, 778);
        c.regenerate(&gen);
        assert_ne!(a.pattern(), c.pattern(), "different seeds should differ");
    }

    #[test]
    fn density_controls_how_many_steps_sound() {
        let count_at = |density: f32| {
            let mut s = Sequencer::new(48000.0, 5);
            let mut gen = settings();
            gen.density = density;
            gen.length = 64;
            s.regenerate(&gen);
            s.pattern().iter().filter(|s| s.active).count()
        };
        assert!(count_at(1.0) == 64);
        let sparse = count_at(0.25);
        assert!(sparse > 0 && sparse < 32, "sparse pattern had {sparse} notes");
    }

    #[test]
    fn the_first_step_always_sounds() {
        for seed in 0..50u64 {
            let mut s = Sequencer::new(48000.0, seed);
            let mut gen = settings();
            gen.density = 0.05;
            s.regenerate(&gen);
            assert!(s.pattern()[0].active, "seed {seed} started with a rest");
        }
    }

    /// The proximity weighting should keep the line walking rather than
    /// scattering. Average interval well under an octave is the check.
    #[test]
    fn the_melody_walks_rather_than_leaping() {
        let mut s = Sequencer::new(48000.0, 31337);
        let mut gen = settings();
        gen.length = 64;
        gen.density = 1.0;
        s.regenerate(&gen);

        let notes: Vec<i32> = s.pattern().iter().map(|s| s.note as i32).collect();
        let total: i32 = notes.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        let average = total as f32 / (notes.len() - 1) as f32;
        assert!(
            average < 5.0,
            "average interval was {average} semitones, too jumpy"
        );
    }

    #[test]
    fn downbeats_favour_chord_tones() {
        let mut s = Sequencer::new(48000.0, 4242);
        let mut gen = settings();
        gen.scale = Scale::Major;
        gen.length = 64;
        gen.density = 1.0;
        gen.chord_bias = 1.0;
        s.regenerate(&gen);

        let quantizer = ScaleQuantizer::new(0, Scale::Major);
        let chord_pitch_classes: Vec<u8> = Scale::Major
            .chord_degrees()
            .iter()
            .map(|&d| Scale::Major.intervals()[d])
            .collect();

        let mut downbeat_hits = 0;
        let mut downbeats = 0;
        for (i, step) in s.pattern().iter().enumerate() {
            if i % 4 == 0 && step.active {
                downbeats += 1;
                let pc = (step.note as i32 - quantizer.root as i32).rem_euclid(12) as u8;
                if chord_pitch_classes.contains(&pc) {
                    downbeat_hits += 1;
                }
            }
        }
        let ratio = downbeat_hits as f32 / downbeats as f32;
        assert!(
            ratio > 0.6,
            "only {}/{} downbeats landed on chord tones",
            downbeat_hits,
            downbeats
        );
    }

    #[test]
    fn playing_produces_matched_note_ons_and_offs() {
        let mut s = Sequencer::new(48000.0, 8);
        let gen = settings();
        s.regenerate(&gen);

        let mut p = Params::default();
        p.tempo = 120.0;
        p.steps_per_beat = 4.0;
        p.seq_gate = 0.5;
        s.clock.start();

        let mut ons = 0;
        let mut offs = 0;
        // Four seconds of audio.
        for _ in 0..(48000 * 4 / BLOCK) {
            let out = s.advance(BLOCK, &p);
            if out.note_on.is_some() {
                ons += 1;
            }
            if out.note_off.is_some() {
                offs += 1;
            }
        }

        assert!(ons > 0, "sequencer never played");
        // Every note is released; at most one may still be sounding at the end.
        assert!(
            (ons - offs) <= 1,
            "{ons} note-ons but only {offs} note-offs: notes are hanging"
        );
    }

    #[test]
    fn a_stopped_sequencer_is_silent() {
        let mut s = Sequencer::new(48000.0, 8);
        s.regenerate(&settings());
        let p = Params::default();
        // Never started the clock.
        for _ in 0..10_000 {
            assert_eq!(s.advance(BLOCK, &p), SeqOutput::default());
        }
    }

    #[test]
    fn swing_delays_the_offbeats() {
        let mut s = Sequencer::new(48000.0, 3);
        let mut gen = settings();
        gen.density = 1.0;
        s.regenerate(&gen);

        let mut p = Params::default();
        p.tempo = 120.0;
        p.steps_per_beat = 4.0;
        p.seq_swing = 0.3;
        s.clock.start();

        let mut on_times = Vec::new();
        for block in 0..(48000 * 2 / BLOCK) {
            if s.advance(BLOCK, &p).note_on.is_some() {
                on_times.push(block * BLOCK);
            }
        }

        assert!(on_times.len() > 8);
        // With swing, alternate gaps should be long-short rather than even.
        let gaps: Vec<usize> = on_times.windows(2).map(|w| w[1] - w[0]).collect();
        let long: Vec<usize> = gaps.iter().step_by(2).copied().collect();
        let short: Vec<usize> = gaps.iter().skip(1).step_by(2).copied().collect();
        let mean = |v: &[usize]| v.iter().sum::<usize>() as f32 / v.len() as f32;
        assert!(
            mean(&long) > mean(&short) * 1.3,
            "swing did not lengthen alternate steps: {} vs {}",
            mean(&long),
            mean(&short)
        );
    }
}
