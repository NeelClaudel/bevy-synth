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

use crate::bass::BassVoice;
use crate::clock::Clock;
use crate::drums::{Column, DrumBuses, DrumPattern, DrumRack};
use crate::event::{Consumer, Event};
use crate::fx::{Compressor, FxChain};
use crate::lfo::Lfo;
use crate::params::{ClockSource, Params, SharedParams, SidechainSource, Smoothed, VoiceMode};
use crate::sequencer::{GenerativeSettings, SeqSettings, Sequencer, MAX_STEPS};
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
    /// The one clock. Both sequencers read the same advance from it, which is
    /// what makes "in sync" a property of the structure rather than of two
    /// accumulators that happen to agree today.
    clock: Clock,

    /// Increments on every note-on; the lowest value is the oldest voice.
    age_counter: u64,

    /// Notes currently held on the keyboard, oldest first. Mono mode falls back
    /// through this when a note is released, which is what lets you trill by
    /// holding one note and tapping another.
    held: Vec<u8>,

    master_gain: Smoothed,
    /// Per-bus levels, smoothed for the same reason `master_gain` is: a knob
    /// dragged across a block boundary steps, and a stepped gain zippers.
    synth_gain: Smoothed,
    drum_gain: Smoothed,
    /// One-pole state for the output DC blocker.
    dc_x1: f32,
    dc_y1: f32,
    dc_coef: f32,

    /// The effects stage: stereo delay into plate reverb.
    fx: FxChain,

    /// Bus inserts. The synth one sits before the send tap so the return
    /// hears the compressed signal; the master one is glue on the sum.
    comp_synth: Compressor,
    comp_master: Compressor,

    /// Eight drum pads and their own grid sequencer, running off the same
    /// clock as the melody.
    drums: DrumRack,

    /// Scratch buffer for one block of voice output.
    block: Vec<f32>,

    /// The bass line. A second sequencer rather than a second channel on the
    /// first: two independent patterns are the point, and they cost one
    /// struct.
    bass_seq: Sequencer,
    bass: BassVoice,
    comp_bass: Compressor,
    bass_gain: Smoothed,
    /// Scratch for the bass's mono render, before it is duplicated to a pair.
    bass_block: Vec<f32>,

    last_voice_mode: VoiceMode,
    last_melody_enabled: bool,
    last_regenerate: u32,
    last_bass_regenerate: u32,
    last_pattern_request: u32,
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

        let mut sequencer = Sequencer::new(
            params.gen_seed.load(core::sync::atomic::Ordering::Relaxed),
        );
        sequencer.regenerate(&GenerativeSettings::from_params(&snapshot));

        let mut bass_seq = Sequencer::new(
            params.bass_gen_seed.load(core::sync::atomic::Ordering::Relaxed),
        );
        bass_seq.regenerate(&GenerativeSettings::for_bass(&snapshot));

        let mut engine = Self {
            sample_rate,
            params,
            events,
            voices,
            lfo: Lfo::new(sample_rate, 0xA5A5),
            sequencer,
            clock: Clock::new(sample_rate),
            age_counter: 1,
            held: Vec::with_capacity(MAX_HELD),
            master_gain: Smoothed::new(snapshot.master_gain, 15.0, control_rate),
            synth_gain: Smoothed::new(snapshot.synth_gain, 15.0, control_rate),
            drum_gain: Smoothed::new(snapshot.drum_gain, 15.0, control_rate),
            dc_x1: 0.0,
            dc_y1: 0.0,
            // ~10 Hz corner: removes DC and subsonic rumble without touching
            // the bottom of the audible range.
            dc_coef: 1.0 - (2.0 * core::f32::consts::PI * 10.0 / sample_rate),
            fx: FxChain::new(sample_rate),
            comp_synth: Compressor::new(sample_rate),
            comp_master: Compressor::new(sample_rate),
            // The seed is arbitrary but fixed: the noise pads must sound the
            // same on every run, or the golden vector would be untestable the
            // moment drums are switched on.
            drums: DrumRack::new(sample_rate, 0xD61B_5EED),
            block: vec![0.0; BLOCK],
            bass_seq,
            bass: BassVoice::new(sample_rate, 0xB455_5EED),
            comp_bass: Compressor::new(sample_rate),
            bass_gain: Smoothed::new(snapshot.bass_gain, 15.0, control_rate),
            bass_block: vec![0.0; BLOCK],
            last_voice_mode: snapshot.voice_mode,
            last_melody_enabled: snapshot.melody_enabled,
            last_regenerate: 0,
            last_bass_regenerate: 0,
            last_pattern_request: 0,
            peak: 0.0,
        };
        engine.clock.set_tempo(snapshot.tempo, snapshot.steps_per_beat);
        engine.publish_pattern();
        engine.publish_bass_pattern();
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
        self.clock.set_sample_rate(sample_rate);
        self.dc_coef = 1.0 - (2.0 * core::f32::consts::PI * 10.0 / sample_rate);
        for gain in [
            &mut self.master_gain,
            &mut self.synth_gain,
            &mut self.drum_gain,
            &mut self.bass_gain,
        ] {
            gain.set_time(15.0, sample_rate / BLOCK as f32);
        }
        self.fx.set_sample_rate(sample_rate);
        self.comp_synth.set_sample_rate(sample_rate);
        self.comp_master.set_sample_rate(sample_rate);
        self.drums.set_sample_rate(sample_rate);
        self.bass.set_sample_rate(sample_rate);
        self.comp_bass.set_sample_rate(sample_rate);
    }

    /// Current sequencer pattern, for display.
    pub fn pattern(&self) -> &[crate::sequencer::Step] {
        self.sequencer.pattern()
    }

    /// Current drum grid, for display.
    pub fn drum_grid(&self) -> &DrumPattern {
        self.drums.sequencer_ref().grid()
    }

    /// Fills `out` with mono audio. This is the audio callback's entry point.
    pub fn process(&mut self, out: &mut [f32]) {
        let params = self.begin_block();
        let mut done = 0;
        while done < out.len() {
            let count = BLOCK.min(out.len() - done);
            let mut left = [0.0f32; BLOCK];
            let mut right = [0.0f32; BLOCK];
            self.render_chunk(&mut left[..count], &mut right[..count], &params);
            for (i, sample) in out[done..done + count].iter_mut().enumerate() {
                // Sum to mono. At the default centred pan the two channels
                // are still bit-identical, and `(x + x) * 0.5` is exact in
                // IEEE-754, so a dry, centred patch passes through untouched.
                // A panned pad makes the channels diverge, and past that
                // point this is an ordinary lossy downmix, not a lossless
                // one.
                *sample = (left[i] + right[i]) * 0.5;
            }
            done += count;
        }
        self.publish_telemetry();
    }

    /// Fills an interleaved stereo buffer. The channels diverge only where the
    /// effects stage puts something in them.
    pub fn process_stereo_interleaved(&mut self, out: &mut [f32]) {
        let params = self.begin_block();
        let frames = out.len() / 2;
        let mut done = 0;
        while done < frames {
            let count = BLOCK.min(frames - done);
            let mut left = [0.0f32; BLOCK];
            let mut right = [0.0f32; BLOCK];
            self.render_chunk(&mut left[..count], &mut right[..count], &params);
            for (i, frame) in out[done * 2..(done + count) * 2]
                .chunks_exact_mut(2)
                .enumerate()
            {
                frame[0] = left[i];
                frame[1] = right[i];
            }
            done += count;
        }
        self.publish_telemetry();
    }

    /// Everything that happens once per `process` call, before any audio:
    /// take the parameter snapshot, handle a mode switch, drain the event
    /// queue. Returns the snapshot the whole call will use.
    fn begin_block(&mut self) -> Params {
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

        // Switching the melody off has to release what it is already holding,
        // or a sequencer note would hang forever with nothing left to send its
        // note-off. Release rather than kill: this is a mute, not a panic.
        if params.melody_enabled != self.last_melody_enabled {
            if !params.melody_enabled {
                self.all_notes_off(false);
            }
            self.last_melody_enabled = params.melody_enabled;
        }

        self.drain_events(&params);

        self.lfo.set_rate(params.lfo_rate);
        self.master_gain.set_target(params.master_gain);
        self.synth_gain.set_target(params.synth_gain);
        self.drum_gain.set_target(params.drum_gain);
        self.bass_gain.set_target(params.bass_gain);

        params
    }

    /// Renders one chunk of at most `BLOCK` samples into two channels.
    ///
    /// Mono up to and including the DC blocker, stereo from the effects on.
    fn render_chunk(&mut self, left: &mut [f32], right: &mut [f32], params: &Params) {
        let count = left.len().min(right.len());

        // Tempo, then one advance, then everything that steps reads the same
        // result. `set_tempo` came here from the sequencer along with the
        // clock; under an external MIDI clock the ticks set the rate instead.
        if params.clock_source == ClockSource::Internal {
            self.clock.set_tempo(params.tempo, params.steps_per_beat);
        }
        let adv = self.clock.advance(count, params.clock_source);
        let view = self.clock.view();

        let seq = self
            .sequencer
            .advance(count, adv, view, &SeqSettings::from_params(params));
        if self.sequencer.take_pattern_changed() {
            // The pattern brought its own length, so the knob has to follow it
            // or the next block's reconcile would cut the melody short.
            self.params
                .seq_length
                .set(self.sequencer.pattern().len() as u32);
            self.publish_pattern();
        }
        if params.melody_enabled {
            if let Some(note) = seq.note_off {
                self.note_off(note, params);
            }
            if let Some((note, velocity)) = seq.note_on {
                self.note_on(note, velocity, params);
            }
        }

        // The same `adv` and `view` the lead just used. One clock advance
        // drives all three sequencers, so nothing can drift from anything
        // else by construction.
        let bass_seq = self
            .bass_seq
            .advance(count, adv, view, &SeqSettings::for_bass(params));
        if self.bass_seq.take_pattern_changed() {
            self.params.bass_seq_length.set(self.bass_seq.pattern().len() as u32);
            self.publish_bass_pattern();
        }
        if params.bass_enabled {
            // Note-off first, then note-on: a tie relies on the voice still
            // being active when the new note arrives, and `BassVoice::note_on`
            // re-gates anyway.
            if bass_seq.note_off.is_some() && !bass_seq.slide {
                self.bass.note_off();
            }
            if let Some((note, _velocity)) = bass_seq.note_on {
                self.bass.note_on(note, bass_seq.accent, bass_seq.slide);
            }
        }

        // Same `adv`, same `view`: one clock advance drives both sequencers,
        // so the drums cannot drift from the melody by construction.
        let drums_playing = self.drums.render(count, adv, view, params);
        if self.drums.sequencer().take_grid_changed() {
            self.publish_drum_grid();
        }

        let lfo = self.lfo.next_block(params.lfo_wave, count);

        // The LFO knob and the mod wheel add, so a patch can be static until
        // the player asks for movement, and the wheel can always reach full
        // depth regardless of where the knob sits.
        let mod_depth = (params.lfo_depth + params.mod_wheel).min(1.0);

        let block = &mut self.block[..count];
        block.fill(0.0);
        for voice in self.voices.iter_mut().take(params.max_voices) {
            voice.process_block(block, params, lfo, mod_depth);
        }

        let gain = self.master_gain.next();
        let synth_gain = self.synth_gain.next();
        let drum_gain = self.drum_gain.next();

        for i in 0..count {
            // Drive into the soft clipper, then out at master gain. Pushing
            // the clipper is what gives the synth teeth; below 1.0 it stays
            // clean.
            let driven = soft_clip(self.block[i] * params.drive);
            let blocked = driven - self.dc_x1 + self.dc_coef * self.dc_y1;
            // Guard here, before the effects, not just at the output: the
            // delay lines and the reverb tank recirculate whatever they're
            // fed, so a non-finite sample stored there is poisoned forever,
            // and the downstream guard can only clean state that was never
            // written in the first place.
            let blocked = if blocked.is_finite() { blocked } else { 0.0 };
            self.dc_x1 = driven;
            self.dc_y1 = blocked;
            // After the clipper, not before it: gain into the clipper changes
            // how hard the synth is saturating, which is `drive`'s job.
            let levelled = blocked * synth_gain;
            left[i] = levelled;
            right[i] = levelled;
        }

        // Insert, before the send tap: the return hears the compressed
        // signal, which is what makes a pumped synth pump in the reverb too.
        let mut detector = [0.0f32; BLOCK];
        let source = params.comp_synth.sidechain;
        let key = sidechain(source, self.drums.buses(), &mut detector, count);
        self.comp_synth
            .process(&mut left[..count], &mut right[..count], key, &params.comp_synth);

        // The sends. The synth is tapped post-compressor; the drums bring
        // their own per-pad send pair, scaled by the bus knob.
        let mut send_l = [0.0f32; BLOCK];
        let mut send_r = [0.0f32; BLOCK];
        for i in 0..count {
            send_l[i] = left[i] * params.synth_send;
            send_r[i] = right[i] * params.synth_send;
        }

        // The bass bus. Not through `drive`, the soft clipper or the DC
        // blocker: those are the synth's, and the same argument the kick makes
        // above applies here — the 303's dirt comes from resonance and
        // envelope depth, not from saturation.
        if params.bass_enabled {
            let bass_gain = self.bass_gain.next();
            let block = &mut self.bass_block[..count];
            self.bass.process_block(block, &params.bass);

            // Mono, duplicated. A bass is centred; there is no pan control and
            // no reason for one.
            let mut bass_l = [0.0f32; BLOCK];
            let mut bass_r = [0.0f32; BLOCK];
            for i in 0..count {
                let value = self.bass_block[i] * bass_gain;
                let value = if value.is_finite() { value } else { 0.0 };
                bass_l[i] = value;
                bass_r[i] = value;
            }

            // Insert, before the send tap, matching `comp_synth`: the return
            // hears the compressed signal, so a bass ducking under the kick
            // ducks in the reverb too.
            let mut detector = [0.0f32; BLOCK];
            let source = params.comp_bass.sidechain;
            let key = sidechain(source, self.drums.buses(), &mut detector, count);
            self.comp_bass.process(
                &mut bass_l[..count],
                &mut bass_r[..count],
                key,
                &params.comp_bass,
            );

            for i in 0..count {
                left[i] += bass_l[i];
                right[i] += bass_r[i];
                send_l[i] += bass_l[i] * params.bass_send;
                send_r[i] += bass_r[i] * params.bass_send;
            }
        } else {
            // Keep the smoother tracking while the bus is muted, so switching
            // the bass on does not start it with a stale ramp.
            self.bass_gain.next();
        }

        // Drums are summed after the drive stage and the DC blocker: a kick
        // through the soft clipper at drive 3.0 is a different instrument,
        // and not a better one.
        if drums_playing {
            let bus = self.drums.buses();
            let send = drum_gain * params.drum_send;
            for i in 0..count {
                left[i] += bus.dry_l[i] * drum_gain;
                right[i] += bus.dry_r[i] * drum_gain;
                send_l[i] += bus.send_l[i] * send;
                send_r[i] += bus.send_r[i] * send;
            }
        }

        // The clock, not `params.tempo`: when an external MIDI clock is
        // driving the sequencer, that is the tempo the delay must lock to.
        let tempo = self.clock.tempo_bpm(params.steps_per_beat);

        // The return. `FxChain` is an insert, so it hands back dry + wet;
        // subtracting the send leaves the wet, and at zero mix that is
        // exactly `0.0` rather than approximately.
        let mut ret_l = send_l;
        let mut ret_r = send_r;
        self.fx
            .process_block(&mut ret_l[..count], &mut ret_r[..count], params, tempo);
        for i in 0..count {
            left[i] += ret_l[i] - send_l[i];
            right[i] += ret_r[i] - send_r[i];
        }

        // Glue on the sum.
        let source = params.comp_master.sidechain;
        let key = sidechain(source, self.drums.buses(), &mut detector, count);
        self.comp_master
            .process(&mut left[..count], &mut right[..count], key, &params.comp_master);

        for i in 0..count {
            let l = left[i] * gain;
            let r = right[i] * gain;
            // A NaN reaching the driver is a loud, ugly failure. It should be
            // impossible, but silence is the right answer if it ever happens.
            //
            // This clamp is hard, not soft: `soft_clip` runs upstream, before
            // the DC blocker and the effects, so it saturates only the dry
            // voices and never sees the wet signal. The effects can gain the
            // signal by roughly 7x at extreme settings (full delay feedback
            // and reverb size), so this is the only thing standing between
            // that and the device, and at those settings it clips hard rather
            // than soft: with `delay_mix` 1.0, `delay_feedback` 0.9,
            // `reverb_mix` 1.0 and `reverb_size` 1.0, about two thirds of
            // output samples sit pinned at exactly ±1.0. Known behaviour, not
            // a bug — the loudest shipping preset asks for 0.55, and moving
            // the saturation downstream of the effects would change the dry
            // path too.
            let l = if l.is_finite() { l.clamp(-1.0, 1.0) } else { 0.0 };
            let r = if r.is_finite() { r.clamp(-1.0, 1.0) } else { 0.0 };
            self.peak = self.peak.max(l.abs()).max(r.abs());
            left[i] = l;
            right[i] = r;
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
                    // The engine owns the clock, so it also owns the decision
                    // about whether this tick advances anything.
                    let ticked = params.clock_source == ClockSource::ExternalMidi
                        && self.clock.is_running()
                        && self.clock.on_midi_tick(params.steps_per_beat);
                    let seq = self.sequencer.on_midi_tick(
                        ticked,
                        self.clock.view(),
                        &SeqSettings::from_params(params),
                    );
                    // Gated exactly as `render_chunk` gates the same pair. The
                    // sequencer still steps while muted — a mute stops the
                    // events, not the playhead — but nothing it produces
                    // reaches a voice.
                    if params.melody_enabled {
                        if let Some(note) = seq.note_off {
                            self.note_off(note, params);
                        }
                        if let Some((note, velocity)) = seq.note_on {
                            self.note_on(note, velocity, params);
                        }
                    }
                    self.drums.on_tick(ticked, self.clock.view(), params);
                }
                Event::ClockStart => {
                    self.sequencer.rewind();
                    self.drums.sequencer().rewind();
                    self.bass_seq.rewind();
                    self.clock.start();
                    self.params.seq_playing.set(true);
                }
                Event::ClockStop => {
                    self.clock.stop();
                    self.params.seq_playing.set(false);
                    if let Some(note) = self.sequencer.release_all() {
                        self.note_off(note, params);
                    }
                    // Drums are one-shots with no note-off, so stopping the
                    // transport has to cut them: there is nothing else that
                    // ever would.
                    self.drums.silence();
                    // The bass amp envelope sustains at 1.0 with no release,
                    // so a note gated on when the transport stops would ring
                    // forever otherwise.
                    let _ = self.bass_seq.release_all();
                    self.bass.silence();
                }
                Event::ClockContinue => {
                    self.clock.resume();
                    self.params.seq_playing.set(true);
                }
                Event::SetStep { index, step } => {
                    self.sequencer.set_step(index as usize, step);
                    self.publish_pattern();
                }
                Event::Regenerate => self.regenerate(params),
                Event::SetBassStep { index, step } => {
                    self.bass_seq.set_step(index as usize, step);
                    self.publish_bass_pattern();
                }
                Event::RegenerateBass => self.regenerate_bass(params),
                Event::SetDrumCell { step, pad, cell } => {
                    // No publish here: `set_cell` raises the changed flag and
                    // `render_chunk` publishes once per block, so a burst of
                    // edits in one block costs one publish, not one each.
                    self.drums
                        .sequencer()
                        .set_cell(step as usize, pad as usize, cell);
                }
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

        // Same idea for the bass line: the control side can ask for a new one
        // by bumping its own counter, independent of the lead's.
        let requested = self
            .params
            .bass_regenerate
            .load(core::sync::atomic::Ordering::Relaxed);
        if requested != self.last_bass_regenerate {
            self.last_bass_regenerate = requested;
            self.regenerate_bass(params);
        }

        // A saved pattern is loaded the same way, by counter. It is too big to
        // travel through the event queue — every note-on would pay for the
        // largest variant — so the control side stages it in the parameter
        // block and bumps a counter once it is all there.
        let requested = self
            .params
            .pending_request
            .load(core::sync::atomic::Ordering::Relaxed);
        if requested != self.last_pattern_request {
            self.last_pattern_request = requested;
            self.sequencer
                .queue_pattern(self.params.read_pending_pattern());
        }

        // The transport can also be driven by the `seq_playing` parameter, for
        // callers that would rather set a flag than send an event.
        //
        // Read it fresh rather than from the block's snapshot: a `ClockStart`
        // event handled just above sets this same flag, and comparing against
        // the pre-event snapshot would see "running, but the flag says stopped"
        // and immediately undo the start.
        let should_play = self.params.seq_playing.get();
        if should_play != self.clock.is_running() {
            if should_play {
                self.clock.start();
            } else {
                self.clock.stop();
                if let Some(note) = self.sequencer.release_all() {
                    self.note_off(note, params);
                }
                self.drums.silence();
                // Same unconditional cut as `ClockStop`: the bass envelope
                // never releases on its own.
                let _ = self.bass_seq.release_all();
                self.bass.silence();
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

    fn regenerate_bass(&mut self, params: &Params) {
        self.bass_seq.set_seed(
            self.params.bass_gen_seed.load(core::sync::atomic::Ordering::Relaxed),
        );
        self.bass_seq
            .regenerate(&GenerativeSettings::for_bass(params));
        self.publish_bass_pattern();
    }

    /// Copies the pattern into the shared mirror so the control side can show
    /// it. Called only when the pattern changes, not every block: 64 atomic
    /// stores is cheap but not free, and the pattern is static in between.
    fn publish_pattern(&self) {
        for (index, step) in self.sequencer.pattern().iter().enumerate() {
            self.params.publish_step(index, step);
        }
        self.params.publish_len(self.sequencer.pattern().len());
    }

    /// Copies the bass pattern into its mirror. Same contract as
    /// [`Engine::publish_pattern`]: only when it changes, never every block.
    fn publish_bass_pattern(&self) {
        for (index, step) in self.bass_seq.pattern().iter().enumerate() {
            self.params.publish_bass_step(index, step);
        }
        self.params.publish_bass_len(self.bass_seq.pattern().len());
    }

    /// Copies the drum grid into the shared mirror. Same contract as
    /// [`Engine::publish_pattern`]: only when it changes, never every block.
    fn publish_drum_grid(&self) {
        let grid = self.drums.sequencer_ref().grid();
        // Every column, not just the active length. Shortening the pattern and
        // lengthening it again must not show the UI stale cells, and 64 atomic
        // stores on an edit is the same budget the melodic mirror already
        // spends.
        for step in 0..MAX_STEPS {
            let column: Column = core::array::from_fn(|pad| grid.get(step, pad));
            self.params.publish_drum_column(step, &column);
        }
        self.params.publish_drum_len(grid.len());
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
        if hard {
            self.drums.silence();
            // Same reasoning as the drums: `Panic` and a voice-mode switch
            // both have to be able to stop everything, and the bass envelope
            // sustains at 1.0 forever with nothing else to cut it.
            let _ = self.bass_seq.release_all();
            self.bass.silence();
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
        self.params
            .drum_position
            .store(self.drums.sequencer_ref().position() as u32, Relaxed);
        self.params
            .bass_step
            .store(self.bass_seq.position() as u32, Relaxed);

        // Keep the highest peak the control side has not yet read, so a UI
        // meter polling slower than the audio callback still catches transients.
        let previous = self.params.output_peak.get();
        self.params.output_peak.set(previous.max(self.peak));
        self.peak = 0.0;

        // Straight set, no `max`: the meter should follow the compressor down
        // as it releases.
        self.params
            .comp_synth_gr
            .set(self.comp_synth.gain_reduction_db());
        self.params
            .comp_master_gr
            .set(self.comp_master.gain_reduction_db());
        self.params.comp_bass_gr.set(self.comp_bass.gain_reduction_db());
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

/// Fills `scratch` with the detector signal and hands back a view of it, or
/// `None` when the compressor should detect on its own input.
///
/// The tap is ahead of the drum fader, so pulling the rack to silence leaves
/// a trigger track that ducks without being heard — the usual way to key a
/// compressor off a kick. A rack that is switched off or has stopped ringing
/// zeroes these buses itself, so an idle rack cannot pump anything.
fn sidechain<'a>(
    source: SidechainSource,
    bus: &DrumBuses,
    scratch: &'a mut [f32; BLOCK],
    count: usize,
) -> Option<&'a [f32]> {
    match source {
        SidechainSource::Off => None,
        SidechainSource::DrumBus => {
            for (i, sample) in scratch.iter_mut().enumerate().take(count) {
                *sample = (bus.dry_l[i] + bus.dry_r[i]) * 0.5;
            }
            Some(&scratch[..count])
        }
        SidechainSource::Kick => {
            scratch[..count].copy_from_slice(&bus.kick[..count]);
            Some(&scratch[..count])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drums::Cell;
    use crate::event::channel;
    use crate::params::{ClockSource, CompressorParams, SharedParams};

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

    /// `frames` frames of interleaved stereo, so `2 * frames` floats. The
    /// mono `render` collapses the buses, which is fine for level and
    /// bit-identity checks but hides everything about the stereo topology.
    fn render_stereo(engine: &mut Engine, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        engine.process_stereo_interleaved(&mut out);
        out
    }

    fn peak(buffer: &[f32]) -> f32 {
        buffer.iter().fold(0.0f32, |a, &b| a.max(b.abs()))
    }

    /// Equality on the bits, not on the values. `assert_eq!` over `f32` says
    /// `-0.0 == 0.0`, which is the wrong answer whenever a test's claim is
    /// that a path was left untouched — the same standard the golden vector
    /// holds the dry path to.
    fn bit_identical(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.to_bits() == y.to_bits())
    }

    /// The bypass, stated as a test. Two engines rendering the same melody,
    /// one with the rack switched off and one with it switched on over an
    /// empty grid: the second sequences a whole run of columns and strikes
    /// nothing, so `render` returns false and the engine never mixes the bus.
    /// Bit-identical output is the claim, and it is the claim the golden
    /// vector rests on.
    ///
    /// Comparing two drums-off engines instead — which is what this test used
    /// to do under a name promising more — only shows that the engine is
    /// deterministic.
    #[test]
    fn an_enabled_but_empty_rack_leaves_the_output_untouched() {
        let (mut off, tx_off, params_off) = engine();
        let (mut on, tx_on, params_on) = engine();
        for params in [&params_off, &params_on] {
            params.tempo.set(140.0);
            params.gen_density.set(1.0);
        }
        params_off.drum_enabled.set(false);
        params_on.drum_enabled.set(true);
        tx_off.push(Event::ClockStart);
        tx_on.push(Event::ClockStart);

        let quiet = render(&mut off, 48_000);
        assert!(peak(&quiet) > 0.02, "the melody never played");
        assert_eq!(render(&mut on, 48_000), quiet);
    }

    /// And the other half: switched on with a hit programmed, it must actually
    /// reach the output.
    #[test]
    fn an_enabled_drum_reaches_the_output() {
        let (mut engine, tx, params) = engine();
        params.drum_enabled.set(true);
        params.seq_playing.set(true);
        // Silence the melody, so the only thing in the buffer is the drum.
        params.melody_enabled.set(false);
        assert!(tx.push(Event::SetDrumCell {
            step: 0,
            pad: 0,
            cell: Cell {
                active: true,
                velocity: 1.0
            },
        }));

        assert!(
            peak(&render(&mut engine, 8_192)) > 0.01,
            "the drum bus never arrived"
        );
    }

    /// The mirror is what the UI draws from, so an edit has to show up in it.
    #[test]
    fn a_drum_edit_reaches_the_mirror() {
        let (mut engine, tx, params) = engine();
        assert!(tx.push(Event::SetDrumCell {
            step: 5,
            pad: 3,
            cell: Cell {
                active: true,
                velocity: 0.5
            },
        }));
        render(&mut engine, BLOCK);

        let grid = params.read_drum_grid();
        assert!(grid.get(5, 3).active);
        assert_eq!(grid.get(5, 3).velocity, 0.5);
        assert!(!grid.get(5, 2).active);
    }

    /// A pattern longer than the default 16 has to survive being generated,
    /// published, read back for saving, and loaded again. Any step of that
    /// chain that assumes 16 truncates the melody.
    #[test]
    fn a_long_pattern_survives_the_round_trip() {
        let (mut engine, _tx, params) = engine();

        params.seq_length.set(32);
        params.regenerate();
        render(&mut engine, 64);

        assert_eq!(engine.sequencer.pattern().len(), 32, "generated length");
        let saved = params.read_pattern();
        assert_eq!(saved.len(), 32, "what the UI would put in a slot");

        // Now play it back, the way clicking a slot does.
        params.seq_length.set(16);
        params.regenerate();
        render(&mut engine, 64);
        params.queue_pattern(&saved);
        render(&mut engine, 64);

        assert_eq!(engine.sequencer.pattern().len(), 32, "loaded length");
        let mirror = params.read_pattern();
        assert_eq!(mirror.len(), 32, "mirror after loading");
        for (i, step) in mirror.iter().enumerate() {
            assert_eq!(step.note, saved[i].note, "step {i}");
            assert_eq!(step.active, saved[i].active, "step {i}");
        }
    }

    /// Growing the pattern with the Length knob, then saving, without ever
    /// pressing Generate. The grid shows 32 steps, so a save has to hand back
    /// the same 32 the grid is showing.
    #[test]
    fn growing_the_length_without_regenerating_still_saves_what_the_grid_shows() {
        let (mut engine, _tx, params) = engine();

        params.regenerate();
        render(&mut engine, 64);
        let short = params.read_pattern();
        assert_eq!(short.len(), 16, "starting length");

        // Drag Length to 32. No Generate.
        params.seq_length.set(32);
        render(&mut engine, 64);

        assert_eq!(engine.sequencer.pattern().len(), 32, "sequencer grew");
        let saved = params.read_pattern();
        assert_eq!(saved.len(), 32, "save length");
        for (i, step) in saved.iter().enumerate() {
            assert_eq!(
                step.note,
                engine.sequencer.pattern()[i].note,
                "step {i} note disagrees with the sequencer"
            );
            assert_eq!(
                step.active,
                engine.sequencer.pattern()[i].active,
                "step {i} active disagrees with the sequencer"
            );
        }
    }

    /// The whole load path, end to end: the control side stages a pattern, the
    /// audio thread notices the counter, the sequencer adopts it, and the
    /// mirror the UI reads is brought back into agreement with it. A break
    /// anywhere along that chain leaves the grid showing one melody while the
    /// speakers play another.
    #[test]
    fn a_loaded_pattern_reaches_the_sequencer_and_the_mirror() {
        use crate::sequencer::{Pattern, Step, MAX_STEPS};
        let (mut engine, _tx, params) = engine();

        let mut steps = [Step::default(); MAX_STEPS];
        for (i, step) in steps.iter_mut().enumerate().take(8) {
            *step = Step {
                active: true,
                // Below anything the generator reaches, so this can only be
                // the loaded pattern.
                note: 12 + i as u8,
                velocity: 1.0,
                accent: false,
                slide: false,
            };
        }
        params.queue_pattern(&Pattern::new(steps, 8));

        // Stopped, so the pattern lands on the first block rather than waiting
        // for a bar line that will never come.
        render(&mut engine, 64);

        assert_eq!(engine.sequencer.pattern().len(), 8, "the length did not follow");
        assert_eq!(params.seq_length.get(), 8, "the knob was left behind");
        let mirror = params.read_pattern();
        assert_eq!(mirror.len(), 8);
        for (i, step) in mirror.iter().enumerate() {
            assert!(step.active);
            assert_eq!(step.note, 12 + i as u8, "step {i} in the mirror");
        }
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

    /// `melody_enabled` is a mute, and until now nothing checked that it
    /// mutes. It was only ever used in tests to get the melody out of the way
    /// while measuring drums, which is why a gap in the external-clock path
    /// went unnoticed for as long as it did.
    #[test]
    fn unticking_the_melody_silences_the_sequencer() {
        let (mut e, tx, params) = engine();
        params.tempo.set(140.0);
        params.gen_density.set(1.0);
        params.melody_enabled.set(false);
        tx.push(Event::ClockStart);

        assert_silent(&render(&mut e, 48000 * 2), "the melody played while muted");
    }

    /// The same mute, under an external MIDI clock. Every step arrives through
    /// the `ClockTick` arm there rather than through `render_chunk`, so the two
    /// paths have to gate on `melody_enabled` alike — otherwise unticking
    /// "Play melody" does nothing at all for anyone slaved to a DAW.
    #[test]
    fn unticking_the_melody_silences_the_sequencer_under_an_external_clock() {
        let (mut e, tx, params) = engine();
        params.clock_source.set(ClockSource::ExternalMidi as u32);
        params.gen_density.set(1.0);
        params.melody_enabled.set(false);
        tx.push(Event::ClockStart);

        // 24 ticks per beat at roughly 120 BPM — the same drive as
        // `external_clock_ticks_drive_the_sequencer`, which proves these ticks
        // do reach the sequencer when the melody is not muted.
        let samples_per_tick = 48000.0 * 60.0 / (120.0 * 24.0);
        let mut out = Vec::new();
        for _ in 0..96 {
            tx.push(Event::ClockTick);
            out.extend(render(&mut e, samples_per_tick as usize));
        }
        assert_silent(&out, "the melody played while muted under an external clock");
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
            slide: false,
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

    #[test]
    fn the_reverb_makes_the_output_stereo() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.reverb_mix.set(0.8);
        params.reverb_size.set(0.8);
        params.reverb_width.set(1.0);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 0.8,
        });

        let mut out = vec![0.0; 8192];
        e.process_stereo_interleaved(&mut out);

        let differing = out
            .chunks_exact(2)
            .filter(|frame| (frame[0] - frame[1]).abs() > 1e-6)
            .count();
        assert!(differing > 1000, "only {differing} frames differed");
    }

    #[test]
    fn the_dry_output_is_still_mono() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 0.8,
        });

        let mut out = vec![0.0; 4096];
        e.process_stereo_interleaved(&mut out);

        for (i, frame) in out.chunks_exact(2).enumerate() {
            assert_eq!(frame[0], frame[1], "frame {i} was not mono");
        }
    }

    #[test]
    fn the_delay_survives_the_engines_output_stage() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.delay_mix.set(0.8);
        params.delay_time.set(0.2);
        params.delay_feedback.set(0.6);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 1.0,
        });

        let mut out = render(&mut e, 4800);
        tx.push(Event::NoteOff { note: 60 });
        out.extend(render(&mut e, 43200));

        // The note stopped a tenth of a second in; anything still audible at
        // half a second is the delay.
        let late: f32 = out[24000..].iter().map(|s| s.abs()).sum();
        assert!(late > 1.0, "no repeats after the note ended: {late}");
    }

    #[test]
    fn a_wet_patch_still_respects_the_output_limits() {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        params.drive.set(4.0);
        params.delay_mix.set(1.0);
        params.delay_feedback.set(0.95);
        params.delay_time.set(0.01);
        params.reverb_mix.set(1.0);
        params.reverb_size.set(1.0);
        params.reverb_damping.set(0.0);

        for note in 40..56 {
            tx.push(Event::NoteOn {
                note,
                velocity: 1.0,
            });
        }

        let out = render(&mut e, 48000 * 4);
        for (i, sample) in out.iter().enumerate() {
            assert!(sample.is_finite(), "sample {i} was {sample}");
            assert!(sample.abs() <= 1.0, "sample {i} was {sample}");
        }
    }

    /// FNV-1a over the raw bits of every sample.
    ///
    /// A checksum rather than a stored buffer because 4096 floats do not
    /// belong in a source file, and because the bits are what matter: any
    /// change to any sample, however small, changes the hash.
    fn bit_hash(samples: &[f32]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for sample in samples {
            for byte in sample.to_bits().to_le_bytes() {
                h ^= byte as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h
    }

    /// Renders the reference signal: one held middle C, sequencer off,
    /// everything else at its default. Deterministic and dry.
    fn golden_render() -> Vec<f32> {
        let (mut e, tx, params) = engine();
        params.seq_playing.set(false);
        tx.push(Event::NoteOn {
            note: 60,
            velocity: 0.8,
        });
        render(&mut e, 4096)
    }

    /// The hash of the reference signal, and a readable window into it.
    ///
    /// The effects stage sits between the DC blocker and the master gain, and
    /// it must be inaudible when both mixes are zero. "Inaudible" is not good
    /// enough here — the dry path has to come out bit for bit identical, or
    /// every existing patch has quietly changed. The window makes a failure
    /// diagnosable: the hash tells you something moved, the sixteen samples
    /// tell you by how much.
    const GOLDEN_HASH: u64 = 0x1c357a5698234b39;
    const GOLDEN_WINDOW: [u32; 16] = [0xbe1e6c64, 0xbe1a2c83, 0xbe15e73a, 0xbe119c18, 0xbe0d4af2, 0xbe08f415, 0xbe049826, 0xbe0037d3, 0xbdf7a754, 0xbdeed81e, 0xbde60286, 0xbddd26f3, 0xbdd445b6, 0xbdcb5f25, 0xbdc2739a, 0xbdb9836e];

    #[test]
    fn the_dry_path_matches_the_golden_vector() {
        let out = golden_render();
        let window: Vec<u32> = out[2048..2064].iter().map(|s| s.to_bits()).collect();
        let hash = bit_hash(&out);

        if hash != GOLDEN_HASH || window[..] != GOLDEN_WINDOW[..] {
            let listed: Vec<String> = window.iter().map(|b| format!("0x{b:08x}")).collect();
            panic!(
                "dry output changed.\n\
                 const GOLDEN_HASH: u64 = 0x{hash:016x};\n\
                 const GOLDEN_WINDOW: [u32; 16] = [{}];\n\
                 (If this is the first run, paste the two lines above into the test.)",
                listed.join(", ")
            );
        }
    }

    /// Builds an engine with the params already set, so the gain smoothers
    /// start *at* the value under test rather than gliding toward it. Setting
    /// a gain after construction would leave the first ~15 ms ramping down
    /// from the default, and "silent" would not be true of the whole buffer.
    fn engine_preset(setup: impl FnOnce(&SharedParams)) -> (Engine, crate::event::Producer) {
        let params = Arc::new(SharedParams::default());
        setup(&params);
        let (tx, rx) = channel(256);
        (Engine::new(48000.0, params.clone(), rx), tx)
    }

    /// A drum cell programmed on the downbeat of pad 0, for the tests that
    /// need the rack to make a noise.
    fn kick_on_one(tx: &crate::event::Producer) {
        assert!(tx.push(Event::SetDrumCell {
            step: 0,
            pad: 0,
            cell: Cell {
                active: true,
                velocity: 1.0
            },
        }));
    }

    #[test]
    fn a_zeroed_synth_gain_silences_the_melody() {
        let (mut engine, tx) = engine_preset(|p| {
            p.synth_gain.set(0.0);
            p.tempo.set(140.0);
            p.gen_density.set(1.0);
        });
        tx.push(Event::ClockStart);

        assert_eq!(
            peak(&render(&mut engine, 48_000)),
            0.0,
            "the melody played through a closed synth gain"
        );
    }

    /// The other half of the claim: the knob is a melody-bus control, so the
    /// rack has to be audible with it shut. Without this, a `synth_gain`
    /// wired into the master would pass the test above and still be wrong.
    #[test]
    fn a_zeroed_synth_gain_leaves_the_drums_alone() {
        let (mut engine, tx) = engine_preset(|p| {
            p.synth_gain.set(0.0);
            p.drum_enabled.set(true);
            p.seq_playing.set(true);
            p.melody_enabled.set(false);
        });
        kick_on_one(&tx);

        assert!(
            peak(&render(&mut engine, 8_192)) > 0.01,
            "the synth gain swallowed the drum bus"
        );
    }

    /// Both sum sites, not just one: the rack reaches the master through its
    /// dry pair and the return bus through its send pair, and a gain applied
    /// to only one of them would leave the rack audible on the other.
    #[test]
    fn a_zeroed_drum_gain_silences_the_drums() {
        for send in [0.0, 1.0] {
            let (mut engine, tx) = engine_preset(|p| {
                p.drum_gain.set(0.0);
                p.drum_enabled.set(true);
                p.seq_playing.set(true);
                p.melody_enabled.set(false);
                p.drum_send.set(send);
                p.delay_mix.set(1.0);
            });
            kick_on_one(&tx);

            assert_eq!(
                peak(&render(&mut engine, 8_192)),
                0.0,
                "the rack played through a closed drum gain (drum_send = {send})"
            );
        }
    }

    #[test]
    fn a_zeroed_drum_gain_leaves_the_melody_alone() {
        let (mut engine, tx) = engine_preset(|p| {
            p.drum_gain.set(0.0);
            p.tempo.set(140.0);
            p.gen_density.set(1.0);
        });
        tx.push(Event::ClockStart);

        assert!(
            peak(&render(&mut engine, 48_000)) > 0.02,
            "the drum gain swallowed the melody"
        );
    }

    /// The seam between the rack and the engine, not the rack itself: Task 3's
    /// pan tests (`a_hard_left_pad_empties_the_right_channel_and_leaves_the_left_alone`
    /// in `drums/rack.rs`) pin `DrumBuses` in isolation, but nothing checked
    /// that the rack's stereo pair actually reaches
    /// `process_stereo_interleaved`'s output as a stereo pair rather than,
    /// say, both channels reading from `dry_l`.
    #[test]
    fn a_hard_left_pad_stays_stereo_at_the_engine_output() {
        let (mut engine, tx) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.pad_pan[0].set(-1.0);
        });
        kick_on_one(&tx);
        tx.push(Event::ClockStart);

        let out = render_stereo(&mut engine, 8192);
        let l: Vec<f32> = out.iter().step_by(2).copied().collect();
        let r: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(peak(&r), 0.0, "the hard-left pad leaked into the right channel");
        assert!(peak(&l) > 0.0, "the hard-left pad silenced the left channel too");
    }

    /// The claim the golden vector rests on: with both mixes at zero the
    /// return hands back exactly what it was sent, so `chain(send) - send`
    /// is `0.0` and the send knob is inaudible. Bit-identical, not close.
    #[test]
    fn the_return_adds_nothing_at_zero_mix() {
        let (mut full, tx_full) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.synth_send.set(1.0);
        });
        let (mut none, tx_none) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.synth_send.set(0.0);
        });
        for tx in [&tx_full, &tx_none] {
            tx.push(Event::NoteOn {
                note: 60,
                velocity: 0.8,
            });
        }

        assert_eq!(render(&mut full, 4096), render(&mut none, 4096));
    }

    /// The send/return has to reproduce the insert it replaced. The reference
    /// is an `FxChain` of its own, fed the engine's dry output in the same
    /// 32-frame chunks the engine uses: that *is* the old code path.
    #[test]
    fn a_full_send_reproduces_the_insert_it_replaced() {
        // The dry signal. At zero mix the return contributes nothing, so
        // this is the engine's dry path with the master fader wide open.
        let (mut dry_engine, tx_dry) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
            p.delay_time.set(0.01);
        });
        tx_dry.push(Event::NoteOn {
            note: 60,
            velocity: 0.8,
        });
        let dry = render_stereo(&mut dry_engine, 4096);

        // The same signal through a fresh chain, as an insert.
        let mut l: Vec<f32> = dry.iter().step_by(2).copied().collect();
        let mut r: Vec<f32> = dry.iter().skip(1).step_by(2).copied().collect();
        let mut chain = crate::fx::FxChain::new(48_000.0);
        let mut reference_params = Params::default();
        reference_params.delay_mix = 0.5;
        // 0.375 s is 18 000 samples at 48 kHz: over a 4096-frame window the
        // delay line never fills and the chain degenerates into a memoryless
        // multiply by `1 - mix`, which a scrambled or channel-swapped return
        // would satisfy just as well. At 0.01 s it rings inside the window.
        reference_params.delay_time = 0.01;
        // A centred note through the delay alone comes out with `l == r`, and a
        // channel-swapped return is then unobservable. The plate's two channels
        // are built from different tap lengths, so it decorrelates them and the
        // swap has somewhere to show up.
        reference_params.reverb_mix = 0.5;
        for (cl, cr) in l.chunks_mut(BLOCK).zip(r.chunks_mut(BLOCK)) {
            chain.process_block(cl, cr, &reference_params, reference_params.tempo);
        }

        // And the same signal through the engine, sent to the return at 1.0.
        let (mut wet_engine, tx_wet) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
            p.synth_send.set(1.0);
            p.delay_mix.set(0.5);
            p.delay_time.set(0.01);
            p.reverb_mix.set(0.5);
        });
        tx_wet.push(Event::NoteOn {
            note: 60,
            velocity: 0.8,
        });
        let wet = render_stereo(&mut wet_engine, 4096);

        // Sanity: the delay actually did something, or this proves nothing.
        assert!(peak(&wet) > 0.0);
        let moved = wet
            .iter()
            .zip(dry.iter())
            .fold(0.0f32, |a, (w, d)| a.max((w - d * 0.5).abs()));
        assert!(moved > 1.0e-3, "the delay was inaudible; the test is vacuous: {moved:e}");
        let spread = l
            .iter()
            .zip(r.iter())
            .fold(0.0f32, |a, (x, y)| a.max((x - y).abs()));
        assert!(spread > 1.0e-3, "the reference is mono; a channel swap would pass: {spread:e}");

        for (i, sample) in wet.iter().enumerate() {
            let want = if i % 2 == 0 { l[i / 2] } else { r[i / 2] };
            assert!(
                (sample - want).abs() < 1.0e-6,
                "sample {i}: return {sample} vs insert {want}"
            );
        }
    }

    /// `drum_send` replaces the old bool's `false` position: at 0.0 the rack
    /// puts nothing into the return, so raising a mix changes nothing.
    #[test]
    fn a_closed_drum_send_keeps_the_rack_out_of_the_return() {
        let (mut wet, tx_wet) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(0.0);
            p.delay_mix.set(1.0);
        });
        let (mut dry, tx_dry) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(0.0);
            p.delay_mix.set(0.0);
        });
        for tx in [&tx_wet, &tx_dry] {
            kick_on_one(tx);
            tx.push(Event::ClockStart);
        }

        let wet_out = render(&mut wet, 8192);
        assert!(peak(&wet_out) > 0.0, "the rack never sounded");
        assert_eq!(wet_out, render(&mut dry, 8192));
    }

    /// The other position of the old bool: opened up, the rack reaches the
    /// effects. The algebra that makes this equal the old insert is the same
    /// code `a_full_send_reproduces_the_insert_it_replaced` pins; what is
    /// specific here is that the rack's send pair is wired to it at all.
    #[test]
    fn an_open_drum_send_reaches_the_effects() {
        let (mut open, tx_open) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(1.0);
            p.delay_mix.set(1.0);
        });
        let (mut shut, tx_shut) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(0.0);
            p.delay_mix.set(1.0);
        });
        for tx in [&tx_open, &tx_shut] {
            kick_on_one(tx);
            tx.push(Event::ClockStart);
        }

        assert_ne!(render(&mut open, 8192), render(&mut shut, 8192));
    }

    /// Per-pad sends are why the rack renders a send pair instead of the
    /// engine scaling the dry one. Pad 0 muted out of the send has to leave
    /// the return empty even with the bus send wide open.
    #[test]
    fn a_pad_send_of_zero_survives_to_the_return() {
        let (mut sent, tx_sent) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(1.0);
            p.delay_mix.set(1.0);
            p.pad_send[0].set(1.0);
        });
        let (mut held_back, tx_held) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(1.0);
            p.delay_mix.set(1.0);
            p.pad_send[0].set(0.0);
        });
        // The same engine again with the return itself shut. An empty return
        // adds exactly `+0.0`, so if pad 0 really is held out of the send then
        // `held_back` has to come out bit-for-bit equal to this.
        let (mut closed, tx_closed) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(1.0);
            p.delay_mix.set(0.0);
            p.pad_send[0].set(0.0);
        });
        for tx in [&tx_sent, &tx_held, &tx_closed] {
            kick_on_one(tx);
            tx.push(Event::ClockStart);
        }

        let sent_out = render(&mut sent, 8192);
        let held_out = render(&mut held_back, 8192);
        let closed_out = render(&mut closed, 8192);
        // Anti-vacuity: the return is audible when pad 0 is allowed into it.
        assert_ne!(sent_out, held_out);
        assert!(
            bit_identical(&held_out, &closed_out),
            "muting pad 0 out of the send left something in the return"
        );
    }

    /// A compressor whose detector never crosses the threshold computes
    /// `10^((0 - 0)/20)` — exactly 1.0 — and multiplying by 1.0 is bit-exact.
    /// So an armed sidechain over a silent rack has to be *identical*, not
    /// merely close: anything else means the detector is picking up the synth.
    #[test]
    fn an_armed_sidechain_over_a_silent_rack_changes_nothing() {
        let (mut armed, tx_armed) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.drum_enabled.set(true);
            p.comp_synth.on.set(true);
            p.comp_synth.sidechain.set(SidechainSource::Kick as u32);
        });
        let (mut bypassed, tx_bypassed) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.drum_enabled.set(true);
        });
        for tx in [&tx_armed, &tx_bypassed] {
            tx.push(Event::NoteOn {
                note: 60,
                velocity: 0.8,
            });
            tx.push(Event::ClockStart);
        }

        // `assert_eq!` would not say this: `-0.0 == 0.0` is true, so a sign
        // flip would slip through a claim of identity.
        assert!(
            bit_identical(&render(&mut armed, 4096), &render(&mut bypassed, 4096)),
            "the armed sidechain moved the output"
        );
    }

    /// The point of the whole feature: pad 0 ducks the synth. Both engines
    /// hold the same note with the same compressor armed; only one has a
    /// kick programmed, and the ducked one has to come out quieter.
    #[test]
    fn the_kick_ducks_the_synth_through_the_sidechain() {
        let arm = |p: &SharedParams| {
            p.seq_playing.set(false);
            p.drum_enabled.set(true);
            p.drum_gain.set(0.0); // Hear the ducking, not the kick.
            p.comp_synth.on.set(true);
            p.comp_synth.sidechain.set(SidechainSource::Kick as u32);
            p.comp_synth.threshold_db.set(-30.0);
            p.comp_synth.ratio.set(10.0);
            p.comp_synth.attack_ms.set(1.0);
            p.comp_synth.release_ms.set(200.0);
        };
        let (mut ducked, tx_ducked) = engine_preset(arm);
        let (mut steady, tx_steady) = engine_preset(arm);
        for tx in [&tx_ducked, &tx_steady] {
            tx.push(Event::NoteOn {
                note: 60,
                velocity: 0.8,
            });
        }
        kick_on_one(&tx_ducked);
        tx_ducked.push(Event::ClockStart);
        tx_steady.push(Event::ClockStart);

        let ducked_out = render(&mut ducked, 8192);
        let steady_out = render(&mut steady, 8192);
        assert!(peak(&steady_out) > 0.0, "the synth never sounded");
        assert!(
            peak(&ducked_out) < peak(&steady_out) * 0.9,
            "the kick did not duck the synth: {} vs {}",
            peak(&ducked_out),
            peak(&steady_out)
        );
    }

    /// Nothing pinned the synth compressor ahead of the send tap: every test
    /// above that exercises the return leaves `comp_synth.on` at its default
    /// `false`, where a bypassed compressor is a pass-through and the
    /// ordering cannot matter. Build the expected chain by hand -- compress
    /// first, then feed the compressed signal to a fresh `FxChain`, exactly
    /// like `a_full_send_reproduces_the_insert_it_replaced` builds its
    /// reference -- and check the engine's actual output matches it. Swap
    /// the compressor and the send tap and the engine would feed the chain
    /// the raw signal instead, so it would no longer match this reference.
    #[test]
    fn the_synth_compressor_sits_ahead_of_the_send_tap() {
        // The pre-compression dry signal: master wide open, nothing else
        // running.
        let (mut dry_engine, tx_dry) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
        });
        tx_dry.push(Event::NoteOn {
            note: 60,
            velocity: 0.8,
        });
        let dry = render_stereo(&mut dry_engine, 4096);
        let mut comp_l: Vec<f32> = dry.iter().step_by(2).copied().collect();
        let mut comp_r: Vec<f32> = dry.iter().skip(1).step_by(2).copied().collect();

        // Compress it by hand, with a punishing enough threshold that the
        // ordering is unmistakable, chunked by `BLOCK` the same way the
        // engine calls it.
        let comp_params = CompressorParams {
            on: true,
            threshold_db: -36.0,
            ratio: 8.0,
            attack_ms: 0.1,
            release_ms: 50.0,
            makeup_db: 0.0,
            sidechain: SidechainSource::Off,
        };
        let mut reference_comp = Compressor::new(48_000.0);
        for (cl, cr) in comp_l.chunks_mut(BLOCK).zip(comp_r.chunks_mut(BLOCK)) {
            reference_comp.process(cl, cr, None, &comp_params);
        }

        // Sanity: the compressor actually did something, or this proves
        // nothing.
        let moved = comp_l
            .iter()
            .zip(dry.iter().step_by(2))
            .fold(0.0f32, |a, (c, d)| a.max((c - d).abs()));
        assert!(
            moved > 1.0e-3,
            "the compressor was inaudible; the test is vacuous: {moved:e}"
        );

        // Feed the compressed signal to a fresh chain. At `synth_send = 1.0`
        // this is exactly what the send tap hands the return if the
        // compressor really does sit ahead of it; the dry term the engine
        // carries forward is that same compressed signal, so `dry + (ret -
        // send)` collapses to `ret`, the same identity
        // `a_full_send_reproduces_the_insert_it_replaced` uses.
        let mut ret_l = comp_l.clone();
        let mut ret_r = comp_r.clone();
        let mut chain = FxChain::new(48_000.0);
        let reference_params = Params {
            delay_mix: 0.5,
            delay_time: 0.01,
            reverb_mix: 0.5,
            ..Default::default()
        };
        for (cl, cr) in ret_l.chunks_mut(BLOCK).zip(ret_r.chunks_mut(BLOCK)) {
            chain.process_block(cl, cr, &reference_params, reference_params.tempo);
        }

        let (mut wet_engine, tx_wet) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
            p.synth_send.set(1.0);
            p.delay_mix.set(0.5);
            p.delay_time.set(0.01);
            p.reverb_mix.set(0.5);
            p.comp_synth.on.set(true);
            p.comp_synth.threshold_db.set(-36.0);
            p.comp_synth.ratio.set(8.0);
            p.comp_synth.attack_ms.set(0.1);
            p.comp_synth.release_ms.set(50.0);
        });
        tx_wet.push(Event::NoteOn {
            note: 60,
            velocity: 0.8,
        });
        let wet = render_stereo(&mut wet_engine, 4096);

        for (i, sample) in wet.iter().enumerate() {
            let want = if i % 2 == 0 { ret_l[i / 2] } else { ret_r[i / 2] };
            assert!(
                (sample - want).abs() < 1.0e-6,
                "sample {i}: return {sample} vs compressed-then-chained reference {want}"
            );
        }
    }

    /// The meter has to agree with the gain actually applied. Drive a
    /// compressed engine hard, then check the reported reduction against the
    /// difference the compressor made to the peak.
    #[test]
    fn the_meter_reports_the_reduction_the_compressor_applied() {
        let squash = |p: &SharedParams| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
            p.comp_master.on.set(true);
            p.comp_master.threshold_db.set(-24.0);
            p.comp_master.ratio.set(8.0);
            p.comp_master.attack_ms.set(0.1);
        };
        let (mut squashed, tx_squashed) = engine_preset(squash);
        let (mut open, tx_open) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
        });
        for tx in [&tx_squashed, &tx_open] {
            tx.push(Event::NoteOn { note: 60, velocity: 1.0 });
        }

        // Long enough for the attack to settle at the steady-state reduction.
        let squashed_out = render(&mut squashed, 8_192);
        let open_out = render(&mut open, 8_192);
        assert!(peak(&open_out) > 0.0, "the synth never sounded");

        let reported = squashed.params.comp_master_gr.get();
        assert!(reported > 0.0, "the meter reported no reduction");

        let measured = 20.0 * (peak(&open_out) / peak(&squashed_out)).log10();
        assert!(
            (reported - measured).abs() < 3.0,
            "meter says {reported} dB, the output moved {measured} dB"
        );
    }

    /// The near-copy of the above for the synth bus: `comp_synth_gr` is
    /// written in `publish_telemetry` but nothing reads it, so a broken
    /// publish (or one that always writes `0.0`) would survive the suite
    /// forever and ship as a permanently dead meter in the UI.
    #[test]
    fn the_synth_meter_reports_the_reduction_the_compressor_applied() {
        let squash = |p: &SharedParams| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
            p.comp_synth.on.set(true);
            p.comp_synth.threshold_db.set(-24.0);
            p.comp_synth.ratio.set(8.0);
            p.comp_synth.attack_ms.set(0.1);
        };
        let (mut squashed, tx_squashed) = engine_preset(squash);
        let (mut open, tx_open) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
        });
        for tx in [&tx_squashed, &tx_open] {
            tx.push(Event::NoteOn { note: 60, velocity: 1.0 });
        }

        // Long enough for the attack to settle at the steady-state reduction.
        let squashed_out = render(&mut squashed, 8_192);
        let open_out = render(&mut open, 8_192);
        assert!(peak(&open_out) > 0.0, "the synth never sounded");

        let reported = squashed.params.comp_synth_gr.get();
        assert!(reported > 0.0, "the meter reported no reduction");

        let measured = 20.0 * (peak(&open_out) / peak(&squashed_out)).log10();
        assert!(
            (reported - measured).abs() < 3.0,
            "meter says {reported} dB, the output moved {measured} dB"
        );
    }

    /// A bypassed compressor reads zero, so the meter empties rather than
    /// freezing at whatever it last saw.
    #[test]
    fn the_meter_empties_when_the_compressor_is_off() {
        let (mut engine, tx) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.comp_master.on.set(true);
            p.comp_master.threshold_db.set(-40.0);
        });
        tx.push(Event::NoteOn { note: 60, velocity: 1.0 });
        render(&mut engine, 4_096);
        assert!(engine.params.comp_master_gr.get() > 0.0, "never compressed");

        engine.params.comp_master.on.set(false);
        render(&mut engine, 4_096);
        assert_eq!(engine.params.comp_master_gr.get(), 0.0);
    }

    /// Renders `blocks` blocks through the *stereo* path and reports the peak
    /// absolute sample.
    ///
    /// `render_stereo` and `peak` are already in this `mod tests`; the stereo
    /// entry point is the one that matters here, because the bass bus is a
    /// pair and the mono `process` collapses it before anything can be seen.
    fn engine_peak(engine: &mut Engine, blocks: usize) -> f32 {
        let out = render_stereo(engine, blocks * BLOCK);
        assert!(out.iter().all(|s| s.is_finite()), "the bus went non-finite");
        peak(&out)
    }

    #[test]
    fn the_bass_is_silent_until_it_is_enabled() {
        let quiet = Params {
            bass_enabled: false,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&quiet));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        assert!(engine_peak(&mut engine, 400) < 1e-6, "the bass must stay off");

        shared.bass_enabled.set(true);
        assert!(engine_peak(&mut engine, 400) > 0.001, "and sound once switched on");
    }

    #[test]
    fn bass_and_lead_step_from_the_same_clock() {
        let p = Params { bass_enabled: true, seq_playing: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        // Same length on both lines: locked to one clock, they must agree on
        // which step they are on, every block, forever.
        shared.bass_seq_length.set(shared.seq_length.get());
        for _ in 0..2_000 {
            render_stereo(&mut engine, BLOCK);
            assert_eq!(
                shared.bass_step.load(core::sync::atomic::Ordering::Relaxed),
                shared.current_step.load(core::sync::atomic::Ordering::Relaxed),
                "two lines, one clock"
            );
        }
    }

    #[test]
    fn bass_send_feeds_the_return_and_zero_does_not() {
        // Reverb fully wet, everything else muted: whatever comes out is the
        // send.
        //
        // Two separate engines, not one engine measured twice in sequence: the
        // bass sequencer keeps advancing between measurements, so reusing one
        // engine would compare two different slices of the generative pattern
        // (different notes, different accents) rather than the same notes
        // with and without a tail. Every other dry/wet comparison in this
        // file (e.g. `an_open_drum_send_reaches_the_effects`) already builds
        // a fresh engine per side for exactly this reason.
        //
        // `bass_gain` is turned down from its 1.0 default: the 303 voice's
        // resonant filter (resonance 0.7, env_mod 3.0) overshoots unity by
        // up to ~2x on accented notes, so at default gain the dry signal
        // alone already pins the engine's hard output clamp. Once both
        // sides are clamped to the same ceiling, adding a send tail can't
        // raise the measured peak and the comparison stops meaning
        // anything. 0.4 keeps the dry peak comfortably under the clamp so
        // the wet tail actually shows up in the measurement.
        let base = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            reverb_mix: 1.0,
            bass_gain: 0.4,
            ..Params::default()
        };
        let closed = Params { bass_send: 0.0, ..base };
        let shared_closed = std::sync::Arc::new(SharedParams::from_params(&closed));
        let (_tx_dry, rx_dry) = crate::event::channel(64);
        let mut dry_engine = Engine::new(48_000.0, shared_closed, rx_dry);
        let dry = engine_peak(&mut dry_engine, 800);

        let open = Params { bass_send: 1.0, ..base };
        let shared_open = std::sync::Arc::new(SharedParams::from_params(&open));
        let (_tx_wet, rx_wet) = crate::event::channel(64);
        let mut wet_engine = Engine::new(48_000.0, shared_open, rx_wet);
        let wet = engine_peak(&mut wet_engine, 800);

        assert!(wet > dry, "an open send should add a tail: {dry} -> {wet}");
    }

    #[test]
    fn comp_bass_reports_the_reduction_it_applies() {
        let p = Params {
            bass_enabled: true,
            seq_playing: true,
            comp_bass: CompressorParams {
                on: true,
                threshold_db: -50.0,
                ratio: 20.0,
                ..CompressorParams::default()
            },
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        engine_peak(&mut engine, 600);
        assert!(
            shared.comp_bass_gr.get() > 0.5,
            "a -50 dB threshold at 20:1 should show real gain reduction"
        );
    }

    #[test]
    fn the_bass_bus_skips_the_drive_stage() {
        // Drive belongs to the synth. Turning it up must not change the bass,
        // exactly as it does not change the drums.
        let p = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            drive: 1.0,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        let clean = engine_peak(&mut engine, 600);

        shared.drive.set(8.0);
        let driven = engine_peak(&mut engine, 600);
        assert!(
            (clean - driven).abs() < clean * 0.05,
            "drive must not touch the bass: {clean} vs {driven}"
        );
    }

    #[test]
    fn clock_stop_silences_a_gated_bass_note() {
        // The bass amp envelope sustains at 1.0 with no release, so a note
        // gated on when the transport stops has nothing else to cut it. The
        // sequencer's own gate-length timeout counts down in real samples
        // rather than clock ticks, so it would eventually release the note
        // on its own even without the fix — the window checked here is well
        // under one step's length (a gate of 2.0 steps at this tempo is far
        // longer than 200 samples), so that timeout cannot be the one doing
        // the silencing.
        let p = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_gate: 2.0,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        shared.tempo.set(140.0);
        shared.bass_gen_density.set(1.0);

        tx.push(Event::ClockStart);
        let gated = render(&mut engine, 48_000);
        assert!(peak(&gated) > SILENT, "bass never gated a note");

        tx.push(Event::ClockStop);
        // Short enough that the sequencer's own gate-length timeout cannot
        // have expired before the assertion.
        let after = render(&mut engine, 200);
        assert_silent(&after, "bass note hung after the transport stopped");
    }

    #[test]
    fn seq_playing_false_silences_a_gated_bass_note() {
        // Same cut as `ClockStop`, but reached through the `seq_playing`
        // reconcile path instead of an event. Same short window as above,
        // for the same reason: it must be the fix cutting the note, not the
        // sequencer's own gate-length timeout expiring on its own.
        let p = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_gate: 2.0,
            seq_playing: true,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        shared.tempo.set(140.0);
        shared.bass_gen_density.set(1.0);

        let gated = render(&mut engine, 48_000);
        assert!(peak(&gated) > SILENT, "bass never gated a note");

        shared.seq_playing.set(false);
        let after = render(&mut engine, 200);
        assert_silent(&after, "bass note hung after seq_playing went false");
    }

    #[test]
    fn panic_silences_a_gated_bass_note() {
        let p = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        shared.tempo.set(140.0);
        shared.bass_gen_density.set(1.0);

        let gated = render(&mut engine, 48_000);
        assert!(peak(&gated) > SILENT, "bass never gated a note");

        tx.push(Event::Panic);
        // Short enough that the sequencer cannot have reached its next tick
        // and re-gated a new note before the assertion.
        let after = render(&mut engine, 200);
        assert_silent(&after, "panic left the bass gated");
    }

    #[test]
    fn clock_start_rewinds_the_bass_sequencer_with_the_lead() {
        use core::sync::atomic::Ordering::Relaxed;
        let p = Params { bass_enabled: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        shared.bass_seq_length.set(shared.seq_length.get());
        shared.tempo.set(300.0);

        tx.push(Event::ClockStart);
        render_stereo(&mut engine, BLOCK * 200);
        assert_ne!(
            shared.bass_step.load(Relaxed),
            0,
            "test setup: the bass sequencer never left step 0"
        );

        tx.push(Event::ClockStop);
        tx.push(Event::ClockStart);
        // One block: just enough for `begin_block` to drain the two events
        // and publish the resulting position, nowhere near the next tick.
        render_stereo(&mut engine, BLOCK);

        assert_eq!(
            shared.bass_step.load(Relaxed),
            shared.current_step.load(Relaxed),
            "bass sequencer wasn't rewound with the lead"
        );
        assert_eq!(
            shared.bass_step.load(Relaxed),
            0,
            "bass sequencer should be back at step 0"
        );
    }

    #[test]
    fn editing_a_bass_step_leaves_the_lead_alone() {
        let p = Params { bass_enabled: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        render_stereo(&mut engine, BLOCK);

        let lead_before = shared.read_step(2);
        assert!(tx.push(Event::SetBassStep {
            index: 2,
            step: crate::sequencer::Step {
                active: true, note: 31, velocity: 0.7, accent: true, slide: true,
            },
        }));
        render_stereo(&mut engine, BLOCK);

        assert_eq!(shared.read_bass_step(2).note, 31);
        assert!(shared.read_bass_step(2).slide);
        assert_eq!(shared.read_step(2), lead_before, "the lead must be untouched");
    }

    #[test]
    fn regenerating_one_line_leaves_the_other_alone() {
        let p = Params { bass_enabled: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        render_stereo(&mut engine, BLOCK);

        let lead: Vec<u8> = shared.read_pattern().iter().map(|s| s.note).collect();
        let bass: Vec<u8> = shared.read_bass_pattern().iter().map(|s| s.note).collect();

        shared.bass_gen_seed.store(0xFACE_0001, core::sync::atomic::Ordering::Relaxed);
        assert!(tx.push(Event::RegenerateBass));
        render_stereo(&mut engine, BLOCK);

        let lead_after: Vec<u8> = shared.read_pattern().iter().map(|s| s.note).collect();
        let bass_after: Vec<u8> = shared.read_bass_pattern().iter().map(|s| s.note).collect();
        assert_eq!(lead, lead_after, "regenerating the bass must not touch the lead");
        assert_ne!(bass, bass_after, "and must actually write a new bass line");
    }

    /// `melody_enabled` defaults to `true`, so isolate the bass the same way
    /// the sibling tests above do (`clock_stop_silences_a_gated_bass_note` and
    /// friends) — otherwise a still-ringing lead voice, on its own 250 ms
    /// release, would fail this on the lead's account rather than the
    /// bass's.
    #[test]
    fn stopping_the_transport_silences_the_bass() {
        let p = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        assert!(engine_peak(&mut engine, 600) > 0.001, "playing");

        assert!(tx.push(Event::ClockStop));
        render_stereo(&mut engine, BLOCK);
        // Past the bass amp envelope's release.
        assert!(engine_peak(&mut engine, 200) < 1e-6, "stopped means silent");
    }

    /// Same isolation as above, and for the same reason.
    #[test]
    fn panic_silences_the_bass() {
        let p = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        engine_peak(&mut engine, 600);
        shared.seq_playing.set(false);
        assert!(tx.push(Event::Panic));
        render_stereo(&mut engine, BLOCK);
        assert!(engine_peak(&mut engine, 100) < 1e-6, "panic cuts everything");
    }
}
