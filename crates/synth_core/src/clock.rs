//! Musical timing, internal or locked to external MIDI clock.
//!
//! # Why the clock lives on the audio thread
//!
//! Timing driven from a game loop inherits the game loop's jitter. At 60 fps
//! that is a 16 ms grid with spikes, and a note landing 16 ms late is audible
//! as sloppy playing — a drummer that far off the beat would be fired. The
//! audio thread, by contrast, counts samples, and a sample is 20 microseconds.
//!
//! # Resolution
//!
//! Step boundaries are quantised to the control block ([`crate::BLOCK`], 32
//! samples, ~0.7 ms). That is well below the ~10 ms at which listeners start
//! hearing timing error, and it keeps the engine's inner loop free of
//! per-sample branching. If you ever need true sample accuracy, split the block
//! at the step boundary instead — the clock already reports the exact offset.

pub use crate::params::ClockSource;

/// Ticks per quarter note in the MIDI clock spec. Fixed at 24 by the standard:
/// every device that sends MIDI clock sends exactly this many.
pub const MIDI_CLOCK_PPQN: f32 = 24.0;

/// Counts musical time in samples.
#[derive(Debug, Clone)]
pub struct Clock {
    sample_rate: f32,
    running: bool,

    /// Position within the current step, in samples.
    ///
    /// Kept in `f64` and in the sample domain rather than as a normalised `f32`
    /// phase. Adding a small `f32` increment 48000 times a second accumulates
    /// visible rounding error within seconds — enough to drop a step at exactly
    /// the wrong moment — whereas whole sample counts are represented exactly
    /// in `f64` for longer than any session will last.
    position: f64,
    /// How long one step lasts, in samples. Derived from the tempo when
    /// internal, estimated from tick spacing when external.
    samples_per_step: f64,

    /// Whole MIDI ticks received since the last step boundary.
    ticks_since_step: f32,
    /// Samples since the last MIDI tick, for tempo estimation.
    samples_since_tick: f32,
    /// Smoothed estimate of samples between MIDI ticks.
    samples_per_tick: f32,
    /// True once enough ticks have arrived for the estimate to be trustworthy.
    tick_estimate_valid: bool,
}

/// What a clock advance produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Advance {
    /// How many step boundaries were crossed. Normally 0 or 1; more only at
    /// extreme tempos or very long blocks.
    pub steps: u32,
}

