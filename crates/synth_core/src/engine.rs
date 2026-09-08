//! The synth engine: voice allocation, event handling and the output stage.
//!
//! This is what the audio callback calls. Everything below it is real-time
//! safe — no allocation, no locks, no syscalls — so all storage is allocated
//! once in [`Engine::new`] and reused forever after.
//!
//! # Voice allocation
//!
//! Polyphony is not just "grab a free voice". The interesting cases are what
//! happens when there is no free voice, and what mono mode does when notes
//! overlap; both are handled below and both are what make a synth feel like an
//! instrument rather than a sample player.

use std::sync::Arc;

use crate::event::{Consumer, Event};
use crate::lfo::Lfo;
use crate::params::{Params, SharedParams, Smoothed, VoiceMode};
use crate::sequencer::{GenerativeSettings, Sequencer};
use crate::voice::Voice;
use crate::BLOCK;

/// Hard ceiling on polyphony. Voices are preallocated, so this is a memory
/// cost, not a runtime one; the live limit is `Params::max_voices`.
pub const MAX_VOICES: usize = 32;

/// The most notes that can be physically held at once.
const MAX_HELD: usize = 128;

/// The complete synthesizer.
pub struct Engine {
    sample_rate: f32,
    params: Arc<SharedParams>,
    /// Event sources, drained in order every block.
    ///
    /// A `Vec` rather than one queue because the sources are genuinely
    /// independent: the game thread pushes note and parameter events, a MIDI
    /// thread pushes keyboard and clock events on its own timing. Each gets its
    /// own single-producer queue, which stays lock-free and cheap; funnelling
    /// both through one queue would need a multi-producer structure and buy
    /// nothing.
    events: Vec<Consumer>,

    voices: Vec<Voice>,
    lfo: Lfo,
    sequencer: Sequencer,

    /// Increments on every note-on; the lowest value is the oldest voice.
    age_counter: u64,

    /// Notes currently held on the keyboard, oldest first. Mono mode falls back
    /// through this when a note is released, which is what lets you trill by
    /// holding one note and tapping another.
    held: Vec<u8>,

    master_gain: Smoothed,
    /// One-pole state for the output DC blocker.
    dc_x1: f32,
    dc_y1: f32,
    dc_coef: f32,

    /// Scratch buffer for one block of voice output.
    block: Vec<f32>,

    last_voice_mode: VoiceMode,
    last_regenerate: u32,
    peak: f32,
}

impl Engine {
    pub fn new(sample_rate: f32, params: Arc<SharedParams>, events: Consumer) -> Self {
        Self::with_sources(sample_rate, params, vec![events])
    }

