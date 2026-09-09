//! The rack: eight voices, one grid, one mono bus.
//!
//! The rack is where a hit becomes a sound. It owns the sequencer and the
//! voices, applies the per-pad controls the sequencer knows nothing about
//! (level, tuning, decay scaling, mute), resolves choke groups, and sums the
//! result into a scratch buffer the engine can mix.
//!
//! [`DrumRack::render`] returns whether it produced anything. That boolean is
//! the hard bypass: with an empty grid and no pad ringing the engine skips the
//! drum bus entirely, which is what keeps the golden vector byte-identical for
//! a patch that never touches the drums.

use super::sequencer::{DrumOutput, DrumSequencer};
use super::{DrumVoice, Pad, PAD_COUNT};
use crate::clock::{Advance, ClockView};
use crate::params::Params;
use crate::BLOCK;

pub struct DrumRack {
    sequencer: DrumSequencer,
    voices: [DrumVoice; PAD_COUNT],
    /// Mono scratch. The engine pans it; the rack only sums.
    buffer: [f32; BLOCK],
    sample_rate: f32,
}

impl DrumRack {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        Self {
            sequencer: DrumSequencer::new(),
            // Each voice gets its own stream, or eight noise pads would draw
            // identical numbers and sum into one loud correlated hiss.
            voices: core::array::from_fn(|i| {
                DrumVoice::new(sample_rate, seed ^ (0x9E37_79B9 * (i as u64 + 1)))
            }),
            buffer: [0.0; BLOCK],
            sample_rate: sample_rate.max(1.0),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for voice in &mut self.voices {
            voice.set_sample_rate(self.sample_rate);
        }
    }

    pub fn sequencer(&mut self) -> &mut DrumSequencer {
        &mut self.sequencer
    }

    pub fn sequencer_ref(&self) -> &DrumSequencer {
        &self.sequencer
    }

    pub fn silence(&mut self) {
        for voice in &mut self.voices {
            voice.silence();
        }
        self.buffer = [0.0; BLOCK];
    }

    /// The external-clock path. The engine calls this from its MIDI tick
    /// handler; the strikes land on the next `render`.
    pub fn on_tick(&mut self, ticked: bool, clock: ClockView, p: &Params) {
        if !p.drum_enabled {
            return;
        }
        let out = self.sequencer.on_tick(ticked, clock, p);
        self.strike(out, p);
    }

    /// Fill the scratch buffer. Returns false when there was nothing to make —
    /// the engine then skips the drum bus altogether.
    pub fn render(&mut self, frames: usize, adv: Advance, clock: ClockView, p: &Params) -> bool {
        let frames = frames.min(BLOCK);
        if !p.drum_enabled {
            // Still track the length knob. The grid mirror the UI draws from
            // is published whether or not the rack is switched on, and a
            // mirror stuck at length zero would show an empty machine.
            self.sequencer.reconcile_length(p);
            self.buffer[..frames].fill(0.0);
            return false;
        }

        let hits = self.sequencer.advance(frames, adv, clock, p);
        self.strike(hits, p);

        // Nothing struck and nothing ringing: zero the scratch so a stale tail
        // can never be read back, and tell the engine not to bother mixing.
        if self.voices.iter().all(|v| v.is_silent()) {
            self.buffer[..frames].fill(0.0);
            return false;
        }

        let level = p.drum_level;
        for slot in self.buffer[..frames].iter_mut() {
            let mut sum = 0.0;
            for voice in self.voices.iter_mut() {
                sum += voice.next();
            }
            *slot = sum * level;
        }
        true
    }

    /// The mono drum bus for the block just rendered.
    pub fn output(&self, frames: usize) -> &[f32] {
        &self.buffer[..frames.min(BLOCK)]
    }

