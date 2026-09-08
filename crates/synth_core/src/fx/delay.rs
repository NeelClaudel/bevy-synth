//! The stereo delay, and the note divisions it can lock to.

/// How long one delay repeat lasts, in musical time.
///
/// Ordered longest to shortest, which is how they read in a dropdown and how
/// a musician thinks about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum NoteDivision {
    Whole = 0,
    Half = 1,
    QuarterDot = 2,
    Quarter = 3,
    EighthDot = 4,
    #[default]
    Eighth = 5,
    EighthTriplet = 6,
    Sixteenth = 7,
    SixteenthTriplet = 8,
}

impl NoteDivision {
    pub const ALL: [NoteDivision; 9] = [
        NoteDivision::Whole,
        NoteDivision::Half,
        NoteDivision::QuarterDot,
        NoteDivision::Quarter,
        NoteDivision::EighthDot,
        NoteDivision::Eighth,
        NoteDivision::EighthTriplet,
        NoteDivision::Sixteenth,
        NoteDivision::SixteenthTriplet,
    ];

    pub fn from_u32(value: u32) -> Self {
        match value {
            0 => NoteDivision::Whole,
            1 => NoteDivision::Half,
            2 => NoteDivision::QuarterDot,
            3 => NoteDivision::Quarter,
            4 => NoteDivision::EighthDot,
            5 => NoteDivision::Eighth,
            6 => NoteDivision::EighthTriplet,
            7 => NoteDivision::Sixteenth,
            8 => NoteDivision::SixteenthTriplet,
            _ => NoteDivision::Eighth,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            NoteDivision::Whole => "1/1",
            NoteDivision::Half => "1/2",
            NoteDivision::QuarterDot => "1/4.",
            NoteDivision::Quarter => "1/4",
            NoteDivision::EighthDot => "1/8.",
            NoteDivision::Eighth => "1/8",
            NoteDivision::EighthTriplet => "1/8T",
            NoteDivision::Sixteenth => "1/16",
            NoteDivision::SixteenthTriplet => "1/16T",
        }
    }

    /// Length in beats, where one beat is a quarter note.
    pub fn beats(self) -> f32 {
        match self {
            NoteDivision::Whole => 4.0,
            NoteDivision::Half => 2.0,
            NoteDivision::QuarterDot => 1.5,
            NoteDivision::Quarter => 1.0,
            NoteDivision::EighthDot => 0.75,
            NoteDivision::Eighth => 0.5,
            NoteDivision::EighthTriplet => 1.0 / 3.0,
            NoteDivision::Sixteenth => 0.25,
            NoteDivision::SixteenthTriplet => 1.0 / 6.0,
        }
    }

    /// Length in seconds at the given tempo.
    ///
    /// The tempo is clamped because it may have come from `Clock::tempo_bpm`,
    /// which reports 0.0 until an external clock has been running long enough
    /// to measure — and 0 BPM means an infinitely long note.
    pub fn seconds(self, tempo_bpm: f32) -> f32 {
        let bpm = if tempo_bpm.is_finite() {
            tempo_bpm.clamp(20.0, 300.0)
        } else {
            120.0
        };
        self.beats() * 60.0 / bpm
    }
}

use crate::fx::line::{flush, DelayLine};
use crate::params::{Params, Smoothed};
use crate::BLOCK;

/// Longest delay the lines are sized for, matching the parameter's range.
const MAX_DELAY_SECONDS: f32 = 2.0;
/// Shortest usable delay. Below about a millisecond this stops being a delay
/// and starts being a comb filter.
const MIN_DELAY_SECONDS: f32 = 0.001;
/// How fast the delay time chases a new setting. Slow on purpose: this is what
/// gives the tape-style pitch glide when you turn the time knob.
const TIME_GLIDE_MS: f32 = 120.0;
/// Everything else settles quickly — long enough to kill the zipper, short
/// enough that the knob feels connected.
const LEVEL_GLIDE_MS: f32 = 20.0;

/// Two delay lines with a shared time, damped feedback and optional crossing.
pub struct StereoDelay {
    left: DelayLine,
    right: DelayLine,
    sample_rate: f32,

    /// Delay time in samples. Smoothed, then linearly interpolated across the
    /// chunk, because a step in the read position is an audible click.
    time: Smoothed,
    mix: Smoothed,
    feedback: Smoothed,
    damping: Smoothed,

    /// One-pole state for the damping filter in each feedback path.
    damp_left: f32,
    damp_right: f32,

    /// Tracks whether the delay is fully bypassed, to clear on transition.
    bypassed: bool,
}

