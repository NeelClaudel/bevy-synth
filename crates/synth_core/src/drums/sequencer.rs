//! The drum sequencer: one column of the grid per step, no note-off.
//!
//! This is the melodic [`crate::sequencer::Sequencer`] with everything a drum
//! does not need taken out. A drum is a one-shot: there is no gate to hold and
//! no note to release, so there is no `samples_until_off` and no `sounding`.
//! What is added is width — a step fires a whole [`Column`], up to eight pads
//! at once, so the swing buffer holds a column rather than a single note.
//!
//! It does not own a clock. It reads the one the engine owns, exactly as the
//! melodic sequencer does after Task 1, which is what keeps the two tracks on
//! the same grid.

use super::{Cell, Column, DrumPattern, PAD_COUNT};
use crate::clock::{Advance, ClockView};
use crate::params::Params;

/// What one block of drum sequencing produced.
///
/// `hits[i]` is the velocity a pad was struck at this block, `0.0` meaning it
/// was not struck. Like [`crate::sequencer::SeqOutput`], this reports at most
/// one step per block: a block is under a millisecond, and no tempo this synth
/// accepts steps twice inside one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DrumOutput {
    pub hits: [f32; PAD_COUNT],
    pub stepped: bool,
}

/// A column held back by swing, waiting out its delay.
#[derive(Clone, Copy)]
struct PendingColumn {
    column: Column,
    samples_remaining: usize,
}

pub struct DrumSequencer {
    pattern: DrumPattern,
    position: usize,
    pending: Option<PendingColumn>,
    grid_changed: bool,
    last_length_param: usize,
}

impl DrumSequencer {
    pub fn new() -> Self {
        Self {
            pattern: DrumPattern::default(),
            // `usize::MAX` means "has not started"; the first step lands on 0
            // rather than 1. Same trick the melodic sequencer uses.
            position: usize::MAX,
            pending: None,
            grid_changed: true,
            last_length_param: 0,
        }
    }

    pub fn position(&self) -> usize {
        if self.position == usize::MAX {
            0
        } else {
            self.position
        }
    }

    pub fn grid(&self) -> &DrumPattern {
        &self.pattern
    }

    /// Follow the length knob.
    ///
    /// Separate from `advance` because the rack calls it on every block,
    /// including blocks where the rack is switched off: the UI reads the grid
    /// mirror whether the drums are enabled or not, and the mirror carries the
    /// length.
    pub fn reconcile_length(&mut self, p: &Params) {
        // The UI owns the length. Only react when the parameter actually
        // moves, so a pattern that set its own length is not overwritten
        // every block.
        let requested = p.drum_length;
        if requested == self.last_length_param {
            return;
        }
        self.last_length_param = requested;
        self.pattern.set_len(requested);
        // The mirror carries the length as well as the cells, so a length
        // move has to reach the UI the same way an edit does.
        self.grid_changed = true;
        if self.position != usize::MAX && self.position >= self.pattern.len() {
            self.position = usize::MAX;
        }
    }

    pub fn set_cell(&mut self, step: usize, pad: usize, cell: Cell) {
        self.pattern.set(step, pad, cell);
        self.grid_changed = true;
    }

    pub fn toggle(&mut self, step: usize, pad: usize) {
        self.pattern.toggle(step, pad);
        self.grid_changed = true;
    }

    /// True once after any edit, so the engine knows to republish the mirror.
    pub fn take_grid_changed(&mut self) -> bool {
        core::mem::replace(&mut self.grid_changed, false)
    }

    pub fn rewind(&mut self) {
        self.position = usize::MAX;
        self.pending = None;
    }

    /// Advance by `samples` worth of block, stepping `adv.steps` times.
    pub fn advance(
        &mut self,
        samples: usize,
        adv: Advance,
        clock: ClockView,
        p: &Params,
    ) -> DrumOutput {
        let mut out = DrumOutput::default();
        self.reconcile_length(p);

        // A swung column comes due partway through a block. Fire it at the
        // top of the block it lands in — sub-block placement would need a
        // sample offset in DrumOutput, and 32 samples is under a millisecond.
        if let Some(mut pending) = self.pending.take() {
            if pending.samples_remaining <= samples {
                fire(&pending.column, &mut out);
            } else {
                pending.samples_remaining -= samples;
                self.pending = Some(pending);
            }
        }

        if !clock.running {
            return out;
        }
        for _ in 0..adv.steps {
            self.step(clock, p, &mut out);
        }
        out
    }

