//! ADSR envelope generators.
//!
//! # Why the curves are exponential
//!
//! A linear envelope sounds wrong. Loudness perception is roughly logarithmic,
//! so a linear fade sits at "still fairly loud" for most of its length and then
//! drops off a cliff at the end. Analogue envelopes are RC curves — exponential
//! — and that is what the ear expects a note to do.
//!
//! The implementation is the standard one-pole-toward-an-overshooting-target
//! trick. Each stage aims at a target it will never reach and stops early when
//! it crosses the real threshold. Changing the target ratio changes the curve
//! shape from nearly linear to sharply exponential, at no extra cost.
//!
//! # Why times are in seconds and not samples
//!
//! Sample rates change. A patch stored in samples would drift in tempo between
//! 44.1 and 48 kHz machines.

/// Which segment of the envelope is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EnvStage {
    /// Finished and silent. The voice owning this envelope can be reclaimed.
    #[default]
    Idle,
    Attack,
    Decay,
    /// Holding at the sustain level, waiting for note-off.
    Sustain,
    Release,
}

/// Envelope times in seconds, sustain as a level.
#[derive(Debug, Clone, Copy)]
pub struct AdsrSettings {
    pub attack: f32,
    pub decay: f32,
    /// Level held while the key is down, `0.0..=1.0`.
    pub sustain: f32,
    pub release: f32,
}

impl Default for AdsrSettings {
    fn default() -> Self {
        Self {
            attack: 0.005,
            decay: 0.2,
            sustain: 0.7,
            release: 0.3,
        }
    }
}

/// Shapes the attack curve. Near 0 is sharply exponential; larger is closer to
/// linear. Attacks sound best fairly linear, so this is comparatively large.
const ATTACK_RATIO: f32 = 0.3;
/// Shapes decay and release. Small, so they curve steeply — like a real RC
/// discharge, and like every analogue envelope people have learned to expect.
const DECAY_RATIO: f32 = 0.0001;

/// A single ADSR envelope generator.
#[derive(Debug, Clone)]
pub struct Adsr {
    sample_rate: f32,
    stage: EnvStage,
    level: f32,

    settings: AdsrSettings,

    attack_coef: f32,
    attack_base: f32,
    decay_coef: f32,
    decay_base: f32,
    release_coef: f32,
    release_base: f32,
}

impl Adsr {
    pub fn new(sample_rate: f32) -> Self {
        let mut e = Self {
            sample_rate,
            stage: EnvStage::Idle,
            level: 0.0,
            settings: AdsrSettings::default(),
            attack_coef: 0.0,
            attack_base: 0.0,
            decay_coef: 0.0,
            decay_base: 0.0,
            release_coef: 0.0,
            release_base: 0.0,
        };
        e.recalculate();
        e
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.recalculate();
    }

    pub fn settings(&self) -> AdsrSettings {
        self.settings
    }

    /// Updates the envelope times. Safe to call every block: it only touches
    /// coefficients, never the running level, so a knob turn mid-note bends the
    /// curve rather than jumping it.
    pub fn set_settings(&mut self, settings: AdsrSettings) {
        self.settings = settings;
        self.recalculate();
    }

    fn recalculate(&mut self) {
        let sr = self.sample_rate;
        let s = self.settings;

        self.attack_coef = coef(s.attack, sr, ATTACK_RATIO);
        self.attack_base = (1.0 + ATTACK_RATIO) * (1.0 - self.attack_coef);

        let sustain = s.sustain.clamp(0.0, 1.0);
        self.decay_coef = coef(s.decay, sr, DECAY_RATIO);
        self.decay_base = (sustain - DECAY_RATIO) * (1.0 - self.decay_coef);

        self.release_coef = coef(s.release, sr, DECAY_RATIO);
        self.release_base = -DECAY_RATIO * (1.0 - self.release_coef);
    }

    /// Starts the envelope.
    ///
    /// `reset` controls retrigger behaviour. `true` snaps the level back to zero
    /// first, which gives every note an identical percussive attack. `false` is
    /// legato: the attack starts from wherever the envelope currently is, so
    /// overlapping notes glide instead of clicking. Mono patches usually want
    /// `false`, drums usually want `true`.
    #[inline]
    pub fn gate_on(&mut self, reset: bool) {
        if reset {
            self.level = 0.0;
        }
        self.stage = EnvStage::Attack;
    }

    /// Releases the envelope. Ignored if already idle.
    #[inline]
    pub fn gate_off(&mut self) {
        if self.stage != EnvStage::Idle {
            self.stage = EnvStage::Release;
        }
    }

    /// Cuts the envelope dead. Only for voice stealing, and even then the
    /// stealing code should fade out over a few milliseconds first — an
    /// instantaneous jump to zero is a click.
    #[inline]
    pub fn hard_reset(&mut self) {
        self.stage = EnvStage::Idle;
        self.level = 0.0;
    }