impl Clock {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            running: false,
            position: 0.0,
            samples_per_step: (sample_rate * 0.125) as f64,
            ticks_since_step: 0.0,
            samples_since_tick: 0.0,
            samples_per_tick: sample_rate / 20.0,
            tick_estimate_valid: false,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.tick_estimate_valid = false;
    }

    #[inline]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Starts from the beginning of the pattern.
    pub fn start(&mut self) {
        self.running = true;
        self.position = 0.0;
        self.ticks_since_step = 0.0;
    }

    /// Resumes without rewinding.
    pub fn resume(&mut self) {
        self.running = true;
    }

    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Position within the current step, `0.0..1.0`. Used for gate lengths and
    /// swing.
    #[inline]
    pub fn phase(&self) -> f32 {
        if self.samples_per_step <= 0.0 {
            return 0.0;
        }
        (self.position / self.samples_per_step) as f32
    }

    #[inline]
    pub fn samples_per_step(&self) -> f32 {
        self.samples_per_step as f32
    }

    /// Best estimate of the current tempo in BPM. With an external clock this
    /// is derived from tick spacing, so it is what a DAW is actually running
    /// at, not what anyone typed in.
    pub fn tempo_bpm(&self, steps_per_beat: f32) -> f32 {
        if self.samples_per_step <= 0.0 {
            return 0.0;
        }
        let samples_per_beat = self.samples_per_step * steps_per_beat as f64;
        (60.0 * self.sample_rate as f64 / samples_per_beat) as f32
    }

    /// Sets the step length from a tempo. Only used by the internal clock.
    pub fn set_tempo(&mut self, bpm: f32, steps_per_beat: f32) {
        let bpm = bpm.clamp(20.0, 300.0);
        let steps_per_beat = steps_per_beat.clamp(0.25, 16.0);
        let samples_per_beat = 60.0 * self.sample_rate as f64 / bpm as f64;
        self.samples_per_step = (samples_per_beat / steps_per_beat as f64).max(1.0);
    }

    /// Feeds in one MIDI clock tick.
    ///
    /// Two things happen. The tick counter advances toward the next step
    /// boundary, and the spacing between ticks updates the step-length estimate
    /// so the sequencer can interpolate smoothly *between* ticks — without
    /// that, gate lengths and swing would quantise to the 24 PPQN grid, which
    /// at 120 BPM is a coarse 20 ms.
    pub fn on_midi_tick(&mut self, steps_per_beat: f32) -> bool {
        let ticks_per_step = (MIDI_CLOCK_PPQN / steps_per_beat.clamp(0.25, 16.0)).max(0.01);

        // Update the tempo estimate, ignoring implausible gaps: a dropped USB
        // packet or a transport stall would otherwise wreck it.
        if self.samples_since_tick > 1.0 && self.samples_since_tick < self.sample_rate {
            let measured = self.samples_since_tick;
            if self.tick_estimate_valid {
                // One-pole smoothing. MIDI clock jitters by a millisecond or so
                // over USB, and reacting to every tick makes the tempo wobble.
                self.samples_per_tick += (measured - self.samples_per_tick) * 0.15;
            } else {
                self.samples_per_tick = measured;
                self.tick_estimate_valid = true;
            }
            self.samples_per_step = (self.samples_per_tick * ticks_per_step) as f64;
            self.samples_per_step = self.samples_per_step.max(1.0);
        }
        self.samples_since_tick = 0.0;

        self.ticks_since_step += 1.0;
        if self.ticks_since_step >= ticks_per_step {
            self.ticks_since_step -= ticks_per_step;
            // Re-align to the tick, correcting any drift that accumulated while
            // interpolating between ticks.
            self.position = 0.0;
            return true;
        }
        false
    }

    /// Advances by a block of samples and reports how many steps were crossed.
    ///
    /// With an external clock the step boundaries come from
    /// [`Clock::on_midi_tick`], so this only moves the phase along for gate and
    /// swing purposes and never reports a step of its own.
    pub fn advance(&mut self, samples: usize, source: ClockSource) -> Advance {
        if !self.running {
            return Advance::default();
        }

        self.samples_since_tick += samples as f32;

        if self.samples_per_step <= 0.0 {
            return Advance::default();
        }

        self.position += samples as f64;

        match source {
            ClockSource::Internal => {
                let mut steps = 0;
                while self.position >= self.samples_per_step {
                    self.position -= self.samples_per_step;
                    steps += 1;
                    // Guard against a pathological tempo/blocksize combination
                    // producing an unbounded loop.
                    if steps > 64 {
                        self.position = 0.0;
                        break;
                    }
                }
                Advance { steps }
            }
            ClockSource::ExternalMidi => {
                // Ticks drive the steps. Hold just short of the boundary if the
                // next tick is late, rather than running ahead and then jumping
                // back when it arrives.
                if self.position > self.samples_per_step {
                    self.position = self.samples_per_step;
                }
                Advance::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BLOCK;

    #[test]
    fn internal_clock_hits_the_right_tempo() {
        let sr = 48000.0;
        let mut c = Clock::new(sr);
        // 120 BPM in sixteenths: 8 steps per second.
        c.set_tempo(120.0, 4.0);
        c.start();

        let mut steps = 0;
        // Exactly one second of audio.
        for _ in 0..(48000 / BLOCK) {
            steps += c.advance(BLOCK, ClockSource::Internal).steps;
        }
        assert_eq!(steps, 8, "expected 8 sixteenths per second at 120 BPM");
    }

    #[test]
    fn tempo_scales_the_step_count() {
        let sr = 48000.0;
        let count_at = |bpm: f32| {
            let mut c = Clock::new(sr);
            c.set_tempo(bpm, 4.0);
            c.start();
            let mut steps = 0;
            for _ in 0..(48000 / BLOCK) {
                steps += c.advance(BLOCK, ClockSource::Internal).steps;
            }
            steps
        };
        assert_eq!(count_at(60.0), 4);
        assert_eq!(count_at(120.0), 8);
        assert_eq!(count_at(240.0), 16);
    }

    #[test]
    fn a_stopped_clock_does_not_move() {
        let mut c = Clock::new(48000.0);
        c.set_tempo(120.0, 4.0);
        // Never started.
        for _ in 0..10000 {
            assert_eq!(c.advance(BLOCK, ClockSource::Internal).steps, 0);
        }
    }

    /// 24 ticks per quarter note, 4 steps per quarter note: a step every 6
    /// ticks.
    #[test]
    fn external_clock_steps_every_six_ticks() {
        let mut c = Clock::new(48000.0);
        c.start();
        let mut steps = 0;
        for _ in 0..24 {
            if c.on_midi_tick(4.0) {
                steps += 1;
            }
        }
        assert_eq!(steps, 4, "one quarter note should be 4 sixteenth steps");
    }

    #[test]
    fn external_clock_estimates_the_incoming_tempo() {
        let sr = 48000.0;
        let mut c = Clock::new(sr);
        c.start();

        // Simulate a 140 BPM source: 24 ticks per beat, beat = 60/140 s.
        let samples_per_tick = sr * 60.0 / (140.0 * 24.0);
        for _ in 0..200 {
            let mut remaining = samples_per_tick;
            while remaining > 0.0 {
                let n = remaining.min(BLOCK as f32);
                c.advance(n as usize, ClockSource::ExternalMidi);
                remaining -= n;
            }
            c.on_midi_tick(4.0);
        }

        let estimated = c.tempo_bpm(4.0);
        assert!(
            (estimated - 140.0).abs() < 2.0,
            "estimated {estimated} BPM, expected ~140"
        );
    }

    #[test]
    fn external_clock_ignores_a_dropped_tick_gap() {
        let sr = 48000.0;
        let mut c = Clock::new(sr);
        c.start();
        let samples_per_tick = sr * 60.0 / (120.0 * 24.0);

        for _ in 0..100 {
            c.advance(samples_per_tick as usize, ClockSource::ExternalMidi);
            c.on_midi_tick(4.0);
        }
        let before = c.tempo_bpm(4.0);

        // A two-second stall, then normal ticks resume.
        c.advance(96000, ClockSource::ExternalMidi);
        c.on_midi_tick(4.0);
        let after = c.tempo_bpm(4.0);

        assert!(
            (after - before).abs() < 5.0,
            "an implausible gap moved the tempo from {before} to {after}"
        );
    }

    #[test]
    fn start_rewinds_but_resume_does_not() {
        let mut c = Clock::new(48000.0);
        c.set_tempo(120.0, 4.0);
        c.start();
        c.advance(1000, ClockSource::Internal);
        let mid = c.phase();
        assert!(mid > 0.0);

        c.stop();
        c.resume();
        assert_eq!(c.phase(), mid, "resume must not rewind");

        c.start();
        assert_eq!(c.phase(), 0.0, "start must rewind");
    }
}
