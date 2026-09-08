//! A plate reverb, following Jon Dattorro's 1997 design.
//!
//! The structure is a pre-delay and four cascaded all-pass diffusers feeding a
//! figure-of-eight "tank": two halves, each an all-pass, a delay, a damping
//! filter, another all-pass and another delay, with each half's output feeding
//! the other's input. Seven fixed taps per channel, drawn from points scattered
//! through both halves, turn the single mono tank into a stereo output. That
//! is where the width comes from — not from running two reverbs.

use crate::fx::line::DelayLine;
use crate::params::{Params, Smoothed};
use crate::BLOCK;

/// The sample rate Dattorro's published delay lengths are given at. Every
/// length below is scaled from this to whatever we are actually running at.
const REFERENCE_RATE: f32 = 29761.0;

/// Input diffuser lengths and coefficients, from the paper.
const DIFFUSER_LENGTHS: [usize; 4] = [142, 107, 379, 277];
const DIFFUSER_COEFFICIENTS: [f32; 4] = [0.75, 0.75, 0.625, 0.625];

/// Tank all-pass and delay lengths. `L` and `R` name the two halves of the
/// figure of eight, not the output channels.
const AP_L1: usize = 672;
const DELAY_L1: usize = 4453;
const AP_L2: usize = 1800;
const DELAY_L2: usize = 3720;
const AP_R1: usize = 908;
const DELAY_R1: usize = 4217;
const AP_R2: usize = 2656;
const DELAY_R2: usize = 3163;

/// Diffusion inside the tank's first all-pass pair.
const TANK_DIFFUSION_1: f32 = 0.7;

/// Input low-pass. Near enough to open; it only exists to keep the very top
/// of the spectrum out of a structure that will smear it into hiss.
const INPUT_BANDWIDTH: f32 = 0.9995;

/// How far the tank's first all-pass pair is modulated, at the reference rate.
///
/// Without this the tank rings: the same fixed set of delays reinforcing each
/// other forever is exactly how you build a metallic comb. A slow wobble of a
/// few samples decorrelates them and the ring becomes a wash.
const EXCURSION: f32 = 16.0;
/// Modulation rate in Hz, and a ratio for the second half so the two never
/// line up.
const MOD_HZ: f32 = 0.7;
const MOD_HZ_RATIO: f32 = 1.618;

const MAX_PREDELAY_SECONDS: f32 = 0.25;
const GLIDE_MS: f32 = 25.0;
/// Slower for pre-delay: it moves the read position, so a step is a click.
const PREDELAY_GLIDE_MS: f32 = 120.0;

/// A Schroeder all-pass: flat magnitude response, scrambled phase. Diffusion
/// without colouration, which is the whole trick a reverb is built on.
struct Allpass {
    line: DelayLine,
    length: usize,
}

impl Allpass {
    fn new(length: usize, headroom: usize) -> Self {
        let length = length.max(1);
        Self {
            line: DelayLine::new(length + headroom + 2),
            length,
        }
    }

    #[inline]
    fn process(&mut self, input: f32, coefficient: f32) -> f32 {
        let delayed = self.line.read(self.length - 1);
        let stored = input + coefficient * delayed;
        self.line.write(stored);
        delayed - coefficient * stored
    }

    /// As `process`, but the read position wobbles by `excursion` samples.
    #[inline]
    fn process_modulated(&mut self, input: f32, coefficient: f32, excursion: f32) -> f32 {
        let delayed = self.line.read_frac((self.length - 1) as f32 + excursion);
        let stored = input + coefficient * delayed;
        self.line.write(stored);
        delayed - coefficient * stored
    }

    #[inline]
    fn tap(&self, offset: usize) -> f32 {
        self.line.read(offset)
    }

    fn clear(&mut self) {
        self.line.clear();
    }
}

/// A delay inside the tank: fixed length, plus taps at other offsets.
struct TankDelay {
    line: DelayLine,
    length: usize,
}

impl TankDelay {
    fn new(length: usize) -> Self {
        let length = length.max(1);
        Self {
            line: DelayLine::new(length + 2),
            length,
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        self.line.write(input);
        self.line.read(self.length - 1)
    }

    #[inline]
    fn tap(&self, offset: usize) -> f32 {
        self.line.read(offset)
    }

    fn clear(&mut self) {
        self.line.clear();
    }
}

/// The seven tap offsets per output channel, already scaled to the running
/// sample rate.
struct Taps {
    left: [usize; 7],
    right: [usize; 7],
}

/// Dattorro's plate.
pub struct PlateReverb {
    sample_rate: f32,

