//! The effects stage: a stereo delay into a plate reverb.
//!
//! The order is fixed and deliberate. Delay first means the reverb hears the
//! repeats and puts each one in the same room — a space with echoes in it.
//! Reverse them and the delay repeats the reverb tail, so each echo is a
//! smeared copy of the last, and it turns to mud within about a second.

mod delay;
mod line;
mod reverb;

pub use delay::{NoteDivision, StereoDelay};
pub use line::DelayLine;
pub use reverb::PlateReverb;

use crate::params::Params;
use crate::BLOCK;

/// Owns the effects and runs them in order.
pub struct FxChain {
    delay: StereoDelay,
    reverb: PlateReverb,
    /// Whether the smoothers have been aligned with the first real parameters
    /// they saw.
    primed: bool,
}

impl FxChain {
    /// Not real-time safe: allocates every delay line in the stage.
    pub fn new(sample_rate: f32) -> Self {
        Self {
            delay: StereoDelay::new(sample_rate),
            reverb: PlateReverb::new(sample_rate),
            primed: false,
        }
    }

    /// Rebuilds both effects for a new sample rate.
    ///
    /// Not real-time safe: call from the setup path. The lines are sized in
    /// samples, so a rate change means new allocations — and the audio already
    /// in them belongs to the old rate and would come back out at the wrong
    /// pitch, so it goes.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.delay = StereoDelay::new(sample_rate);
        self.reverb = PlateReverb::new(sample_rate);
        self.primed = false;
    }

    /// Runs the chain over a buffer of any length.
    ///
    /// The buffer is split into `BLOCK`-sized chunks so smoothing advances at
    /// the control rate regardless of what the host hands us. Without this the
    /// effects would glide at a speed set by the audio device's buffer size.
    pub fn process_block(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        params: &Params,
        tempo_bpm: f32,
    ) {
        if !self.primed {
            // The first parameters we ever see are the patch as loaded, not
            // the defaults the constructor guessed at. Jump rather than glide.
            self.delay.snap(params, tempo_bpm);
            self.reverb.snap(params);
            self.primed = true;
        }

        for (l, r) in left.chunks_mut(BLOCK).zip(right.chunks_mut(BLOCK)) {
            self.delay.process_chunk(l, r, params, tempo_bpm);
            self.reverb.process_chunk(l, r, params);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::Params;

    const SR: f32 = 48000.0;

    #[test]
    fn defaults_pass_the_signal_through_untouched() {
        let mut chain = FxChain::new(SR);
        let params = Params::default();

        let source: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.03).sin()).collect();
        let mut left = source.clone();
        let mut right = source.clone();
        chain.process_block(&mut left, &mut right, &params, 120.0);

        assert_eq!(left, source);
        assert_eq!(right, source);
    }

    #[test]
    fn smoothing_advances_per_block_not_per_call() {
        // One long call and a sequence of short calls must produce the same
        // audio, for buffer lengths that are multiples of `BLOCK`. If
        // smoothing advanced per call instead of per BLOCK, the effects
        // would sound different depending on the host's buffer size — which
        // is not something the host gets to decide.
        let mut params = Params::default();
        params.delay_mix = 0.5;
        params.delay_feedback = 0.4;
        params.reverb_mix = 0.4;

        let source: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.05).sin()).collect();

        let mut whole_l = source.clone();
        let mut whole_r = source.clone();
        let mut chain = FxChain::new(SR);
        chain.process_block(&mut whole_l, &mut whole_r, &params, 120.0);

        let mut piece_l = source.clone();
        let mut piece_r = source.clone();
        let mut chain = FxChain::new(SR);
        for (l, r) in piece_l.chunks_mut(96).zip(piece_r.chunks_mut(96)) {
            chain.process_block(l, r, &params, 120.0);
        }

        assert_eq!(whole_l, piece_l);
        assert_eq!(whole_r, piece_r);
    }

    #[test]
    fn both_effects_are_audible_together() {
        let mut params = Params::default();
        params.delay_mix = 0.5;
        params.delay_time = 0.05;
        params.delay_feedback = 0.5;
        params.reverb_mix = 0.5;
        params.reverb_size = 0.8;

        let mut chain = FxChain::new(SR);
        let mut left = vec![0.0; 24000];
        let mut right = vec![0.0; 24000];
        left[0] = 1.0;
        right[0] = 1.0;
        chain.process_block(&mut left, &mut right, &params, 120.0);

        // Long after the impulse, both the repeats and the tail are still
        // making sound.
        let late: f32 = left[12000..].iter().map(|s| s.abs()).sum();
        assert!(late > 0.1, "nothing left by halfway: {late}");
        assert!(left.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn changing_the_sample_rate_clears_the_effects() {
        let mut params = Params::default();
        params.reverb_mix = 1.0;
        params.reverb_size = 0.9;

        let mut chain = FxChain::new(SR);
        let mut left = vec![1.0; 4096];
        let mut right = vec![1.0; 4096];
        chain.process_block(&mut left, &mut right, &params, 120.0);

        // The tank is full of the old audio. After a rate change it must not
        // come back out at the wrong pitch.
        chain.set_sample_rate(44100.0);
        let mut left = vec![0.0; 512];
        let mut right = vec![0.0; 512];
        chain.process_block(&mut left, &mut right, &params, 120.0);
        for (i, sample) in left.iter().enumerate() {
            assert!(sample.abs() < 1e-6, "sample {i} leaked: {sample}");
        }
    }

    #[test]
    fn a_buffer_length_that_is_not_a_multiple_of_block_does_not_panic() {
        // Every other test above uses a multiple of BLOCK (32). The engine
        // does not get to guarantee that, so the partial-chunk path needs its
        // own coverage.
        let mut params = Params::default();
        params.delay_mix = 0.5;
        params.delay_feedback = 0.4;
        params.reverb_mix = 0.4;
        params.reverb_size = 0.7;

        let mut chain = FxChain::new(SR);
        let source: Vec<f32> = (0..100).map(|i| (i as f32 * 0.07).sin()).collect();
        let mut left = source.clone();
        let mut right = source;
        chain.process_block(&mut left, &mut right, &params, 120.0);

        assert!(left.iter().all(|s| s.is_finite()));
        assert!(right.iter().all(|s| s.is_finite()));
    }
}