    /// Builds an engine that drains several event queues.
    pub fn with_sources(
        sample_rate: f32,
        params: Arc<SharedParams>,
        events: Vec<Consumer>,
    ) -> Self {
        let mut voices = Vec::with_capacity(MAX_VOICES);
        for i in 0..MAX_VOICES {
            voices.push(Voice::new(sample_rate, i as u64 + 1));
        }

        let control_rate = sample_rate / BLOCK as f32;
        let snapshot = params.snapshot();

        let mut sequencer = Sequencer::new(sample_rate, params.gen_seed.load(core::sync::atomic::Ordering::Relaxed));
        sequencer.regenerate(&GenerativeSettings::from_params(&snapshot));

        let mut engine = Self {
            sample_rate,
            params,
            events,
            voices,
            lfo: Lfo::new(sample_rate, 0xA5A5),
            sequencer,
            age_counter: 1,
            held: Vec::with_capacity(MAX_HELD),
            master_gain: Smoothed::new(snapshot.master_gain, 15.0, control_rate),
            dc_x1: 0.0,
            dc_y1: 0.0,
            // ~10 Hz corner: removes DC and subsonic rumble without touching
            // the bottom of the audible range.
            dc_coef: 1.0 - (2.0 * core::f32::consts::PI * 10.0 / sample_rate),
            block: vec![0.0; BLOCK],
            last_voice_mode: snapshot.voice_mode,
            last_regenerate: 0,
            peak: 0.0,
        };
        engine.sequencer.clock.set_tempo(snapshot.tempo, snapshot.steps_per_beat);
        engine.publish_pattern();
        engine
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn params(&self) -> &Arc<SharedParams> {
        &self.params
    }

    /// Reconfigures for a new sample rate. Not real-time safe: call from the
    /// setup path, not from the callback.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        for voice in &mut self.voices {
            voice.set_sample_rate(sample_rate);
        }
        self.lfo.set_sample_rate(sample_rate);
        self.sequencer.set_sample_rate(sample_rate);
        self.dc_coef = 1.0 - (2.0 * core::f32::consts::PI * 10.0 / sample_rate);
        self.master_gain
            .set_time(15.0, sample_rate / BLOCK as f32);
    }

    /// Current sequencer pattern, for display.
    pub fn pattern(&self) -> &[crate::sequencer::Step] {
        self.sequencer.pattern()
    }

    /// Fills `out` with mono audio. This is the audio callback's entry point.
    pub fn process(&mut self, out: &mut [f32]) {
        let params = self.params.snapshot();

        // Handle a mono/poly switch first. Doing it after draining events would
        // silence the very notes those events just triggered, because the reset
        // cannot tell a new note from a stale one.
        if params.voice_mode != self.last_voice_mode {
            // Switching mid-note would otherwise leave orphaned voices sounding
            // with no way to release them: a poly voice has no owner in mono.
            self.all_notes_off(true);
            self.last_voice_mode = params.voice_mode;
        }

        self.drain_events(&params);

        // The LFO knob and the mod wheel add, so a patch can be static until
        // the player asks for movement, and the wheel can always reach full
        // depth regardless of where the knob sits.
        let mod_depth = (params.lfo_depth + params.mod_wheel).min(1.0);
        self.lfo.set_rate(params.lfo_rate);
        self.master_gain.set_target(params.master_gain);

        for chunk in out.chunks_mut(BLOCK) {
            let len = chunk.len();

            let seq = self.sequencer.advance(len, &params);
            if let Some(note) = seq.note_off {
                self.note_off(note, &params);
            }
            if let Some((note, velocity)) = seq.note_on {
                self.note_on(note, velocity, &params);
            }

            let lfo = self.lfo.next_block(params.lfo_wave, len);

            let block = &mut self.block[..len];
            block.fill(0.0);
            for voice in self.voices.iter_mut().take(params.max_voices) {
                voice.process_block(block, &params, lfo, mod_depth);
            }

            let gain = self.master_gain.next();

            // The DC blocker's state lives in locals for the length of the
            // loop: `self.block` is already borrowed, and copying two floats in
            // and out beats splitting the struct.
            let (mut dc_x1, mut dc_y1, dc_coef) = (self.dc_x1, self.dc_y1, self.dc_coef);
            let mut peak = self.peak;

            for (dst, &src) in chunk.iter_mut().zip(block.iter()) {
                // Drive into the soft clipper, then out at master gain. Pushing
                // the clipper is what gives the synth teeth; below 1.0 it stays
                // clean.
                let driven = soft_clip(src * params.drive);

                let blocked = driven - dc_x1 + dc_coef * dc_y1;
                dc_x1 = driven;
                dc_y1 = blocked;

                let value = blocked * gain;
                let value = if value.is_finite() {
                    value.clamp(-1.0, 1.0)
                } else {
                    // A NaN reaching the driver is a loud, ugly failure. It
                    // should be impossible, but silence is the right answer if
                    // it ever happens.
                    0.0
                };
                peak = peak.max(value.abs());
                *dst = value;
            }

            self.dc_x1 = dc_x1;
            self.dc_y1 = dc_y1;
            self.peak = peak;
        }

        self.publish_telemetry();
    }

    /// Fills an interleaved stereo buffer with the same signal on both channels.
    pub fn process_stereo_interleaved(&mut self, out: &mut [f32]) {
        // Render mono into the first half, then expand in place from the back
        // so the read and write cursors never collide.
        let frames = out.len() / 2;
        for chunk_start in (0..frames).step_by(BLOCK) {
            let n = BLOCK.min(frames - chunk_start);
            let mut mono = [0.0f32; BLOCK];
            self.process(&mut mono[..n]);
            let frames = &mut out[chunk_start * 2..(chunk_start + n) * 2];
            for (frame, &sample) in frames.chunks_exact_mut(2).zip(mono.iter()) {
                frame[0] = sample;
                frame[1] = sample;
            }
        }
    }

    /// Adds another event source. Not real-time safe: call before starting the
    /// stream.
    pub fn add_event_source(&mut self, events: Consumer) {
        self.events.push(events);
    }

    /// Pops the next event, moving on to the next source as each empties.
    ///
    /// `cursor` carries the position across calls so the sweep is linear
    /// overall rather than restarting from source zero every time.
    #[inline]
    fn next_event(&mut self, cursor: &mut usize) -> Option<Event> {
        while *cursor < self.events.len() {
            if let Some(event) = self.events[*cursor].pop() {
                return Some(event);
            }
            *cursor += 1;
        }
        None
    }

    /// Applies every queued control event, from every source.
    fn drain_events(&mut self, params: &Params) {
        // Bound the work per call. A pathological producer flooding a queue
        // must not be able to make the callback miss its deadline. The budget
        // is shared across sources, so one flood cannot buy itself extra time
        // at the others' expense either.
        let mut budget = 512;
        let mut cursor = 0;
        while budget > 0 {
            budget -= 1;
            let Some(event) = self.next_event(&mut cursor) else { break };
            match event {
                Event::NoteOn { note, velocity } => {
                    self.remember_held(note);
                    self.note_on(note, velocity, params);
                }
                Event::NoteOff { note } => {
                    self.forget_held(note);
                    self.note_off(note, params);
                }
                Event::AllNotesOff => self.all_notes_off(false),
                Event::Panic => self.all_notes_off(true),
                Event::PitchBend(semitones) => self.params.pitch_bend.set(semitones),
                Event::ModWheel(value) => self.params.mod_wheel.set(value),
                Event::ClockTick => {
                    let seq = self.sequencer.on_midi_tick(params);
                    if let Some(note) = seq.note_off {
                        self.note_off(note, params);
                    }
                    if let Some((note, velocity)) = seq.note_on {
                        self.note_on(note, velocity, params);
                    }
                }
                Event::ClockStart => {
                    self.sequencer.rewind();
                    self.sequencer.clock.start();
                    self.params.seq_playing.set(true);
                }
                Event::ClockStop => {
                    self.sequencer.clock.stop();
                    self.params.seq_playing.set(false);
                    if let Some(note) = self.sequencer.release_all() {
                        self.note_off(note, params);
                    }
                }
                Event::ClockContinue => {
                    self.sequencer.clock.resume();
                    self.params.seq_playing.set(true);
                }
                Event::SetStep { index, step } => {
                    self.sequencer.set_step(index as usize, step);
                    self.publish_pattern();
                }
                Event::Regenerate => self.regenerate(params),
            }
        }

        // The control side can also ask for a new pattern by bumping a counter,
        // which works from anywhere without needing a queue slot.
        let requested = self
            .params
            .gen_regenerate
            .load(core::sync::atomic::Ordering::Relaxed);
        if requested != self.last_regenerate {
            self.last_regenerate = requested;
            self.regenerate(params);
        }

        // The transport can also be driven by the `seq_playing` parameter, for
        // callers that would rather set a flag than send an event.
        //
        // Read it fresh rather than from the block's snapshot: a `ClockStart`
        // event handled just above sets this same flag, and comparing against
        // the pre-event snapshot would see "running, but the flag says stopped"
        // and immediately undo the start.
        let should_play = self.params.seq_playing.get();
        if should_play != self.sequencer.clock.is_running() {
            if should_play {
                self.sequencer.clock.start();
            } else {
                self.sequencer.clock.stop();
                if let Some(note) = self.sequencer.release_all() {
                    self.note_off(note, params);
                }
            }
        }
    }

    fn regenerate(&mut self, params: &Params) {
        self.sequencer
            .set_seed(self.params.gen_seed.load(core::sync::atomic::Ordering::Relaxed));
        self.sequencer
            .regenerate(&GenerativeSettings::from_params(params));
        self.publish_pattern();
    }

    /// Copies the pattern into the shared mirror so the control side can show
    /// it. Called only when the pattern changes, not every block: 64 atomic
    /// stores is cheap but not free, and the pattern is static in between.
    fn publish_pattern(&self) {
        for (index, step) in self.sequencer.pattern().iter().enumerate() {
            self.params.publish_step(index, step);
        }
    }

    // --- Voice allocation ---

    fn note_on(&mut self, note: u8, velocity: f32, params: &Params) {
        match params.voice_mode {
            VoiceMode::Mono => self.mono_note_on(note, velocity, params),
            VoiceMode::Poly => self.poly_note_on(note, velocity, params),
        }
    }

    fn note_off(&mut self, note: u8, params: &Params) {
        match params.voice_mode {
            VoiceMode::Mono => self.mono_note_off(note, params),
            VoiceMode::Poly => {
                for voice in self.voices.iter_mut() {
                    if voice.gate && voice.note == note {
                        voice.note_off();
                    }
                }
            }
        }
    }

    /// Mono: one voice, last-note priority, with fallback.
    ///
    /// Releasing a note while another is still held returns to that held note
    /// rather than going silent. That behaviour is what makes mono synth
    /// basslines playable — trills and hammer-ons come from it directly.
    fn mono_note_on(&mut self, note: u8, velocity: f32, params: &Params) {
        let voice = &mut self.voices[0];
        // Legato: if a note is already sounding, slide to the new pitch without
        // restarting the envelopes. That is what makes overlapping notes join
        // into a phrase instead of re-articulating.
        let legato = params.legato && voice.is_held();

        if legato {
            voice.set_target_note(note);
            voice.velocity = velocity.clamp(0.0, 1.0);
        } else {
            let glide_from = if params.glide > 0.0 && voice.is_active() {
                Some(voice.note as f32)
            } else {
                None
            };
            self.age_counter += 1;
            let age = self.age_counter;
            self.voices[0].note_on(note, velocity, glide_from, true, age);
        }
    }

    fn mono_note_off(&mut self, note: u8, params: &Params) {
        let sounding_note = self.voices[0].note;
        let velocity = self.voices[0].velocity;

        if sounding_note != note && !self.held.is_empty() {
            // A note other than the sounding one was released. Nothing to do.
            return;
        }

        // Fall back to the most recently pressed note still held.
        match self.held.last().copied() {
            Some(previous) => {
                if params.legato {
                    self.voices[0].set_target_note(previous);
                } else {
                    self.age_counter += 1;
                    let age = self.age_counter;
                    self.voices[0].note_on(
                        previous,
                        velocity,
                        Some(sounding_note as f32),
                        true,
                        age,
                    );
                }
            }
            None => self.voices[0].note_off(),
        }
    }

    fn poly_note_on(&mut self, note: u8, velocity: f32, params: &Params) {
        let limit = params.max_voices.clamp(1, MAX_VOICES);
        self.age_counter += 1;
        let age = self.age_counter;

        // Retrigger a voice already holding this note rather than stacking a
        // second one on top. Stacking doubles the volume of repeated notes and
        // burns polyphony for nothing.
        for index in 0..limit {
            if self.voices[index].gate && self.voices[index].note == note {
                self.voices[index].note_on(note, velocity, None, true, age);
                return;
            }
        }

        // A genuinely free voice is always the best choice.
        for index in 0..limit {
            if !self.voices[index].is_active() {
                self.voices[index].note_on(note, velocity, None, true, age);
                return;
            }
        }

        // Out of voices: steal one. Prefer a voice in its release tail — the
        // listener has already stopped attending to it — and among those the
        // quietest, which is the least likely to be missed. Only if every voice
        // is still held do we take the oldest, on the reasoning that the
        // earliest note of a held chord is the one furthest from the player's
        // attention.
        let mut best: Option<usize> = None;
        let mut best_score = f32::MAX;
        for index in 0..limit {
            let voice = &self.voices[index];
            // Held voices score far worse than releasing ones, so a releasing
            // voice always wins if there is one.
            let score = if voice.is_held() {
                1000.0 + voice.age as f32 * 1e-6
            } else {
                voice.level()
            };
            if score < best_score {
                best_score = score;
                best = Some(index);
            }
        }

        if let Some(index) = best {
            self.voices[index].steal(note, velocity, age);
        }
    }

    fn all_notes_off(&mut self, hard: bool) {
        self.held.clear();
        for voice in self.voices.iter_mut() {
            if hard {
                voice.kill();
            } else {
                voice.note_off();
            }
        }
    }

    fn remember_held(&mut self, note: u8) {
        if self.held.contains(&note) {
            return;
        }
        // Never grow past the preallocated capacity: a `push` that reallocates
        // would allocate on the audio thread. 128 is every MIDI note at once,
        // so hitting this means something upstream is misbehaving.
        if self.held.len() < MAX_HELD {
            self.held.push(note);
        }
    }

    fn forget_held(&mut self, note: u8) {
        if let Some(index) = self.held.iter().position(|&n| n == note) {
            self.held.remove(index);
        }
    }

    // --- Output stage ---

    // The DC blocker is a one-pole highpass applied inline in `process`.
    //
    // Pulse waves at extreme widths, asymmetric clipping and resonant highpass
    // tails all leave a DC offset. It is inaudible on its own but it eats
    // headroom and makes the clipper act asymmetrically, so it goes.

    fn publish_telemetry(&mut self) {
        use core::sync::atomic::Ordering::Relaxed;
        let active = self.voices.iter().filter(|v| v.is_active()).count();
        self.params.active_voices.store(active as u32, Relaxed);
        self.params
            .current_step
            .store(self.sequencer.position() as u32, Relaxed);

        // Keep the highest peak the control side has not yet read, so a UI
        // meter polling slower than the audio callback still catches transients.
        let previous = self.params.output_peak.get();
        self.params.output_peak.set(previous.max(self.peak));
        self.peak = 0.0;
    }
}