    predelay: DelayLine,
    bandwidth_state: f32,

    diffusers: [Allpass; 4],

    ap_l1: Allpass,
    delay_l1: TankDelay,
    ap_l2: Allpass,
    delay_l2: TankDelay,
    damp_l: f32,

    ap_r1: Allpass,
    delay_r1: TankDelay,
    ap_r2: Allpass,
    delay_r2: TankDelay,
    damp_r: f32,

    /// Each half's output, carried to the other half on the next sample. The
    /// one-sample gap is what makes a loop that feeds itself computable.
    node_l: f32,
    node_r: f32,

    mod_phase: f32,
    mod_increment: f32,
    excursion: f32,

    taps: Taps,

    mix: Smoothed,
    decay: Smoothed,
    damping: Smoothed,
    predelay_samples: Smoothed,
    width: Smoothed,

    /// Tracks whether the reverb is fully bypassed, to clear on transition.
    bypassed: bool,
}

/// Scales one of the paper's lengths to the running sample rate.
fn scaled(length: usize, ratio: f32) -> usize {
    ((length as f32 * ratio).round() as usize).max(1)
}

/// Maps `reverb_size` to the tank's decay coefficient.
///
/// Never reaches 1.0. At 1.0 the tank is a perfect loop and the tail is
/// infinite, which is a freeze effect, not a reverb.
fn decay_for(size: f32) -> f32 {
    0.2 + size.clamp(0.0, 1.0) * 0.75
}

impl PlateReverb {
    /// Not real-time safe: allocates twelve delay lines.
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 { sample_rate } else { 48000.0 };
        let ratio = sample_rate / REFERENCE_RATE;
        let control_rate = sample_rate / BLOCK as f32;
        let excursion = EXCURSION * ratio;
        let headroom = excursion.ceil() as usize;
        let default = Params::default();

        let taps = Taps {
            // The left output is drawn mostly from the right half of the tank,
            // and vice versa. That asymmetry is what decorrelates the two.
            left: [
                scaled(266, ratio),
                scaled(2974, ratio),
                scaled(1913, ratio),
                scaled(1996, ratio),
                scaled(1990, ratio),
                scaled(187, ratio),
                scaled(1066, ratio),
            ],
            right: [
                scaled(353, ratio),
                scaled(3627, ratio),
                scaled(1228, ratio),
                scaled(2673, ratio),
                scaled(2111, ratio),
                scaled(335, ratio),
                scaled(121, ratio),
            ],
        };

        Self {
            sample_rate,
            predelay: DelayLine::new((MAX_PREDELAY_SECONDS * sample_rate).ceil() as usize),
            bandwidth_state: 0.0,
            diffusers: [
                Allpass::new(scaled(DIFFUSER_LENGTHS[0], ratio), 0),
                Allpass::new(scaled(DIFFUSER_LENGTHS[1], ratio), 0),
                Allpass::new(scaled(DIFFUSER_LENGTHS[2], ratio), 0),
                Allpass::new(scaled(DIFFUSER_LENGTHS[3], ratio), 0),
            ],
            ap_l1: Allpass::new(scaled(AP_L1, ratio), headroom),
            delay_l1: TankDelay::new(scaled(DELAY_L1, ratio)),
            ap_l2: Allpass::new(scaled(AP_L2, ratio), 0),
            delay_l2: TankDelay::new(scaled(DELAY_L2, ratio)),
            damp_l: 0.0,
            ap_r1: Allpass::new(scaled(AP_R1, ratio), headroom),
            delay_r1: TankDelay::new(scaled(DELAY_R1, ratio)),
            ap_r2: Allpass::new(scaled(AP_R2, ratio), 0),
            delay_r2: TankDelay::new(scaled(DELAY_R2, ratio)),
            damp_r: 0.0,
            node_l: 0.0,
            node_r: 0.0,
            mod_phase: 0.0,
            mod_increment: MOD_HZ / sample_rate,
            excursion,
            taps,
            mix: Smoothed::new(default.reverb_mix, GLIDE_MS, control_rate),
            decay: Smoothed::new(decay_for(default.reverb_size), GLIDE_MS, control_rate),
            damping: Smoothed::new(default.reverb_damping, GLIDE_MS, control_rate),
            predelay_samples: Smoothed::new(
                default.reverb_predelay * sample_rate,
                PREDELAY_GLIDE_MS,
                control_rate,
            ),
            width: Smoothed::new(default.reverb_width, GLIDE_MS, control_rate),
            bypassed: false,
        }
    }