    #[inline]
    pub fn stage(&self) -> EnvStage {
        self.stage
    }

    #[inline]
    pub fn level(&self) -> f32 {
        self.level
    }

    /// True while the envelope is producing sound.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.stage != EnvStage::Idle
    }

    /// Advances one sample and returns the new level.
    // Not `Iterator::next`: an envelope is an infinite signal generator, and
    // `next()` is what it is called in every synth codebase. Wrapping it in
    // `Option` to satisfy the trait would cost a branch per sample for nothing.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        match self.stage {
            EnvStage::Idle => {}
            EnvStage::Attack => {
                self.level = self.attack_base + self.level * self.attack_coef;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = EnvStage::Decay;
                }
            }
            EnvStage::Decay => {
                self.level = self.decay_base + self.level * self.decay_coef;
                let sustain = self.settings.sustain.clamp(0.0, 1.0);
                if self.level <= sustain {
                    self.level = sustain;
                    // A sustain of zero means the note dies at the end of decay
                    // — a plucked or percussive patch. Go straight to idle so
                    // the voice can be reused instead of holding silence.
                    self.stage = if sustain <= 0.0 {
                        EnvStage::Idle
                    } else {
                        EnvStage::Sustain
                    };
                }
            }
            EnvStage::Sustain => {
                // Track the sustain knob if it moves while the key is held.
                self.level = self.settings.sustain.clamp(0.0, 1.0);
            }
            EnvStage::Release => {
                self.level = self.release_base + self.level * self.release_coef;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.stage = EnvStage::Idle;
                }
            }
        }
        self.level
    }
}

/// One-pole coefficient that walks from 0 to `1 + ratio` in `time` seconds.
#[inline]
fn coef(time: f32, sample_rate: f32, ratio: f32) -> f32 {
    let samples = time * sample_rate;
    if samples <= 0.0 {
        // Zero-length stage: jump straight to the target next sample.
        return 0.0;
    }
    (-((1.0 + ratio) / ratio).ln() / samples).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(a: f32, d: f32, s: f32, r: f32) -> Adsr {
        let mut e = Adsr::new(48000.0);
        e.set_settings(AdsrSettings {
            attack: a,
            decay: d,
            sustain: s,
            release: r,
        });
        e
    }

    #[test]
    fn runs_through_every_stage() {
        let mut e = env(0.01, 0.01, 0.5, 0.01);
        assert_eq!(e.stage(), EnvStage::Idle);
        e.gate_on(true);
        assert_eq!(e.stage(), EnvStage::Attack);

        // Attack should complete in roughly its stated time.
        let mut n = 0;
        while e.stage() == EnvStage::Attack && n < 48000 {
            e.next();
            n += 1;
        }
        assert!(n < 1000, "attack took {n} samples, expected ~480");

        while e.stage() == EnvStage::Decay {
            e.next();
        }
        assert_eq!(e.stage(), EnvStage::Sustain);
        assert!((e.level() - 0.5).abs() < 0.01);

        e.gate_off();
        let mut n = 0;
        while e.is_active() && n < 48000 {
            e.next();
            n += 1;
        }
        assert!(!e.is_active());
        assert_eq!(e.level(), 0.0);
    }

    #[test]
    fn level_never_leaves_unit_range() {
        let mut e = env(0.001, 0.5, 0.8, 1.0);
        e.gate_on(true);
        for i in 0..96000 {
            if i == 48000 {
                e.gate_off();
            }
            let l = e.next();
            assert!((0.0..=1.0).contains(&l), "level {l} at sample {i}");
        }
    }

    #[test]
    fn zero_sustain_goes_idle_after_decay() {
        let mut e = env(0.001, 0.05, 0.0, 1.0);
        e.gate_on(true);
        for _ in 0..48000 {
            e.next();
        }
        // Never released, but a zero sustain means the note is over.
        assert_eq!(e.stage(), EnvStage::Idle);
    }

    #[test]
    fn legato_retrigger_keeps_the_level() {
        let mut e = env(0.5, 0.1, 0.5, 0.1);
        e.gate_on(true);
        for _ in 0..12000 {
            e.next();
        }
        let before = e.level();
        assert!(before > 0.1);
        e.gate_on(false);
        assert_eq!(e.level(), before, "legato retrigger must not reset");
        e.gate_on(true);
        assert_eq!(e.level(), 0.0, "hard retrigger must reset");
    }

    #[test]
    fn attack_is_faster_when_set_faster() {
        let time_to_peak = |a: f32| {
            let mut e = env(a, 0.1, 0.5, 0.1);
            e.gate_on(true);
            let mut n = 0;
            while e.stage() == EnvStage::Attack && n < 480_000 {
                e.next();
                n += 1;
            }
            n
        };
        assert!(time_to_peak(0.001) < time_to_peak(0.1));
    }
}