impl StereoDelay {
    /// Not real-time safe: allocates both lines.
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 { sample_rate } else { 48000.0 };
        let control_rate = sample_rate / BLOCK as f32;
        let max_samples = (MAX_DELAY_SECONDS * sample_rate).ceil() as usize;
        let default = Params::default();
        Self {
            left: DelayLine::new(max_samples),
            right: DelayLine::new(max_samples),
            sample_rate,
            time: Smoothed::new(default.delay_time * sample_rate, TIME_GLIDE_MS, control_rate),
            mix: Smoothed::new(default.delay_mix, LEVEL_GLIDE_MS, control_rate),
            feedback: Smoothed::new(default.delay_feedback, LEVEL_GLIDE_MS, control_rate),
            damping: Smoothed::new(default.delay_damping, LEVEL_GLIDE_MS, control_rate),
            damp_left: 0.0,
            damp_right: 0.0,
            bypassed: false,
        }
    }

    /// Jumps every smoother to where the parameters say it should be and
    /// empties the lines.
    ///
    /// Used at startup and whenever the sample rate changes: gliding up from
    /// whatever the previous patch happened to leave behind would be an audible
    /// artefact nobody asked for.
    pub fn snap(&mut self, params: &Params, tempo_bpm: f32) {
        self.left.clear();
        self.right.clear();
        self.damp_left = 0.0;
        self.damp_right = 0.0;
        self.bypassed = false;
        self.set_targets(params, tempo_bpm);
        self.time.snap_to_target();
        self.mix.snap_to_target();
        self.feedback.snap_to_target();
        self.damping.snap_to_target();
    }

    fn set_targets(&mut self, params: &Params, tempo_bpm: f32) {
        let seconds = if params.delay_sync {
            params.delay_division.seconds(tempo_bpm)
        } else {
            params.delay_time
        };
        let samples = seconds.clamp(MIN_DELAY_SECONDS, MAX_DELAY_SECONDS) * self.sample_rate;
        let ceiling = (self.left.capacity() - 1) as f32;
        self.time.set_target(samples.clamp(1.0, ceiling));
        self.mix.set_target(params.delay_mix);
        self.feedback.set_target(params.delay_feedback);
        self.damping.set_target(params.delay_damping);
    }

    /// Processes up to `BLOCK` samples in place, advancing smoothing once.
    pub fn process_chunk(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        params: &Params,
        tempo_bpm: f32,
    ) {
        // Fully off: skip the work entirely rather than multiplying by zero.
        // This is what makes a dry patch bit-identical, and it also means an
        // unused delay costs nothing per sample.
        if params.delay_mix == 0.0 && self.mix.value() == 0.0 {
            if !self.bypassed {
                self.left.clear();
                self.right.clear();
                self.damp_left = 0.0;
                self.damp_right = 0.0;
                self.bypassed = true;
            }
            return;
        }
        self.bypassed = false;

        self.set_targets(params, tempo_bpm);

        let time_from = self.time.value();
        let time_to = self.time.next();
        let mix = self.mix.next();
        let feedback = self.feedback.next();
        let damping = self.damping.next();
        // 1.0 is a wire, 0.05 is a heavily muffled repeat. Never 0.0: that
        // would freeze the filter and mute the feedback path outright.
        let damp_coefficient = 1.0 - damping * 0.95;

        let count = left.len().min(right.len());
        let step = if count > 1 {
            (time_to - time_from) / (count - 1) as f32
        } else {
            0.0
        };

        for i in 0..count {
            let time = time_from + step * i as f32 - 1.0;
            let dry_left = left[i];
            let dry_right = right[i];

            // The line is read before this sample is written, so reading one short of the
            // delay time puts the repeat exactly at the interpolated delay, which varies
            // from `time_from` to `time_to` across the chunk.
            let wet_left = self.left.read_frac(time);
            let wet_right = self.right.read_frac(time);

            // Damp what goes back round the loop, not what comes out: the
            // first repeat stays bright and each one after it gets darker,
            // which is how a real echo behaves.
            self.damp_left += damp_coefficient * (wet_left - self.damp_left);
            self.damp_right += damp_coefficient * (wet_right - self.damp_right);
            // No measured cost today (the delay has no dense recirculating
            // lattice to sustain a denormal), but flush anyway so the
            // convention stays uniform across `fx/` rather than half-applied.
            self.damp_left = flush(self.damp_left);
            self.damp_right = flush(self.damp_right);

            if params.delay_ping_pong {
                // Cross both the input and the feedback, so a sound entering
                // on the left first reappears on the right, then the left.
                self.left.write(dry_right + self.damp_right * feedback);
                self.right.write(dry_left + self.damp_left * feedback);
            } else {
                self.left.write(dry_left + self.damp_left * feedback);
                self.right.write(dry_right + self.damp_right * feedback);
            }

            left[i] = dry_left + (wet_left - dry_left) * mix;
            right[i] = dry_right + (wet_right - dry_right) * mix;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisions_are_the_right_length_at_120_bpm() {
        // At 120 BPM a quarter note is half a second.
        assert_eq!(NoteDivision::Quarter.seconds(120.0), 0.5);
        assert_eq!(NoteDivision::Eighth.seconds(120.0), 0.25);
        assert_eq!(NoteDivision::QuarterDot.seconds(120.0), 0.75);
        assert_eq!(NoteDivision::Whole.seconds(120.0), 2.0);
        assert!((NoteDivision::EighthTriplet.seconds(120.0) - 1.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn division_length_scales_inversely_with_tempo() {
        assert_eq!(NoteDivision::Quarter.seconds(60.0), 1.0);
        assert_eq!(NoteDivision::Quarter.seconds(240.0), 0.25);
    }

    #[test]
    fn an_absurd_tempo_still_gives_a_usable_delay_time() {
        // `Clock::tempo_bpm` returns 0.0 before the first external clock tick
        // arrives. Left alone that is a division by zero and an infinite delay
        // time, so the clamp is load-bearing, not defensive decoration.
        assert!(NoteDivision::Quarter.seconds(0.0).is_finite());
        assert!(NoteDivision::Quarter.seconds(0.0) > 0.0);
        assert!(NoteDivision::Quarter.seconds(f32::NAN).is_finite());
        assert!(NoteDivision::Quarter.seconds(1.0e9).is_finite());
    }

    #[test]
    fn divisions_are_ordered_longest_first() {
        let lengths: Vec<f32> = NoteDivision::ALL.iter().map(|d| d.beats()).collect();
        for pair in lengths.windows(2) {
            assert!(pair[0] > pair[1], "{:?} is not descending", lengths);
        }
    }

    #[test]
    fn every_variant_round_trips_through_u32() {
        for &division in &NoteDivision::ALL {
            assert_eq!(NoteDivision::from_u32(division as u32), division);
            assert!(!division.name().is_empty());
        }
        // Out of range falls back rather than panicking: the value came across
        // an atomic from another thread and cannot be trusted.
        assert_eq!(NoteDivision::from_u32(999), NoteDivision::Eighth);
    }

    use crate::params::Params;

    const SR: f32 = 48000.0;

    /// Runs a delay over a buffer of any length, in `BLOCK`-sized chunks, the
    /// way `FxChain` will.
    fn run(delay: &mut StereoDelay, left: &mut [f32], right: &mut [f32], params: &Params) {
        for (l, r) in left
            .chunks_mut(crate::BLOCK)
            .zip(right.chunks_mut(crate::BLOCK))
        {
            delay.process_chunk(l, r, params, 120.0);
        }
    }

    #[test]
    fn a_zero_mix_leaves_the_signal_bit_identical() {
        let mut delay = StereoDelay::new(SR);
        let params = Params::default();
        delay.snap(&params, 120.0);

        let source: Vec<f32> = (0..512).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut left = source.clone();
        let mut right = source.clone();
        run(&mut delay, &mut left, &mut right, &params);

        // Not "close enough": identical. A dry patch must be untouched.
        assert_eq!(left, source);
        assert_eq!(right, source);
    }

    #[test]
    fn an_impulse_comes_back_at_the_configured_time() {
        let mut params = Params::default();
        params.delay_mix = 1.0;
        params.delay_feedback = 0.0;
        params.delay_time = 0.01; // 480 samples at 48 kHz
        params.delay_damping = 0.0;

        let mut delay = StereoDelay::new(SR);
        delay.snap(&params, 120.0);

        let mut left = vec![0.0; 2048];
        let mut right = vec![0.0; 2048];
        left[0] = 1.0;
        right[0] = 1.0;
        run(&mut delay, &mut left, &mut right, &params);

        let loudest = left
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(loudest, 480, "repeat landed at {loudest}");
        assert!((left[480] - 1.0).abs() < 1e-6);
        // With no feedback there is exactly one repeat.
        assert!(left[960].abs() < 1e-6);
    }

    #[test]
    fn feedback_halves_each_repeat() {
        let mut params = Params::default();
        params.delay_mix = 1.0;
        params.delay_feedback = 0.5;
        params.delay_time = 0.01;
        params.delay_damping = 0.0;

        let mut delay = StereoDelay::new(SR);
        delay.snap(&params, 120.0);

        let mut left = vec![0.0; 4096];
        let mut right = vec![0.0; 4096];
        left[0] = 1.0;
        run(&mut delay, &mut left, &mut right, &params);

        assert!((left[480] - 1.0).abs() < 1e-4, "first: {}", left[480]);
        assert!((left[960] - 0.5).abs() < 1e-4, "second: {}", left[960]);
        assert!((left[1440] - 0.25).abs() < 1e-4, "third: {}", left[1440]);
    }

    #[test]
    fn ping_pong_bounces_the_repeats_between_the_channels() {
        let mut params = Params::default();
        params.delay_mix = 1.0;
        params.delay_feedback = 0.6;
        params.delay_time = 0.01;
        params.delay_damping = 0.0;
        params.delay_ping_pong = true;

        let mut delay = StereoDelay::new(SR);
        delay.snap(&params, 120.0);

        let mut left = vec![0.0; 4096];
        let mut right = vec![0.0; 4096];
        left[0] = 1.0; // signal into the left channel only
        run(&mut delay, &mut left, &mut right, &params);

        // First repeat crosses to the right, second comes back to the left.
        assert!(right[480].abs() > 0.5, "first repeat: {}", right[480]);
        assert!(left[480].abs() < 1e-4, "leaked into left: {}", left[480]);
        assert!(left[960].abs() > 0.3, "second repeat: {}", left[960]);
    }

    #[test]
    fn sync_takes_its_time_from_the_tempo() {
        let mut params = Params::default();
        params.delay_mix = 1.0;
        params.delay_feedback = 0.0;
        params.delay_sync = true;
        params.delay_division = NoteDivision::Eighth;
        params.delay_time = 2.0; // ignored while sync is on
        params.delay_damping = 0.0;

        let mut delay = StereoDelay::new(SR);
        delay.snap(&params, 120.0);

        let mut left = vec![0.0; 32768];
        let mut right = vec![0.0; 32768];
        left[0] = 1.0;
        run(&mut delay, &mut left, &mut right, &params);

        // 1/8 at 120 BPM is 0.25 s: 12000 samples.
        let loudest = left
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(loudest, 12000, "repeat landed at {loudest}");
    }

    #[test]
    fn maximum_feedback_stays_bounded() {
        let mut params = Params::default();
        params.delay_mix = 1.0;
        params.delay_feedback = 0.95;
        params.delay_time = 0.005;
        params.delay_damping = 0.0;

        let mut delay = StereoDelay::new(SR);
        delay.snap(&params, 120.0);

        let mut left: Vec<f32> = (0..48000 * 4).map(|i| ((i % 71) as f32 / 35.0) - 1.0).collect();
        let mut right = left.clone();
        run(&mut delay, &mut left, &mut right, &params);

        for (i, sample) in left.iter().enumerate() {
            assert!(sample.is_finite(), "sample {i} was {sample}");
            assert!(sample.abs() < 40.0, "sample {i} was {sample}");
        }
    }

    #[test]
    fn bypass_clears_tail_on_reenable() {
        // Drive an impulse through the delay with feedback.
        let mut params = Params::default();
        params.delay_mix = 1.0;
        params.delay_feedback = 0.5;
        params.delay_time = 0.01; // 480 samples at 48 kHz
        params.delay_damping = 0.0;

        let mut delay = StereoDelay::new(SR);
        delay.snap(&params, 120.0);

        let mut left = vec![0.0; 6000];
        let mut right = vec![0.0; 6000];
        left[0] = 1.0;

        // Run until the first repeat is clearly audible and fed back.
        run(&mut delay, &mut left[..960], &mut right[..960], &params);
        let first_repeat_amplitude = left[480].abs();
        assert!(first_repeat_amplitude > 0.9, "first repeat: {}", first_repeat_amplitude);

        // Now turn off the delay and run long enough for the mix smoother to
        // settle and engage the early-out bypass.
        params.delay_mix = 0.0;
        run(&mut delay, &mut left[960..2960], &mut right[960..2960], &params);

        // Re-enable the delay.
        params.delay_mix = 1.0;
        run(&mut delay, &mut left[2960..6000], &mut right[2960..6000], &params);

        // The delay lines were cleared, so any signal here is fresh input (which
        // is zero). No resurgent tail should appear.
        let max_after_reenable = left[2960..]
            .iter()
            .copied()
            .map(|x| x.abs())
            .fold(0.0_f32, f32::max);
        assert!(
            max_after_reenable < 0.01,
            "resurgent tail after reenable: {}",
            max_after_reenable
        );
    }
}