    /// Empties the tank: pre-delay, the input bandwidth filter, the four
    /// diffusers, the eight tank delay/all-pass lines, the damping states, the
    /// cross-feed nodes, and the modulation phase.
    ///
    /// Factored out of `snap` so the bypass early-out in `process_chunk` can
    /// call it too. Without this a bypassed reverb would leave the tank
    /// holding whatever was in it, and turning the mix back up would surface a
    /// stale tail from minutes earlier instead of starting clean.
    fn clear_tank(&mut self) {
        self.predelay.clear();
        self.bandwidth_state = 0.0;
        for diffuser in &mut self.diffusers {
            diffuser.clear();
        }
        self.ap_l1.clear();
        self.delay_l1.clear();
        self.ap_l2.clear();
        self.delay_l2.clear();
        self.ap_r1.clear();
        self.delay_r1.clear();
        self.ap_r2.clear();
        self.delay_r2.clear();
        self.damp_l = 0.0;
        self.damp_r = 0.0;
        self.node_l = 0.0;
        self.node_r = 0.0;
        self.mod_phase = 0.0;
    }

    /// Empties the tank and jumps every smoother to its target.
    pub fn snap(&mut self, params: &Params) {
        self.clear_tank();
        self.bypassed = false;

        self.set_targets(params);
        self.mix.snap_to_target();
        self.decay.snap_to_target();
        self.damping.snap_to_target();
        self.predelay_samples.snap_to_target();
        self.width.snap_to_target();
    }

    fn set_targets(&mut self, params: &Params) {
        self.mix.set_target(params.reverb_mix);
        self.decay.set_target(decay_for(params.reverb_size));
        self.damping.set_target(params.reverb_damping);
        let ceiling = (self.predelay.capacity() - 1) as f32;
        self.predelay_samples
            .set_target((params.reverb_predelay * self.sample_rate).clamp(0.0, ceiling));
        self.width.set_target(params.reverb_width);
    }

