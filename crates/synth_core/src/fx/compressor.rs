//! A feed-forward peak compressor with an optional external detector.

use crate::params::CompressorParams;

/// The knee width in dB. Fixed rather than exposed: a knee control is one more
/// knob for something almost nobody adjusts away from "a few dB".
const KNEE_DB: f32 = 6.0;

/// Below this the detector is treated as silence, so `log10` never sees zero.
const FLOOR: f32 = 1.0e-9;

/// Above this the detector is treated as already-diverged input. Far above
/// any real signal (+180 dB) so it never engages on audio -- only on
/// infinities, keeping `over` and `target` finite so `gr_db` can never be
/// poisoned with a NaN that would otherwise persist across every following
/// sample and call.
const CEIL: f32 = 1.0e9;

/// A feed-forward peak compressor.
///
/// State is one float plus the sample rate -- no delay lines, no lookahead, no
/// allocation. Two instances cost almost nothing next to one reverb.
#[derive(Debug, Clone)]
pub struct Compressor {
    /// Current gain reduction in dB, positive meaning "turned down by".
    gr_db: f32,
    sample_rate: f32,
}

impl Compressor {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            gr_db: 0.0,
            sample_rate,
        }
    }

    /// Changing the sample rate invalidates the envelope coefficients, so the
    /// envelope is cleared rather than left holding a reduction computed for a
    /// different rate.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.gr_db = 0.0;
    }

    /// The reduction currently being applied, in dB, for a GR meter.
    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db
    }

    /// Compresses `left` and `right` in place.
    ///
    /// `detector` supplies the signal the gain is computed from. `None` means
    /// detect on the signal being compressed. When `Some` and shorter than
    /// `left`, it bounds how many samples are processed rather than panicking
    /// -- the tail of `left`/`right` beyond the detector's length is left
    /// untouched.
    pub fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        detector: Option<&[f32]>,
        p: &CompressorParams,
    ) {
        if !p.on {
            // Bypass is bit-exact: the buffers are not touched at all.
            self.gr_db = 0.0;
            return;
        }

        // One exp per parameter per call, not per sample. The parameters are
        // constant for the length of the call.
        let attack = coefficient(p.attack_ms, self.sample_rate);
        let release = coefficient(p.release_ms, self.sample_rate);
        let slope = 1.0 - 1.0 / p.ratio;
        let half_knee = KNEE_DB * 0.5;
        let makeup = p.makeup_db;

        // The detector's length is folded in with the others: a short detector
        // must shorten the block, not panic on the audio thread.
        let count = left
            .len()
            .min(right.len())
            .min(detector.map_or(usize::MAX, <[f32]>::len));
        for i in 0..count {
            let level = match detector {
                Some(d) => d[i].abs(),
                None => left[i].abs().max(right[i].abs()),
            };
            // `max` then `min`, not `clamp`: `f32::clamp` returns NaN when
            // `self` is NaN, which would flow straight through `log10` into
            // `over`/`target` and poison `gr_db` forever. `max` and `min`
            // both discard a NaN `self` in favour of the other operand, so a
            // NaN sample degrades to `FLOOR` (silence) instead. The ceiling
            // still keeps an already-diverged (+inf) input from turning
            // `target` into a NaN the same way. Do not let clippy "simplify"
            // this back to `.clamp(FLOOR, CEIL)` -- that reintroduces the
            // NaN-poisoning bug this line exists to close.
            #[allow(clippy::manual_clamp)]
            let level_db = 20.0 * level.max(FLOOR).min(CEIL).log10();
            let over = level_db - p.threshold_db;

            let target = if over <= -half_knee {
                0.0
            } else if over >= half_knee {
                over * slope
            } else {
                // Quadratic interpolation across the knee: zero slope at the
                // lower corner, full slope at the upper one.
                let x = over + half_knee;
                slope * x * x / (2.0 * KNEE_DB)
            };

            let coef = if target > self.gr_db { attack } else { release };
            self.gr_db = target + (self.gr_db - target) * coef;

            let gain = db_to_gain(makeup - self.gr_db);
            left[i] *= gain;
            right[i] *= gain;
        }
    }
}

