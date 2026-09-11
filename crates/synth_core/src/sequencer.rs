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

use crate::clock::{Advance, ClockView};
use crate::params::Params;
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
    /// Downbeat marker for the lead's visuals; audible on the bass channel,
    /// where it drives cutoff and level.
    pub accent: bool,
    /// Ties this step to the one before it: the bass glides into it without
    /// retriggering. The lead ignores it.
    pub slide: bool,
}

impl Default for Step {
    fn default() -> Self {
        Self {
            active: false,
            note: 60,
            velocity: 0.8,
            accent: false,
            slide: false,
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
    /// Chance a step is tied to the one before it. `0.0` skips the draw
    /// entirely, which is what keeps the lead's RNG stream unchanged.
    pub slide_chance: f32,
    /// Chance a step is accented on top of the downbeats it already gets.
    /// `0.0` skips the draw, as above.
    pub accent_chance: f32,
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
            // The lead has neither control. Zero here is load-bearing: it is
            // what makes the two draws below short-circuit, leaving the lead's
            // RNG stream byte-identical and `GOLDEN_HASH` intact.
            slide_chance: 0.0,
            accent_chance: 0.0,
        }
    }

    /// The bass's generator.
    ///
    /// Key and scale come from the lead: a bass in a different key from the
    /// melody is a bug, not a feature, and splitting them later is a one-line
    /// change if it is ever wanted. Everything that actually distinguishes a
    /// bass line — register, range, how busy it is, how far it leaps — is the
    /// bass's own.
    pub fn for_bass(p: &Params) -> Self {
        Self {
            root: p.gen_root,
            scale: p.gen_scale,
            octave: p.bass_gen_octave,
            range: p.bass_gen_range,
            density: p.bass_gen_density,
            max_jump: p.bass_gen_max_jump,
            chord_bias: p.bass_gen_chord_bias,
            length: p.bass_seq_length,
            slide_chance: p.bass_slide_chance,
            accent_chance: p.bass_accent_chance,
        }
    }
}

/// The three per-line settings the sequencer used to read straight out of the
/// global parameter block.
///
/// The engine runs two sequencers — the lead and the bass — off one clock.
/// Reading `Params` inside `advance` would hand both the same length, swing
/// and gate, which is precisely the coupling a second line exists to avoid.
/// Passing a small block instead lets each caller choose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeqSettings {
    /// Loop length in steps. Clamped to `1..=MAX_STEPS` by the reader.
    pub length: usize,
    /// Delay on every second step, as a fraction of a step.
    pub swing: f32,
    /// Note length, as a fraction of a step.
    pub gate: f32,
}

impl SeqSettings {
    /// The lead's settings.
    pub fn from_params(p: &Params) -> Self {
        Self {
            length: p.seq_length,
            swing: p.seq_swing,
            gate: p.seq_gate,
        }
    }