    /// The external-clock path: one tick is one step.
    pub fn on_tick(&mut self, ticked: bool, clock: ClockView, p: &Params) -> DrumOutput {
        let mut out = DrumOutput::default();
        // Same first move as `advance`, and for the same two reasons. The
        // length knob has to reach the grid on this path too — under an
        // external clock no block ever calls `advance` — and until it does the
        // pattern is `DrumPattern::default()`, whose length is zero: `step`
        // would index an empty slice, on the audio thread.
        self.reconcile_length(p);
        if ticked {
            self.step(clock, p, &mut out);
        }
        out
    }

    fn step(&mut self, clock: ClockView, p: &Params, out: &mut DrumOutput) {
        let len = self.pattern.len().max(1);
        let next = if self.position == usize::MAX {
            0
        } else {
            (self.position + 1) % len
        };
        self.position = next;
        out.stepped = true;

        // A column already waiting on swing is overtaken by this one. Fire it
        // rather than drop it — at extreme swing and a fast tempo the delay
        // can outlast a step, and a silently eaten hit is worse than an early
        // one.
        if let Some(pending) = self.pending.take() {
            fire(&pending.column, out);
        }

        let column = self.pattern[next];
        if !column.iter().any(|c| c.active) {
            return;
        }

        // Swing pushes the off-beats late. Same rule and same parameter as the
        // melodic sequencer, so the two tracks swing together.
        let delay = if next % 2 == 1 {
            (p.seq_swing * clock.samples_per_step) as usize
        } else {
            0
        };
        if delay == 0 {
            fire(&column, out);
        } else {
            self.pending = Some(PendingColumn {
                column,
                samples_remaining: delay,
            });
        }
    }
}

impl Default for DrumSequencer {
    fn default() -> Self {
        Self::new()
    }
}