/// Cubic soft clipper, normalised to unity gain for small signals.
///
/// A hard `clamp` generates infinite harmonics at the clip point and sounds
/// like a fault. This curve bends smoothly into saturation, which is what
/// analogue circuits do and what "drive" is supposed to sound like.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    const LIMIT: f32 = 2.0 / 3.0;
    if x <= -1.0 {
        -LIMIT
    } else if x >= 1.0 {
        LIMIT
    } else {
        x - (x * x * x) / 3.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::channel;
    use crate::params::{ClockSource, SharedParams};

    fn engine() -> (Engine, crate::event::Producer, Arc<SharedParams>) {
        let params = Arc::new(SharedParams::default());
        let (tx, rx) = channel(256);
        let engine = Engine::new(48000.0, params.clone(), rx);
        (engine, tx, params)
    }

    fn render(engine: &mut Engine, samples: usize) -> Vec<f32> {
        let mut out = vec![0.0; samples];
        engine.process(&mut out);
        out
    }

    fn peak(buffer: &[f32]) -> f32 {
        buffer.iter().fold(0.0f32, |a, &b| a.max(b.abs()))
    }

    /// Below this, the output is inaudible: -80 dBFS, well under the noise
    /// floor of 16-bit audio. Exact zero is the wrong test — the output DC
    /// blocker is a one-pole filter, so its tail decays toward zero
    /// asymptotically and never quite arrives.
    const SILENT: f32 = 1e-4;

    fn assert_silent(buffer: &[f32], what: &str) {
        let p = peak(buffer);
        assert!(p < SILENT, "{what}: peak was {p}");
    }

    #[test]
    fn silent_with_no_input() {
        let (mut e, _tx, params) = engine();
        params.seq_playing.set(false);
        let out = render(&mut e, 48000);
        assert_eq!(peak(&out), 0.0);
    }

    #[test]
    fn a_note_makes_sound() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 1.0,
        });
        let out = render(&mut e, 24000);
        assert!(peak(&out) > 0.05, "peak was {}", peak(&out));
    }

    #[test]
    fn output_never_leaves_the_legal_range() {
        let (mut e, tx, params) = engine();
        // Everything cranked: full polyphony, maximum resonance and drive.
        params.resonance.set(1.0);
        params.drive.set(20.0);
        params.master_gain.set(2.0);
        params.seq_playing.set(true);
        for note in 36..60u8 {
            tx.push(Event::NoteOn {
                note,
                velocity: 1.0,
            });
        }
        let out = render(&mut e, 48000 * 2);
        for (i, s) in out.iter().enumerate() {
            assert!(s.is_finite(), "sample {i} was {s}");
            assert!(s.abs() <= 1.0, "sample {i} was {s}, outside [-1, 1]");
        }
    }

    #[test]
    fn polyphony_is_limited_to_max_voices() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.max_voices.set(4);
        for note in 40..60u8 {
            tx.push(Event::NoteOn {
                note,
                velocity: 1.0,
            });
        }
        render(&mut e, 4800);
        let active = params.active_voices.load(core::sync::atomic::Ordering::Relaxed);
        assert!(active <= 4, "{active} voices active with a limit of 4");
        assert!(active > 0, "voice limit silenced everything");
    }

    #[test]
    fn note_off_releases_the_voice() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.amp_release.set(0.02);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 1.0,
        });
        render(&mut e, 4800);
        tx.push(Event::NoteOff { note: 60 });
        render(&mut e, 48000);
        let tail = render(&mut e, 4800);
        assert_silent(&tail, "voice never released");
    }

    #[test]
    fn panic_silences_everything_immediately() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.amp_release.set(10.0);
        for note in 50..60u8 {
            tx.push(Event::NoteOn {
                note,
                velocity: 1.0,
            });
        }
        render(&mut e, 4800);
        tx.push(Event::Panic);
        // A panic cuts the voices dead, which is a step discontinuity, and the
        // output DC blocker rings for a few tens of milliseconds afterward.
        // That ring is the point of the blocker working, not a stuck note, so
        // let it settle before checking for actual silence.
        render(&mut e, 24000);
        let after = render(&mut e, 4800);
        assert_silent(&after, "panic left sound behind");
    }

    /// Mono mode must fall back to a still-held note when the top one is
    /// released — the behaviour that makes basslines playable.
    #[test]
    fn mono_falls_back_to_the_held_note() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.voice_mode.set(VoiceMode::Mono as u32);
        params.glide.set(0.0);
        params.legato.set(true);

        tx.push(Event::NoteOn {
            note: 48,
            velocity: 1.0,
        });
        render(&mut e, 2400);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 1.0,
        });
        render(&mut e, 2400);
        // Releasing the upper note should return to the lower one, not silence.
        tx.push(Event::NoteOff { note: 60 });
        render(&mut e, 2400);

        assert_eq!(e.voices[0].note, 48, "mono did not fall back");
        assert!(e.voices[0].is_active());
    }

    #[test]
    fn mono_uses_exactly_one_voice() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.voice_mode.set(VoiceMode::Mono as u32);
        for note in 40..60u8 {
            tx.push(Event::NoteOn {
                note,
                velocity: 1.0,
            });
        }
        render(&mut e, 4800);
        let active = params.active_voices.load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(active, 1, "mono mode used {active} voices");
    }

    #[test]
    fn poly_reuses_the_same_voice_for_a_repeated_note() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 1.0,
        });
        render(&mut e, 2400);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 1.0,
        });
        render(&mut e, 2400);
        let active = params.active_voices.load(core::sync::atomic::Ordering::Relaxed);
        assert_eq!(active, 1, "repeated note stacked {active} voices");
    }

    #[test]
    fn stealing_prefers_releasing_voices_over_held_ones() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.max_voices.set(2);
        params.amp_release.set(5.0);

        tx.push(Event::NoteOn { note: 60, velocity: 1.0 });
        tx.push(Event::NoteOn { note: 64, velocity: 1.0 });
        render(&mut e, 2400);
        // Release one; it keeps sounding through its long release.
        tx.push(Event::NoteOff { note: 60 });
        render(&mut e, 2400);
        // A third note must take the released voice, not the held one.
        tx.push(Event::NoteOn { note: 67, velocity: 1.0 });
        render(&mut e, 4800);

        let still_held: Vec<u8> = e
            .voices
            .iter()
            .take(2)
            .filter(|v| v.is_held())
            .map(|v| v.note)
            .collect();
        assert!(still_held.contains(&64), "stole a held voice: {still_held:?}");
        assert!(still_held.contains(&67), "new note never sounded");
    }

    #[test]
    fn the_sequencer_plays_when_started() {
        let (mut e, tx, params) = engine();
        params.tempo.set(140.0);
        params.gen_density.set(1.0);
        tx.push(Event::ClockStart);
        let out = render(&mut e, 48000 * 2);
        assert!(peak(&out) > 0.02, "sequencer produced nothing");
    }

    #[test]
    fn stopping_the_transport_stops_the_sound() {
        let (mut e, tx, params) = engine();
        params.tempo.set(140.0);
        params.amp_release.set(0.05);
        tx.push(Event::ClockStart);
        render(&mut e, 48000);
        tx.push(Event::ClockStop);
        render(&mut e, 48000);
        let after = render(&mut e, 24000);
        assert_silent(&after, "notes hung after stop");
    }

    #[test]
    fn external_clock_ticks_drive_the_sequencer() {
        let (mut e, tx, params) = engine();
        params.clock_source.set(ClockSource::ExternalMidi as u32);
        params.gen_density.set(1.0);
        tx.push(Event::ClockStart);

        // 24 ticks per beat at roughly 120 BPM.
        let samples_per_tick = 48000.0 * 60.0 / (120.0 * 24.0);
        let mut out = Vec::new();
        for _ in 0..96 {
            tx.push(Event::ClockTick);
            out.extend(render(&mut e, samples_per_tick as usize));
        }
        assert!(peak(&out) > 0.02, "external clock produced no notes");
    }

    #[test]
    fn a_flood_of_events_does_not_stall_the_callback() {
        let (mut e, tx, _params) = engine();
        // Push far more than the drain budget.
        for _ in 0..10_000 {
            tx.push(Event::ModWheel(0.5));
        }
        // Must return promptly and produce valid audio regardless.
        let out = render(&mut e, BLOCK);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    /// The UI reads the pattern only through the mirror, so a regeneration that
    /// failed to publish would leave the grid showing a stale melody.
    #[test]
    fn regenerating_publishes_the_pattern_to_the_mirror() {
        let (mut e, tx, params) = engine();
        params.gen_density.set(1.0);
        params.seq_length.set(16);

        tx.push(Event::Regenerate);
        render(&mut e, BLOCK);

        let mirrored = params.read_pattern();
        let actual = e.pattern();
        assert_eq!(mirrored.len(), actual.len());
        for (index, (a, b)) in actual.iter().zip(mirrored.iter()).enumerate() {
            assert_eq!(a.active, b.active, "step {index} active flag");
            assert_eq!(a.note, b.note, "step {index} note");
        }
    }

    #[test]
    fn setting_a_step_updates_the_mirror() {
        let (mut e, tx, params) = engine();
        let step = crate::sequencer::Step {
            active: true,
            note: 42,
            velocity: 1.0,
            accent: false,
        };
        tx.push(Event::SetStep { index: 3, step });
        render(&mut e, BLOCK);

        let back = params.read_step(3);
        assert!(back.active);
        assert_eq!(back.note, 42);
    }

    #[test]
    fn soft_clip_is_smooth_and_bounded() {
        let mut previous = soft_clip(-10.0);
        let mut x = -10.0f32;
        while x < 10.0 {
            x += 0.001;
            let y = soft_clip(x);
            assert!(y.abs() <= 2.0 / 3.0 + 1e-6);
            assert!((y - previous).abs() < 0.01, "discontinuity at {x}");
            previous = y;
        }
    }

    #[test]
    fn stereo_matches_mono() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        tx.push(Event::NoteOn { note: 60, velocity: 1.0 });
        let mut stereo = vec![0.0; 2048];
        e.process_stereo_interleaved(&mut stereo);
        for frame in stereo.chunks(2) {
            assert_eq!(frame[0], frame[1], "channels diverged");
        }
        assert!(peak(&stereo) > 0.0);
    }
}