    /// The bass's sequencer settings.
    ///
    /// Its own length, the lead's groove: two lines that swing differently
    /// against one clock is not an arrangement, it is a mistake.
    pub fn for_bass(p: &Params) -> Self {
        Self {
            length: p.bass_seq_length,
            swing: p.seq_swing,
            gate: p.seq_gate,
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
    /// Whether the note in `note_on` came from an accented step.
    pub accent: bool,
    /// Whether the note in `note_on` is tied to the note before it.
    pub slide: bool,
}

/// A note waiting out its swing delay.
#[derive(Debug, Clone, Copy)]
struct Pending {
    note: u8,
    velocity: f32,
    accent: bool,
    slide: bool,
    /// The step after this one is a tie, so ignore the gate and hold.
    hold: bool,
    samples_remaining: f32,
}

/// The sequencer.
#[derive(Debug, Clone)]
pub struct Sequencer {
    pattern: [Step; MAX_STEPS],
    length: usize,
    position: usize,
    rng: Rng,

    /// The note currently sounding from the sequencer, if any.
    sounding: Option<u8>,
    /// Countdown to releasing it, from the gate length.
    samples_until_off: f32,
    pending: Option<Pending>,

    /// A pattern the control side has loaded, waiting for the top of the loop.
    queued: Option<Pattern>,
    /// Set when a queued pattern lands, so the caller knows to republish it.
    pattern_changed: bool,
    /// The requested length from the previous block, so the reconcile in
    /// `advance` can tell a knob turn from a length that merely differs.
    last_length_param: usize,
}

impl Sequencer {
    pub fn new(seed: u64) -> Self {
        Self {
            pattern: [Step::default(); MAX_STEPS],
            length: 16,
            // Start before step 0 so the first advance lands on it.
            position: usize::MAX,
            rng: Rng::new(seed),
            sounding: None,
            samples_until_off: 0.0,
            pending: None,
            queued: None,
            pattern_changed: false,
            last_length_param: 16,
        }
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

    /// Loads a pattern, to take effect at the top of the next loop.
    ///
    /// Swapping mid-phrase moves the melody under the ear halfway through a
    /// bar. Waiting for the wrap puts the change where a listener is expecting
    /// one anyway. With the transport stopped no wrap is coming, so `advance`
    /// applies it at once instead.
    pub fn queue_pattern(&mut self, pattern: Pattern) {
        self.queued = Some(pattern);
    }

    /// Reports, once, that a loaded pattern has landed. The caller republishes
    /// the pattern on the strength of it, so a second `true` would be a wasted
    /// copy and a missed one would leave a stale display.
    pub fn take_pattern_changed(&mut self) -> bool {
        core::mem::replace(&mut self.pattern_changed, false)
    }

    /// Adopts a loaded pattern, length and all. A pattern is saved with the
    /// loop length it was written at, and the two belong together: half a
    /// melody looping is not the melody.
    fn apply_pattern(&mut self, pattern: &Pattern) {
        self.pattern[..pattern.len()].copy_from_slice(pattern);
        // An empty pattern would leave nothing to play, and a modulo by zero
        // to do it with.
        self.length = pattern.len().max(1);
        if self.position != usize::MAX && self.position >= self.length {
            self.position = 0;
        }
        self.pattern_changed = true;
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

            // Both draws short-circuit at 0.0. This is not a micro-
            // optimisation: an unconditional draw would advance the RNG on
            // every step of every lead pattern and change every melody the
            // synth has ever generated.
            let mut accent = strong;
            if gen.accent_chance > 0.0 && !accent {
                accent = self.rng.chance(gen.accent_chance);
            }
            let slide = gen.slide_chance > 0.0 && self.rng.chance(gen.slide_chance);

            self.pattern[i] = Step { active: true, note, velocity, accent, slide };
        }
    }

    /// Advances by a block of samples.
    ///
    /// `samples` cannot be inferred from `adv`: the gate and swing countdowns
    /// are measured in samples, and a block that crosses no step boundary
    /// still has to tick them down.
    pub fn advance(
        &mut self,
        samples: usize,
        adv: Advance,
        clock: ClockView,
        s: &SeqSettings,
    ) -> SeqOutput {
        let mut out = SeqOutput::default();

        // Only when the knob actually moves. Comparing against our own length
        // instead would fight a loaded pattern, whose length comes from the
        // pattern rather than from this block's snapshot of the parameters.
        let requested_length = s.length.clamp(1, MAX_STEPS);
        if requested_length != self.last_length_param {
            self.last_length_param = requested_length;
            self.length = requested_length;
            if self.position != usize::MAX && self.position >= self.length {
                self.position = 0;
            }
            // The steps on show changed even though none of them was edited.
            // Without this the mirror keeps the old length and the control
            // side reads slots the engine never published.
            self.pattern_changed = true;
        }

        // Stopped, there is no bar line to wait for. Checked here rather than
        // in `queue_pattern` so that both orderings land: loaded while stopped,
        // and loaded while playing and then stopped before the wrap came.
        if !clock.running {
            if let Some(pattern) = self.queued.take() {
                self.apply_pattern(&pattern);
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
                self.trigger(
                    pending.note,
                    pending.velocity,
                    pending.accent,
                    pending.slide,
                    pending.hold,
                    s,
                    clock,
                    &mut out,
                );
            }
        }

        for _ in 0..adv.steps {
            self.step(s, clock, &mut out);
        }

        out
    }

    /// Called by the engine when an external MIDI clock tick arrives.
    ///
    /// Whether the tick lands on a step is the engine's decision — it owns the
    /// clock and the clock source — so the answer arrives as an argument.
    pub fn on_midi_tick(&mut self, ticked: bool, clock: ClockView, s: &SeqSettings) -> SeqOutput {
        let mut out = SeqOutput::default();
        if ticked {
            self.step(s, clock, &mut out);
        }
        out
    }

    /// Moves to the next step and triggers whatever is there.
    fn step(&mut self, s: &SeqSettings, clock: ClockView, out: &mut SeqOutput) {
        let next = if self.position == usize::MAX {
            0
        } else {
            (self.position + 1) % self.length
        };
        // Swap at the top of the loop, before the step is read, so the new
        // pattern is heard from its own first note rather than from wherever
        // the old one happened to be.
        if next == 0 {
            if let Some(pattern) = self.queued.take() {
                self.apply_pattern(&pattern);
            }
        }
        self.position = next;
        out.stepped = true;

        let step = self.pattern[self.position];
        if !step.active {
            return;
        }

        // A tie needs the note before it still sounding when it lands, or the
        // glide has nothing to glide from. Look one step ahead: hold this note
        // through the whole step rather than releasing it at the gate. Two
        // steps rather than one, so the maximum swing delay of 0.75 is covered
        // and the note still releases if the tie is edited away mid-flight.
        let following = self.pattern[(self.position + 1) % self.length];
        let hold = following.active && following.slide;

        // Swing delays every second step. It is what separates a groove from a
        // metronome, and a little goes a long way — 0.1 to 0.2 is most of the
        // useful range.
        let swing_delay = if self.position % 2 == 1 && s.swing > 0.0 {
            s.swing * clock.samples_per_step
        } else {
            0.0
        };

        if swing_delay > 0.0 {
            self.pending = Some(Pending {
                note: step.note,
                velocity: step.velocity,
                accent: step.accent,
                slide: step.slide,
                hold,
                samples_remaining: swing_delay,
            });
        } else {
            self.trigger(step.note, step.velocity, step.accent, step.slide, hold, s, clock, out);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn trigger(
        &mut self,
        note: u8,
        velocity: f32,
        accent: bool,
        slide: bool,
        hold: bool,
        s: &SeqSettings,
        clock: ClockView,
        out: &mut SeqOutput,
    ) {
        // Release whatever is sounding first, so the voice allocator sees a
        // clean note-off before the note-on. Without this, a gate above 1.0
        // would leak voices.
        if let Some(previous) = self.sounding.take() {
            out.note_off = Some(previous);
        }

        out.note_on = Some((note, velocity));
        out.accent = accent;
        out.slide = slide;
        self.sounding = Some(note);
        self.samples_until_off = if hold {
            2.0 * clock.samples_per_step
        } else {
            s.gate * clock.samples_per_step
        };
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
    use crate::clock::{Clock, ClockSource};
    use crate::scale::Scale;
    use crate::BLOCK;

    /// Drives a sequencer for one block from a clock the test owns.
    ///
    /// The engine does this for real; the tests need the same wiring in one
    /// line, so that hoisting the clock does not turn into a rewrite of every
    /// test.
    fn block(s: &mut Sequencer, c: &mut Clock, settings: &SeqSettings) -> SeqOutput {
        // Tempo/steps-per-beat are fixed to the default here; callers cannot
        // change this helper's transport by mutating their own `Params`.
        let p = Params::default();
        if p.clock_source == ClockSource::Internal {
            c.set_tempo(p.tempo, p.steps_per_beat);
        }
        let adv = c.advance(BLOCK, p.clock_source);
        s.advance(BLOCK, adv, c.view(), settings)
    }

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
            slide_chance: 0.0,
            accent_chance: 0.0,
        }
    }

    #[test]
    fn every_generated_note_is_in_key() {
        for scale in Scale::ALL {
            for root in 0..12u8 {
                let mut s = Sequencer::new(1234);
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
        let mut s = Sequencer::new(99);
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
        let mut a = Sequencer::new(777);
        let mut b = Sequencer::new(777);
        a.regenerate(&gen);
        b.regenerate(&gen);
        assert_eq!(a.pattern(), b.pattern());

        let mut c = Sequencer::new(778);
        c.regenerate(&gen);
        assert_ne!(a.pattern(), c.pattern(), "different seeds should differ");
    }

    #[test]
    fn density_controls_how_many_steps_sound() {
        let count_at = |density: f32| {
            let mut s = Sequencer::new(5);
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
            let mut s = Sequencer::new(seed);
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
        let mut s = Sequencer::new(31337);
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
        let mut s = Sequencer::new(4242);
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
        let mut s = Sequencer::new(8);
        let gen = settings();
        s.regenerate(&gen);

        let mut p = Params::default();
        p.seq_gate = 0.5;
        let settings = SeqSettings::from_params(&p);
        let mut c = Clock::new(48000.0);
        c.start();

        let mut ons = 0;
        let mut offs = 0;
        // Four seconds of audio.
        for _ in 0..(48000 * 4 / BLOCK) {
            let out = block(&mut s, &mut c, &settings);
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
        let mut s = Sequencer::new(8);
        s.regenerate(&settings());
        let p = Params::default();
        let settings = SeqSettings::from_params(&p);
        let mut c = Clock::new(48000.0);
        // Never started the clock.
        for _ in 0..10_000 {
            assert_eq!(block(&mut s, &mut c, &settings), SeqOutput::default());
        }
    }

    #[test]
    fn swing_delays_the_offbeats() {
        let mut s = Sequencer::new(3);
        let mut gen = settings();
        gen.density = 1.0;
        s.regenerate(&gen);

        let mut p = Params::default();
        p.seq_swing = 0.3;
        let settings = SeqSettings::from_params(&p);
        let mut c = Clock::new(48000.0);
        c.start();

        let mut on_times = Vec::new();
        for blk in 0..(48000 * 2 / BLOCK) {
            if block(&mut s, &mut c, &settings).note_on.is_some() {
                on_times.push(blk * BLOCK);
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

    /// A pattern of nothing but `note`, for telling a loaded pattern apart from
    /// a generated one. The generator here works in octave 3 and up, so a note
    /// this low can only have come from the load.
    fn marker_pattern(note: u8, len: usize) -> Pattern {
        let steps = [Step {
            active: true,
            note,
            velocity: 1.0,
            accent: false,
            slide: false,
        }; MAX_STEPS];
        Pattern::new(steps, len)
    }

    /// Runs the sequencer until `stop` says so, giving up rather than hanging
    /// if the condition never comes true.
    fn run_until(
        s: &mut Sequencer,
        c: &mut Clock,
        settings: &SeqSettings,
        what: &str,
        stop: impl Fn(&Sequencer) -> bool,
    ) {
        for _ in 0..100_000 {
            if stop(s) {
                return;
            }
            block(s, c, settings);
        }
        panic!("gave up waiting for {what}");
    }

    /// Swapping patterns mid-phrase moves the melody under the ear halfway
    /// through a bar. The swap waits for the top of the loop, where a listener
    /// is expecting a change anyway.
    #[test]
    fn a_queued_pattern_waits_for_the_bar_line() {
        let mut s = Sequencer::new(8);
        s.regenerate(&settings());
        let before = s.pattern().to_vec();

        let p = Params::default();
        let settings = SeqSettings::from_params(&p);
        let mut c = Clock::new(48000.0);
        c.start();

        // Get off step zero first, so the wrap we are waiting for is a real one.
        run_until(&mut s, &mut c, &settings, "the first step", |s| s.position() == 1);

        s.queue_pattern(marker_pattern(12, 16));
        block(&mut s, &mut c, &settings);
        assert_eq!(s.pattern(), before.as_slice(), "swapped mid-loop");
        assert!(!s.take_pattern_changed());

        run_until(&mut s, &mut c, &settings, "the bar line", |s| s.position() == 0);
        assert!(
            s.pattern().iter().all(|step| step.note == 12),
            "did not swap at the bar line"
        );
        assert!(s.take_pattern_changed(), "the swap went unannounced");
        assert!(!s.take_pattern_changed(), "the flag did not clear");
    }

    /// With the transport stopped there is no bar line coming, so a queued
    /// pattern lands at once — including one queued a moment before the stop.
    #[test]
    fn a_queued_pattern_lands_at_once_when_stopped() {
        let mut s = Sequencer::new(8);
        s.regenerate(&settings());
        let p = Params::default();
        let settings = SeqSettings::from_params(&p);
        let mut c = Clock::new(48000.0);

        s.queue_pattern(marker_pattern(12, 8));
        block(&mut s, &mut c, &settings);

        assert_eq!(s.pattern().len(), 8, "the pattern brought its own length");
        assert!(s.pattern().iter().all(|step| step.note == 12));
        assert!(s.take_pattern_changed());
    }

    /// Clicking through slots faster than the bar goes round. Only the last one
    /// should ever be heard.
    #[test]
    fn the_last_queued_pattern_wins() {
        let mut s = Sequencer::new(8);
        s.regenerate(&settings());

        let p = Params::default();
        let settings = SeqSettings::from_params(&p);
        let mut c = Clock::new(48000.0);
        c.start();
        run_until(&mut s, &mut c, &settings, "the first step", |s| s.position() == 1);

        s.queue_pattern(marker_pattern(12, 16));
        s.queue_pattern(marker_pattern(24, 16));

        run_until(&mut s, &mut c, &settings, "the bar line", |s| s.position() == 0);
        assert!(s.pattern().iter().all(|step| step.note == 24));
    }

    #[test]
    fn output_carries_accent_and_slide_from_the_step() {
        let mut s = Sequencer::new(0x51DE_0001);
        s.set_step(
            1,
            Step { active: true, note: 40, velocity: 0.9, accent: true, slide: true },
        );
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params { seq_length: 4, seq_swing: 0.0, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        // Run until the sequencer lands on step 1 and fires it.
        let mut fired = None;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if out.note_on == Some((40, 0.9)) {
                fired = Some(out);
                break;
            }
        }
        let out = fired.expect("step 1 should have triggered");
        assert!(out.accent, "an accented step must report accent");
        assert!(out.slide, "a tied step must report slide");
    }

    #[test]
    fn swung_notes_keep_their_accent_and_slide() {
        // Swing routes odd steps through `Pending`, which is exactly where the
        // step's flags used to be dropped.
        let mut s = Sequencer::new(0x51DE_0002);
        s.set_step(
            1,
            Step { active: true, note: 41, velocity: 0.9, accent: true, slide: true },
        );
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params { seq_length: 4, seq_swing: 0.4, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        let mut fired = None;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if out.note_on == Some((41, 0.9)) {
                fired = Some(out);
                break;
            }
        }
        let out = fired.expect("swung step 1 should have triggered");
        assert!(out.accent, "swing must not lose the accent flag");
        assert!(out.slide, "swing must not lose the slide flag");
    }

    #[test]
    fn settings_come_from_the_argument_not_the_global_params() {
        // Two sequencers, one clock, different lengths: the whole point of
        // lifting these three fields out of `Params`.
        let mut lead = Sequencer::new(0x51DE_0003);
        let mut bass = Sequencer::new(0x51DE_0003);
        for i in 0..16 {
            let step = Step { active: true, note: 40 + i as u8, velocity: 0.8, accent: false, slide: false };
            lead.set_step(i, step);
            bass.set_step(i, step);
        }
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params::default();
        let long = SeqSettings { length: 16, ..SeqSettings::from_params(&p) };
        let short = SeqSettings { length: 2, ..SeqSettings::from_params(&p) };

        let mut lead_max = 0usize;
        let mut bass_max = 0usize;
        for _ in 0..20_000 {
            let adv = c.advance(BLOCK, p.clock_source);
            let view = c.view();
            lead.advance(BLOCK, adv, view, &long);
            bass.advance(BLOCK, adv, view, &short);
            lead_max = lead_max.max(lead.position());
            bass_max = bass_max.max(bass.position());
        }
        assert!(lead_max > 8, "the long sequencer should reach the back half");
        assert_eq!(bass_max, 1, "the short sequencer must loop within two steps");
    }

    /// Generative settings for the tests below, with the two new chances
    /// spelled out so their effect is never accidental.
    fn gen_with(slide_chance: f32, accent_chance: f32) -> GenerativeSettings {
        GenerativeSettings {
            root: 0,
            scale: Scale::NaturalMinor,
            octave: 3,
            range: 2,
            density: 0.9,
            max_jump: 3.0,
            chord_bias: 0.6,
            length: 16,
            slide_chance,
            accent_chance,
        }
    }

    #[test]
    fn zero_chances_produce_no_slides_and_only_downbeat_accents() {
        let mut s = Sequencer::new(0x6EA5_0001);
        s.regenerate(&gen_with(0.0, 0.0));
        for (i, step) in s.pattern().iter().enumerate() {
            assert!(!step.slide, "step {i} was marked for slide at chance 0.0");
            assert_eq!(
                step.accent,
                i % 4 == 0,
                "step {i} accent should still be the downbeat marker alone"
            );
        }
    }

    #[test]
    fn nonzero_chances_draw_from_the_rng() {
        // The short-circuit is the whole reason the lead's stream is
        // unchanged. Prove the draw really is skipped at 0.0 by showing it
        // is *not* skipped above it: the extra draws shift the stream, so
        // the same seed yields different notes.
        let mut zero = Sequencer::new(0x6EA5_0002);
        zero.regenerate(&gen_with(0.0, 0.0));
        let zero_notes: Vec<u8> = zero.pattern().iter().map(|s| s.note).collect();

        let mut accented = Sequencer::new(0x6EA5_0002);
        accented.regenerate(&gen_with(0.0, 0.5));
        let accented_notes: Vec<u8> = accented.pattern().iter().map(|s| s.note).collect();

        assert_ne!(
            zero_notes, accented_notes,
            "a nonzero accent chance must consume RNG, shifting the melody"
        );

        let mut slid = Sequencer::new(0x6EA5_0002);
        slid.regenerate(&gen_with(1.0, 0.0));
        assert!(
            slid.pattern().iter().filter(|s| s.active).all(|s| s.slide),
            "at chance 1.0 every sounding step should be tied"
        );
    }

    #[test]
    fn a_tie_holds_the_note_before_it_past_the_gate() {
        let mut s = Sequencer::new(0x6EA5_0003);
        for i in 0..4 {
            s.set_step(
                i,
                Step { active: true, note: 40, velocity: 0.8, accent: false, slide: i == 1 },
            );
        }
        let mut c = Clock::new(48_000.0);
        c.start();
        // A short gate: without the lookahead, step 0 releases well before
        // step 1 arrives.
        let p = Params { seq_length: 4, seq_swing: 0.0, seq_gate: 0.2, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        let mut seen_first_on = false;
        let mut off_before_tie = false;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if out.note_on.is_some() {
                if seen_first_on {
                    // This is the tie. Stop here.
                    assert!(out.slide, "step 1 is the tie");
                    break;
                }
                seen_first_on = true;
                continue;
            }
            if seen_first_on && out.note_off.is_some() {
                off_before_tie = true;
            }
        }
        assert!(
            !off_before_tie,
            "the gate must not expire between a note and the tie that follows it"
        );
    }

    #[test]
    fn an_untied_step_still_honours_the_gate() {
        let mut s = Sequencer::new(0x6EA5_0004);
        for i in 0..4 {
            s.set_step(
                i,
                Step { active: true, note: 40, velocity: 0.8, accent: false, slide: false },
            );
        }
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params { seq_length: 4, seq_swing: 0.0, seq_gate: 0.2, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        let mut seen_first_on = false;
        let mut off_before_next = false;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if seen_first_on && out.note_off.is_some() {
                off_before_next = true;
            }
            if out.note_on.is_some() {
                if seen_first_on {
                    break;
                }
                seen_first_on = true;
            }
        }
        assert!(
            off_before_next,
            "with no tie, a 0.2 gate must release long before the next step"
        );
    }
}