    /// Processes up to `BLOCK` samples in place, advancing smoothing once.
    pub fn process_chunk(&mut self, left: &mut [f32], right: &mut [f32], params: &Params) {
        // Fully off: skip the work entirely rather than multiplying by zero.
        // This is what makes a dry patch bit-identical, and it also means an
        // unused reverb costs nothing per sample.
        if params.reverb_mix == 0.0 && self.mix.value() == 0.0 {
            if !self.bypassed {
                self.clear_tank();
                self.bypassed = true;
            }
            return;
        }
        self.bypassed = false;

        self.set_targets(params);

        let mix = self.mix.next();
        let decay = self.decay.next();
        let damping = self.damping.next();
        let predelay = self.predelay_samples.next();
        let width = self.width.next();

        // Dattorro ties the second diffusion pair to the decay so a long tail
        // is also a smoother one.
        let tank_diffusion_2 = (decay + 0.15).clamp(0.25, 0.5);
        // 0.0 leaves the tail bright, 0.95 swallows the top end. Capped short
        // of 1.0, which would freeze the filter and silence the tank.
        let damp_keep = damping * 0.95;

        let count = left.len().min(right.len());
        for i in 0..count {
            let dry_left = left[i];
            let dry_right = right[i];

            // The tank is mono in; the stereo comes back out of the taps.
            self.predelay.write((dry_left + dry_right) * 0.5);
            let delayed = self.predelay.read_frac(predelay);

            self.bandwidth_state += INPUT_BANDWIDTH * (delayed - self.bandwidth_state);
            let mut diffused = self.bandwidth_state;
            for (diffuser, &coefficient) in
                self.diffusers.iter_mut().zip(DIFFUSER_COEFFICIENTS.iter())
            {
                diffused = diffuser.process(diffused, coefficient);
            }

            // Both halves read the *previous* sample's cross-feed, so the
            // order they are computed in cannot matter.
            let (feed_l, feed_r) = (self.node_l, self.node_r);

            self.mod_phase += self.mod_increment;
            if self.mod_phase >= 1.0 {
                self.mod_phase -= 1.0;
            }
            let angle = self.mod_phase * std::f32::consts::TAU;
            let excursion_l = angle.sin() * self.excursion;
            let excursion_r = (angle * MOD_HZ_RATIO).sin() * self.excursion;

            let mut half_l = self
                .ap_l1
                .process_modulated(diffused + feed_r, TANK_DIFFUSION_1, excursion_l);
            half_l = self.delay_l1.process(half_l);
            self.damp_l += (1.0 - damp_keep) * (half_l - self.damp_l);
            half_l = self.ap_l2.process(self.damp_l * decay, tank_diffusion_2);
            let out_l = self.delay_l2.process(half_l);

            let mut half_r = self
                .ap_r1
                .process_modulated(diffused + feed_l, TANK_DIFFUSION_1, excursion_r);
            half_r = self.delay_r1.process(half_r);
            self.damp_r += (1.0 - damp_keep) * (half_r - self.damp_r);
            half_r = self.ap_r2.process(self.damp_r * decay, tank_diffusion_2);
            let out_r = self.delay_r2.process(half_r);

            self.node_l = out_l * decay;
            self.node_r = out_r * decay;

            let tap_left = 0.6
                * (self.delay_r1.tap(self.taps.left[0]) + self.delay_r1.tap(self.taps.left[1])
                    - self.ap_r2.tap(self.taps.left[2])
                    + self.delay_r2.tap(self.taps.left[3])
                    - self.delay_l1.tap(self.taps.left[4])
                    - self.ap_l2.tap(self.taps.left[5])
                    - self.delay_l2.tap(self.taps.left[6]));

            let tap_right = 0.6
                * (self.delay_l1.tap(self.taps.right[0]) + self.delay_l1.tap(self.taps.right[1])
                    - self.ap_l2.tap(self.taps.right[2])
                    + self.delay_l2.tap(self.taps.right[3])
                    - self.delay_r1.tap(self.taps.right[4])
                    - self.ap_r2.tap(self.taps.right[5])
                    - self.delay_r2.tap(self.taps.right[6]));

            // Width collapses the two taps toward their average rather than
            // panning: at 0.0 both channels get the same signal, at 1.0 the
            // taps stand as they are.
            let centre = (tap_left + tap_right) * 0.5;
            let wet_left = centre + (tap_left - centre) * width;
            let wet_right = centre + (tap_right - centre) * width;

            left[i] = dry_left + (wet_left - dry_left) * mix;
            right[i] = dry_right + (wet_right - dry_right) * mix;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::Params;

    const SR: f32 = 48000.0;

    fn run(reverb: &mut PlateReverb, left: &mut [f32], right: &mut [f32], params: &Params) {
        for (l, r) in left
            .chunks_mut(crate::BLOCK)
            .zip(right.chunks_mut(crate::BLOCK))
        {
            reverb.process_chunk(l, r, params);
        }
    }

    fn wet(mix: f32, size: f32, damping: f32) -> Params {
        let mut params = Params::default();
        params.reverb_mix = mix;
        params.reverb_size = size;
        params.reverb_damping = damping;
        params.reverb_predelay = 0.0;
        params.reverb_width = 1.0;
        params
    }

    /// Root mean square of a window, which is how loud it actually is.
    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    /// Renders one impulse into `seconds` of tail.
    fn tail(params: &Params, seconds: f32) -> (Vec<f32>, Vec<f32>) {
        let mut reverb = PlateReverb::new(SR);
        reverb.snap(params);
        let count = (SR * seconds) as usize;
        let mut left = vec![0.0; count];
        let mut right = vec![0.0; count];
        left[0] = 1.0;
        right[0] = 1.0;
        run(&mut reverb, &mut left, &mut right, params);
        (left, right)
    }

    #[test]
    fn a_zero_mix_leaves_the_signal_bit_identical() {
        let mut reverb = PlateReverb::new(SR);
        let params = Params::default();
        reverb.snap(&params);

        let source: Vec<f32> = (0..512).map(|i| (i as f32 * 0.02).sin()).collect();
        let mut left = source.clone();
        let mut right = source.clone();
        run(&mut reverb, &mut left, &mut right, &params);

        assert_eq!(left, source);
        assert_eq!(right, source);
    }

    #[test]
    fn an_impulse_leaves_a_tail_behind_it() {
        let params = wet(1.0, 0.7, 0.3);
        let (left, _right) = tail(&params, 0.5);

        // 200 ms after the impulse there is still something there. A reverb
        // whose tail has already ended is just a filter.
        let late = &left[(SR * 0.2) as usize..(SR * 0.25) as usize];
        assert!(rms(late) > 1e-4, "tail died early: {}", rms(late));
    }

    #[test]
    fn the_tail_decays_rather_than_holding_or_growing() {
        let params = wet(1.0, 0.7, 0.3);
        let (left, _right) = tail(&params, 2.0);

        // 50 ms windows, each quieter than the one before it. Compared across
        // a gap rather than adjacent windows: the early tail is dense and
        // uneven, and demanding strict sample-by-sample decay would be testing
        // the noise rather than the envelope.
        let window = (SR * 0.05) as usize;
        let early = rms(&left[window * 2..window * 3]);
        let middle = rms(&left[window * 10..window * 11]);
        let late = rms(&left[window * 30..window * 31]);
        assert!(middle < early, "{middle} !< {early}");
        assert!(late < middle, "{late} !< {middle}");
    }

    #[test]
    fn a_bigger_size_makes_a_longer_tail() {
        let window = (SR * 0.05) as usize;
        let at = |size: f32| {
            let (left, _) = tail(&wet(1.0, size, 0.2), 2.0);
            rms(&left[window * 20..window * 21])
        };
        let small = at(0.15);
        let large = at(0.95);
        assert!(large > small * 2.0, "small {small}, large {large}");
    }

    #[test]
    fn a_mono_input_comes_out_stereo() {
        let params = wet(1.0, 0.7, 0.3);
        let (left, right) = tail(&params, 0.5);

        let differences = left
            .iter()
            .zip(right.iter())
            .filter(|(l, r)| (*l - *r).abs() > 1e-6)
            .count();
        // The whole point of the shared tank with offset taps: the same input
        // produces two genuinely different outputs.
        assert!(differences > left.len() / 4, "only {differences} differed");
    }

    #[test]
    fn zero_width_collapses_the_tail_to_mono() {
        let mut params = wet(1.0, 0.7, 0.3);
        params.reverb_width = 0.0;
        let (left, right) = tail(&params, 0.3);
        for (i, (l, r)) in left.iter().zip(right.iter()).enumerate() {
            assert!((l - r).abs() < 1e-5, "sample {i}: {l} vs {r}");
        }
    }

    #[test]
    fn predelay_holds_the_tail_back() {
        let mut params = wet(1.0, 0.7, 0.3);
        params.reverb_predelay = 0.1; // 4800 samples
        let (left, _right) = tail(&params, 0.5);

        let before = rms(&left[100..4000]);
        let after = rms(&left[5000..9000]);
        assert!(before < after * 0.05, "before {before}, after {after}");
    }

    #[test]
    fn the_worst_case_settings_stay_bounded() {
        // Maximum size, no damping, full-scale noise, ten seconds. If the tank
        // is going to run away, this is where it happens.
        let params = wet(1.0, 1.0, 0.0);
        let mut reverb = PlateReverb::new(SR);
        reverb.snap(&params);

        let mut seed: u32 = 0x1234_5678;
        let count = (SR * 10.0) as usize;
        let mut left = Vec::with_capacity(count);
        for _ in 0..count {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            left.push((seed >> 8) as f32 / 8_388_608.0 - 1.0);
        }
        let mut right = left.clone();
        run(&mut reverb, &mut left, &mut right, &params);

        for (i, sample) in left.iter().chain(right.iter()).enumerate() {
            assert!(sample.is_finite(), "sample {i} was {sample}");
            // Not tighter than this: this test exists to catch a runaway
            // tank, and runaway is exponential, reaching absurd magnitudes
            // within seconds. A correct tank at these settings (max decay,
            // zero damping, ten seconds of full-scale noise) is loud but
            // bounded, peaking around 7x. 16.0 separates "correct but loud"
            // from "diverging" while leaving headroom over that.
            assert!(sample.abs() <= 16.0, "sample {i} was {sample}");
        }
    }
}