/// Write a column's active cells into the block's hit array.
fn fire(column: &Column, out: &mut DrumOutput) {
    for (slot, cell) in out.hits.iter_mut().zip(column.iter()) {
        if cell.active {
            *slot = cell.velocity;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::params::{ClockSource, Params};
    use crate::sequencer::SeqSettings;
    use crate::BLOCK;

    fn running_clock(p: &Params) -> Clock {
        let mut c = Clock::new(48_000.0);
        c.set_tempo(p.tempo, p.steps_per_beat);
        c.start();
        c
    }

    fn block(s: &mut DrumSequencer, c: &mut Clock, p: &Params) -> DrumOutput {
        let adv = c.advance(BLOCK, ClockSource::Internal);
        s.advance(BLOCK, adv, c.view(), p)
    }

    /// The grid has to fire the pads it says it will, on the step it says.
    #[test]
    fn a_hit_on_step_zero_fires_immediately() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let mut c = running_clock(&p);
        let mut s = DrumSequencer::new();
        s.set_cell(0, 1, Cell { active: true, velocity: 1.0 });

        // At the default tempo (120 bpm, 4 steps/beat, 48 kHz) one step is
        // 6000 samples — 187.5 blocks of `BLOCK` (32) — so the first block
        // does not itself cross the step-0 boundary. Run until the block that
        // does, then check it landed on step 0 with the right hits. This is a
        // fix to the test's reach, not to what it checks: `stepped` still has
        // to be exactly the block that fires, and the hit levels are the same
        // ones the brief specified.
        let mut out = block(&mut s, &mut c, &p);
        let mut blocks = 0;
        while !out.stepped {
            out = block(&mut s, &mut c, &p);
            blocks += 1;
            assert!(blocks < 1_000, "no block ever stepped");
        }
        assert_eq!(s.position(), 0, "the first step to fire should be step 0");
        assert_eq!(out.hits[1], 1.0);
        assert_eq!(out.hits[0], 0.0);
    }

    /// A column is eight pads, not one. This is the difference from the
    /// melodic sequencer that actually changes the data structure.
    #[test]
    fn a_column_fires_every_pad_at_once() {
        let p = Params::default();
        let mut c = running_clock(&p);
        let mut s = DrumSequencer::new();
        for pad in 0..PAD_COUNT {
            s.set_cell(0, pad, Cell { active: true, velocity: 0.5 });
        }

        // Same reach fix as `a_hit_on_step_zero_fires_immediately` above: the
        // first block cannot itself cross the step boundary at the default
        // tempo, so run to the block that does.
        let mut out = block(&mut s, &mut c, &p);
        let mut blocks = 0;
        while !out.stepped {
            out = block(&mut s, &mut c, &p);
            blocks += 1;
            assert!(blocks < 1_000, "no block ever stepped");
        }
        assert!(out.hits.iter().all(|v| *v == 0.5), "{:?}", out.hits);
    }

    /// The grid loops at its own length, not the melody's.
    #[test]
    fn the_grid_wraps_at_its_own_length() {
        let mut p = Params { drum_length: 4, ..Default::default() };
        p.seq_length = 16;
        let mut c = running_clock(&p);
        let mut s = DrumSequencer::new();

        let mut seen = Vec::new();
        // Long enough to wrap a four-step grid more than once.
        for _ in 0..4_000 {
            if block(&mut s, &mut c, &p).stepped {
                seen.push(s.position());
            }
        }
        assert!(seen.len() > 6, "the clock never stepped");
        assert!(seen.iter().all(|p| *p < 4), "walked past the end: {seen:?}");
        assert!(seen.contains(&0) && seen.contains(&3));
    }

    /// Swing has to reach the drums, or the two tracks drift apart on every
    /// off-beat even though they share a clock.
    #[test]
    fn swing_delays_the_odd_column() {
        let p = Params { seq_swing: 0.5, ..Default::default() };
        let mut c = running_clock(&p);
        let mut s = DrumSequencer::new();
        s.set_cell(1, 0, Cell { active: true, velocity: 1.0 });

        let mut stepped_at = None;
        let mut fired_at = None;
        for i in 0..2_000 {
            let out = block(&mut s, &mut c, &p);
            if out.stepped && s.position() == 1 && stepped_at.is_none() {
                stepped_at = Some(i);
            }
            if out.hits[0] > 0.0 && fired_at.is_none() {
                fired_at = Some(i);
            }
        }
        let (stepped, fired) = (stepped_at.unwrap(), fired_at.unwrap());
        assert!(fired > stepped, "swing did not delay the hit: {fired} vs {stepped}");
    }

    /// The polyrhythm claim, made concrete. One clock drives a 16-step drum
    /// grid and a 12-step melody. Each track wraps on its own length, so they
    /// only start together again every lcm(16, 12) = 48 steps — a two-against-
    /// three feel that repeats, not two loops drifting apart. This is the test
    /// that would fail if either sequencer still owned a clock of its own.
    #[test]
    fn a_sixteen_and_a_twelve_realign_after_forty_eight_steps() {
        let mut p = Params { drum_length: 16, ..Default::default() };
        p.seq_length = 12;
        let settings = SeqSettings::from_params(&p);
        let mut c = running_clock(&p);
        let mut drums = DrumSequencer::new();
        let mut melody = crate::sequencer::Sequencer::new(1);

        // Both tracks read the same advance from the same clock, exactly as
        // the engine drives them.
        let mut steps = 0usize;
        let mut together = Vec::new();
        while steps < 96 {
            let adv = c.advance(BLOCK, ClockSource::Internal);
            let out = drums.advance(BLOCK, adv, c.view(), &p);
            let _ = melody.advance(BLOCK, adv, c.view(), &settings);
            if out.stepped {
                if drums.position() == 0 && melody.position() == 0 {
                    together.push(steps);
                }
                steps += 1;
            }
        }

        assert_eq!(
            together,
            vec![0, 48],
            "the two grids did not realign on 48: {together:?}"
        );
    }

    /// The reason the clock moved into the engine, stated as a test. Two
    /// sequencers, one clock: every step boundary has to be the same boundary
    /// for both — while the tempo is yanked out from under them, and again
    /// when the steps are arriving as MIDI ticks rather than as samples.
    #[test]
    fn both_tracks_cross_every_step_boundary_together() {
        let mut p = Params::default();
        let settings = SeqSettings::from_params(&p);
        let mut c = running_clock(&p);
        let mut drums = DrumSequencer::new();
        let mut melody = crate::sequencer::Sequencer::new(1);

        // Internal clock, with a tempo change halfway through.
        for i in 0..4_000 {
            if i == 2_000 {
                p.tempo = 174.0;
                c.set_tempo(p.tempo, p.steps_per_beat);
            }
            let adv = c.advance(BLOCK, ClockSource::Internal);
            let d = drums.advance(BLOCK, adv, c.view(), &p);
            let m = melody.advance(BLOCK, adv, c.view(), &settings);
            assert_eq!(d.stepped, m.stepped, "tracks disagreed at block {i}");
        }

        // External MIDI. The step boundaries now come from the tick counter,
        // and both tracks are driven from the one bool the clock returns.
        p.clock_source = ClockSource::ExternalMidi;
        let mut steps = 0;
        for i in 0..240 {
            let ticked = c.on_midi_tick(p.steps_per_beat);
            let d = drums.on_tick(ticked, c.view(), &p);
            let m = melody.on_midi_tick(ticked, c.view(), &settings);
            assert_eq!(d.stepped, m.stepped, "tracks disagreed at tick {i}");
            steps += usize::from(d.stepped);
        }
        assert!(steps > 0, "no MIDI tick ever produced a step");
    }

    /// Swing is written twice — once here, once in the melodic sequencer — and
    /// the two are deliberately not shared. Nothing stopped them drifting
    /// apart, so this pins the agreement: one clock, one swing setting, a hit
    /// on every step of both tracks, and the same blocks have to fire.
    #[test]
    fn both_tracks_swing_the_same_steps_by_the_same_amount() {
        let p = Params { seq_swing: 0.3, ..Default::default() };
        let settings = SeqSettings::from_params(&p);
        let mut c = running_clock(&p);
        let mut drums = DrumSequencer::new();
        let mut melody = crate::sequencer::Sequencer::new(1);
        for step in 0..16 {
            drums.set_cell(step, 0, Cell { active: true, velocity: 1.0 });
            melody.set_step(
                step,
                crate::sequencer::Step { active: true, ..Default::default() },
            );
        }

        let mut stepped_on = Vec::new();
        let mut drums_fired_on = Vec::new();
        let mut melody_fired_on = Vec::new();
        for i in 0..4_000 {
            let adv = c.advance(BLOCK, ClockSource::Internal);
            let d = drums.advance(BLOCK, adv, c.view(), &p);
            let m = melody.advance(BLOCK, adv, c.view(), &settings);
            if d.stepped {
                stepped_on.push(i);
            }
            if d.hits.iter().any(|&level| level > 0.0) {
                drums_fired_on.push(i);
            }
            if m.note_on.is_some() {
                melody_fired_on.push(i);
            }
        }
        assert_eq!(drums_fired_on, melody_fired_on, "the two tracks swung apart");

        // And the agreement is not the trivial one of neither swinging: the
        // delay a step took, in blocks, has to be zero on the beat and
        // non-zero off it, which is what swing means.
        assert_eq!(drums_fired_on.len(), stepped_on.len(), "a step never fired");
        let delays: Vec<usize> = drums_fired_on
            .iter()
            .zip(&stepped_on)
            .map(|(fired, stepped)| fired - stepped)
            .collect();
        assert!(delays.iter().step_by(2).all(|&d| d == 0), "on-beats swung: {delays:?}");
        assert!(
            delays.iter().skip(1).step_by(2).all(|&d| d > 0),
            "off-beats did not swing: {delays:?}"
        );
    }

    /// A first-ever tick, before any block has rendered. `DrumPattern::default`
    /// starts at length zero and only `advance` reconciled the length knob, so
    /// under an external clock the very first `ClockTick` — `ClockStart` then
    /// `ClockTick` inside one event burst, before `render` ever runs — reached
    /// `step` with an empty pattern and indexed an empty slice. On the audio
    /// thread, where a panic is the one unsurvivable outcome.
    #[test]
    fn the_first_tick_before_any_block_does_not_index_an_empty_grid() {
        let p = Params {
            drum_enabled: true,
            clock_source: ClockSource::ExternalMidi,
            ..Default::default()
        };
        let c = running_clock(&p);
        let mut s = DrumSequencer::new();

        let out = s.on_tick(true, c.view(), &p);
        assert!(out.stepped, "the tick should still have stepped");
        assert_eq!(s.position(), 0);
    }

    /// The length knob has to reach the grid on the tick path too, not only
    /// when a block renders: under an external clock a knob move otherwise sat
    /// unread until something happened to call `advance`.
    #[test]
    fn a_length_change_reaches_the_grid_through_the_tick_path() {
        let mut p = Params {
            drum_length: 4,
            clock_source: ClockSource::ExternalMidi,
            ..Default::default()
        };
        let c = running_clock(&p);
        let mut s = DrumSequencer::new();
        for _ in 0..4 {
            s.on_tick(true, c.view(), &p);
        }
        assert_eq!(s.grid().len(), 4);

        p.drum_length = 8;
        s.on_tick(true, c.view(), &p);
        assert_eq!(s.grid().len(), 8, "the knob never reached the grid");
    }
}