/// The one-pole coefficient that reaches 1 - 1/e of its travel in `time_ms`.
fn coefficient(time_ms: f32, sample_rate: f32) -> f32 {
    let samples = (time_ms * 0.001 * sample_rate).max(1.0);
    (-1.0 / samples).exp()
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::CompressorParams;

    const SR: f32 = 48_000.0;

    /// Turns a linear gain into dB, for asserting on what the compressor did.
    fn db(gain: f32) -> f32 {
        20.0 * gain.log10()
    }

    #[test]
    fn a_bypassed_compressor_is_a_bit_exact_pass_through() {
        let mut comp = Compressor::new(SR);
        let p = CompressorParams::default(); // on: false
        let mut l = [0.9, -0.7, 0.3, 0.0];
        let mut r = [0.1, 0.2, -0.9, 0.5];
        let want_l = l;
        let want_r = r;

        comp.process(&mut l, &mut r, None, &p);

        assert_eq!(l, want_l);
        assert_eq!(r, want_r);
        assert_eq!(comp.gain_reduction_db(), 0.0);
    }

    #[test]
    fn a_signal_below_threshold_passes_at_unity() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0; // 0.2512 linear; -6 dB knee floor is -15 dB
        // -40 dB, far below the knee.
        let mut l = [0.01; 512];
        let mut r = [0.01; 512];

        comp.process(&mut l, &mut r, None, &p);

        assert!((l[511] - 0.01).abs() < 1e-9, "got {}", l[511]);
        assert!(comp.gain_reduction_db() < 1e-6);
    }

    #[test]
    fn a_steady_signal_settles_at_the_gain_reduction_the_ratio_implies() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;
        // One second is a thousand attack time constants: fully settled.
        let mut l = [0.5; 48_000];
        let mut r = [0.5; 48_000];

        comp.process(&mut l, &mut r, None, &p);

        // -6.0206 dB is 5.9794 over the threshold; a 4:1 ratio removes 3/4.
        let want_gr = 5.9794 * 0.75;
        assert!(
            (comp.gain_reduction_db() - want_gr).abs() < 0.01,
            "gr {} want {}",
            comp.gain_reduction_db(),
            want_gr
        );
        // 0.5 turned down by 4.4845 dB is 0.2982.
        assert!((l[47_999] - 0.2982).abs() < 1e-3, "got {}", l[47_999]);
    }

    #[test]
    fn the_attack_reaches_63_percent_of_its_travel_in_one_time_constant() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 10.0;

        // 10 ms at 48 kHz is exactly 480 samples, which is the time constant.
        let mut l = [0.5; 480];
        let mut r = [0.5; 480];
        comp.process(&mut l, &mut r, None, &p);

        let settled = 5.9794 * 0.75;
        let want = settled * (1.0 - (-1.0f32).exp()); // 63.2% of the travel
        assert!(
            (comp.gain_reduction_db() - want).abs() < 0.01,
            "gr {} want {}",
            comp.gain_reduction_db(),
            want
        );
    }

    #[test]
    fn the_release_gives_the_gain_back() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;
        p.release_ms = 10.0;

        let mut l = [0.5; 48_000];
        let mut r = [0.5; 48_000];
        comp.process(&mut l, &mut r, None, &p);
        let compressed = comp.gain_reduction_db();
        assert!(compressed > 4.0);

        // Silence for 480 samples: one release time constant of decay.
        let mut l = [0.0; 480];
        let mut r = [0.0; 480];
        comp.process(&mut l, &mut r, None, &p);

        let want = compressed * (-1.0f32).exp(); // 36.8% of the way left
        assert!(
            (comp.gain_reduction_db() - want).abs() < 0.01,
            "gr {} want {}",
            comp.gain_reduction_db(),
            want
        );
    }

    #[test]
    fn gain_reduction_telemetry_matches_the_gain_actually_applied() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;
        p.makeup_db = 6.0;

        let mut l = [0.5; 48_000];
        let mut r = [0.5; 48_000];
        comp.process(&mut l, &mut r, None, &p);

        let applied = db(l[47_999] / 0.5);
        let reported = p.makeup_db - comp.gain_reduction_db();
        assert!((applied - reported).abs() < 1e-3, "{applied} vs {reported}");
    }

    #[test]
    fn an_external_detector_drives_the_gain_instead_of_the_signal() {
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -40.0;
        p.ratio = 8.0;
        p.attack_ms = 1.0;

        // A loud signal with a silent detector is left alone.
        let mut comp = Compressor::new(SR);
        let mut l = [0.5; 4_800];
        let mut r = [0.5; 4_800];
        let silent = [0.0; 4_800];
        comp.process(&mut l, &mut r, Some(&silent), &p);
        assert!(comp.gain_reduction_db() < 1e-6);
        assert_eq!(l[4_799], 0.5);

        // A quiet signal with a loud detector is ducked.
        let mut comp = Compressor::new(SR);
        let mut l = [0.01; 4_800];
        let mut r = [0.01; 4_800];
        let loud = [1.0; 4_800];
        comp.process(&mut l, &mut r, Some(&loud), &p);
        assert!(comp.gain_reduction_db() > 30.0, "{}", comp.gain_reduction_db());
        assert!(l[4_799] < 0.001, "got {}", l[4_799]);
    }

    #[test]
    fn a_detector_shorter_than_the_signal_does_not_panic() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;

        let mut l = [0.5; 10];
        let mut r = [0.5; 10];
        let detector = [1.0; 5];

        comp.process(&mut l, &mut r, Some(&detector), &p);

        // The first five samples were compressed against the loud detector...
        assert!(l[4] < 0.5, "got {}", l[4]);
        // ...but the tail beyond the detector's length is left exactly as it
        // was, not panicked on and not touched.
        assert_eq!(&l[5..], &[0.5; 5]);
        assert_eq!(&r[5..], &[0.5; 5]);
    }

    #[test]
    fn an_infinite_sample_does_not_poison_the_envelope() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 1.0; // slope is 0.0 -- the case that used to give inf * 0.0.
        p.attack_ms = 1.0;

        let mut l = [f32::INFINITY];
        let mut r = [f32::INFINITY];
        comp.process(&mut l, &mut r, None, &p);
        assert!(
            comp.gain_reduction_db().is_finite(),
            "gr {}",
            comp.gain_reduction_db()
        );

        // A following block of ordinary audio must still come out finite --
        // proving the earlier infinity did not leave `gr_db` as a NaN that
        // poisons every sample from here on.
        let mut l = [0.5; 480];
        let mut r = [0.5; 480];
        comp.process(&mut l, &mut r, None, &p);
        assert!(l.iter().all(|s| s.is_finite()));
        assert!(r.iter().all(|s| s.is_finite()));
        assert!(comp.gain_reduction_db().is_finite());
    }

    #[test]
    fn a_nan_sample_does_not_poison_the_envelope() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;

        let mut l = [f32::NAN];
        let mut r = [f32::NAN];
        comp.process(&mut l, &mut r, None, &p);
        assert!(
            comp.gain_reduction_db().is_finite(),
            "gr {}",
            comp.gain_reduction_db()
        );

        // A following block of ordinary audio must still come out finite --
        // proving the earlier NaN did not leave `gr_db` as a NaN that
        // poisons every sample from here on.
        let mut l = [0.5; 480];
        let mut r = [0.5; 480];
        comp.process(&mut l, &mut r, None, &p);
        assert!(l.iter().all(|s| s.is_finite()));
        assert!(r.iter().all(|s| s.is_finite()));
        assert!(comp.gain_reduction_db().is_finite());
    }
}