    /// Turn a column of velocities into strikes, applying the per-pad controls
    /// and resolving choke groups.
    fn strike(&mut self, out: DrumOutput, p: &Params) {
        for index in 0..PAD_COUNT {
            let velocity = out.hits[index];
            if velocity <= 0.0 || p.pad_mute[index] {
                continue;
            }
            let pad = Pad::ALL[index];

            // A hat cuts its group short before it sounds. `voice.pad()` is
            // what makes this read the voice rather than assume the index —
            // an untouched voice still reports `Kick`, whose group is `None`,
            // so nothing is choked before its first strike. A column that
            // hits both hats leaves the later pad ringing: deterministic, and
            // not a pattern anyone writes on purpose.
            if let Some(group) = pad.choke_group() {
                for (other, voice) in self.voices.iter_mut().enumerate() {
                    if other != index && voice.pad().choke_group() == Some(group) {
                        voice.silence();
                    }
                }
            }

            self.voices[index].strike(
                pad,
                velocity * p.pad_level[index],
                p.pad_tune[index],
                p.pad_decay[index],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::drums::Cell;
    use crate::params::{ClockSource, Params};
    use crate::BLOCK;

    fn setup(p: &Params) -> (DrumRack, Clock) {
        let mut c = Clock::new(48_000.0);
        c.set_tempo(p.tempo, p.steps_per_beat);
        c.start();
        (DrumRack::new(48_000.0, 0x51D), c)
    }

    fn render(r: &mut DrumRack, c: &mut Clock, p: &Params) -> (bool, f32) {
        let adv = c.advance(BLOCK, ClockSource::Internal);
        let sounded = r.render(BLOCK, adv, c.view(), p);
        let peak = r
            .output(BLOCK)
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        (sounded, peak)
    }

    /// The whole reason the golden vector survives: an empty rack does not
    /// merely render silence, it declines to render at all.
    #[test]
    fn an_empty_rack_reports_nothing_to_mix() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        for _ in 0..64 {
            let (sounded, peak) = render(&mut rack, &mut clock, &p);
            assert!(!sounded);
            assert_eq!(peak, 0.0);
        }
    }

    /// One hit on the grid and the rack starts producing audio.
    #[test]
    fn a_grid_with_a_hit_makes_sound() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });

        let mut peak = 0.0f32;
        for _ in 0..200 {
            let (_, block_peak) = render(&mut rack, &mut clock, &p);
            peak = peak.max(block_peak);
        }
        assert!(peak > 0.05, "the kick never reached the bus: {peak}");
    }

    /// Mute is per pad and lives in the rack, not the grid: the pattern keeps
    /// its hits, they just do not strike anything.
    #[test]
    fn a_muted_pad_stays_silent() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        p.pad_mute[0] = true;
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });

        for _ in 0..200 {
            let (sounded, peak) = render(&mut rack, &mut clock, &p);
            assert!(!sounded);
            assert_eq!(peak, 0.0);
        }
    }

    /// The choke group is what makes a hat pair sound like one hat: closing it
    /// has to cut the open one short.
    #[test]
    fn a_closed_hat_chokes_the_open_one() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        // Open hat on 0, closed hat two steps later.
        rack.sequencer()
            .set_cell(0, 3, Cell { active: true, velocity: 1.0 });
        rack.sequencer()
            .set_cell(2, 2, Cell { active: true, velocity: 1.0 });
        p.drum_length = 16;

        // Measure the tail well after the closed hat has itself decayed
        // (45 ms) but while a free-running open hat (380 ms) would still ring.
        let mut choked_tail = 0.0f32;
        let mut steps = 0;
        for _ in 0..4_000 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            let stepped = adv.steps > 0;
            rack.render(BLOCK, adv, clock.view(), &p);
            if stepped {
                steps += 1;
            }
            // Between step 3 and step 4: past the closed hat's own decay.
            if steps == 4 {
                let peak = rack.output(BLOCK).iter().fold(0.0f32, |m, s| m.max(s.abs()));
                choked_tail = choked_tail.max(peak);
            }
        }

        // Now the same pattern with the closed hat removed.
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 3, Cell { active: true, velocity: 1.0 });
        let mut free_tail = 0.0f32;
        let mut steps = 0;
        for _ in 0..4_000 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            let stepped = adv.steps > 0;
            rack.render(BLOCK, adv, clock.view(), &p);
            if stepped {
                steps += 1;
            }
            if steps == 4 {
                let peak = rack.output(BLOCK).iter().fold(0.0f32, |m, s| m.max(s.abs()));
                free_tail = free_tail.max(peak);
            }
        }

        assert!(free_tail > 0.0, "the open hat never rang at all");
        assert!(
            choked_tail < free_tail * 0.5,
            "the closed hat did not choke the open one: {choked_tail} vs {free_tail}"
        );
    }

    /// The enable switch is checked in the rack, so a disabled rack costs one
    /// branch per block and nothing else.
    #[test]
    fn a_disabled_rack_never_renders() {
        let p = Params { drum_enabled: false, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });

        for _ in 0..200 {
            let (sounded, peak) = render(&mut rack, &mut clock, &p);
            assert!(!sounded);
            assert_eq!(peak, 0.0);
        }
    }
}
