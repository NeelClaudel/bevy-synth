//! The bridge between the control thread and the audio thread.
//!
//! # The problem
//!
//! Bevy's schedule runs at ~60 Hz with whatever jitter a frame spike brings.
//! The audio callback runs every few milliseconds and must *never* be late: if
//! it is, the driver plays whatever was left in the buffer and the user hears a
//! click. So the audio thread can never take a lock the game thread might hold,
//! and can never allocate.
//!
//! # The solution
//!
//! Every parameter is an atomic. The control side stores; the audio side loads
//! once per block into a plain [`Params`] struct and works from that. No locks,
//! no allocation, no blocking, and a torn read is impossible because each
//! parameter is a single word.
//!
//! Reads use `Relaxed` ordering throughout. There is nothing to synchronise —
//! we do not care whether the cutoff change lands one block before or after the
//! resonance change, only that neither is torn. Paying for `Acquire`/`Release`
//! on every knob, every block, would be waste.
//!
//! # Why smoothing is not optional
//!
//! Jumping a gain or cutoff between blocks is a step discontinuity, and a step
//! is broadband click. Drag a knob and you get one per block: the "zipper
//! noise" that makes hand-rolled synths sound broken. [`Smoothed`] one-poles
//! every continuous parameter toward its target so a knob sweep is a sweep.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::drums::{pack_column, unpack_column, Column, DrumPattern, PAD_COUNT};
use crate::env::AdsrSettings;
use crate::filter::{Slope, SvfMode};
use crate::fx::NoteDivision;
use crate::lfo::{LfoTarget, LfoWave};
use crate::osc::Waveform;
use crate::scale::Scale;

const REL: Ordering = Ordering::Relaxed;

/// Clamps to `0.0..=1.0`, mapping NaN to 0.0.
///
/// `f32::clamp` propagates a NaN input, so a bare `.clamp(0.0, 1.0)` is not
/// enough for a value that arrived over an atomic from another thread.
fn clamp01(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

/// Replaces a non-finite value with a fallback, leaving finite ones alone.
fn sane(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

/// An `f32` stored atomically, via its bit pattern.
///
/// Rust has no `AtomicF32`. Transmuting through `u32` is the standard workaround
/// and is exactly what every audio library does; `to_bits`/`from_bits` are
/// lossless and total, so no value is lost, including NaN.
#[derive(Debug)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub const fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    #[inline]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(REL))
    }
    #[inline]
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), REL);
    }
}

/// A `bool` stored atomically as a `u32`, for uniformity with the rest.
#[derive(Debug)]
pub struct AtomicBool32(AtomicU32);

impl AtomicBool32 {
    pub const fn new(v: bool) -> Self {
        Self(AtomicU32::new(v as u32))
    }
    #[inline]
    pub fn get(&self) -> bool {
        self.0.load(REL) != 0
    }
    #[inline]
    pub fn set(&self, v: bool) {
        self.0.store(v as u32, REL);
    }
}

/// A `u32` enum discriminant stored atomically.
#[derive(Debug)]
pub struct AtomicEnum(AtomicU32);

impl AtomicEnum {
    pub const fn new(v: u32) -> Self {
        Self(AtomicU32::new(v))
    }
    #[inline]
    pub fn get(&self) -> u32 {
        self.0.load(REL)
    }
    #[inline]
    pub fn set(&self, v: u32) {
        self.0.store(v, REL);
    }
}

/// How voices are allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum VoiceMode {
    /// One voice. Later notes take over the single voice; releasing one while
    /// another is held falls back to it, which is what makes basslines work.
    Mono = 0,
    /// Many voices, one per held note, with stealing when they run out.
    #[default]
    Poly = 1,
}

impl VoiceMode {
    pub fn from_u32(v: u32) -> Self {
        if v == 0 {
            VoiceMode::Mono
        } else {
            VoiceMode::Poly
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            VoiceMode::Mono => "Mono",
            VoiceMode::Poly => "Poly",
        }
    }
}

/// Where the sequencer gets its tempo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum ClockSource {
    /// Count samples against a BPM set here. Standalone.
    #[default]
    Internal = 0,
    /// Advance on incoming MIDI clock ticks, 24 per quarter note. Locks to a
    /// DAW or a drum machine.
    ExternalMidi = 1,
}

impl ClockSource {
    pub fn from_u32(v: u32) -> Self {
        if v == 1 {
            ClockSource::ExternalMidi
        } else {
            ClockSource::Internal
        }
    }
}

/// Where a compressor's detector listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum SidechainSource {
    /// Detect on the signal being compressed.
    #[default]
    Off = 0,
    /// Detect on the mono sum of the dry drum bus.
    DrumBus = 1,
    /// Detect on pad 0 alone.
    Kick = 2,
}

impl SidechainSource {
    pub const ALL: [SidechainSource; 3] = [
        SidechainSource::Off,
        SidechainSource::DrumBus,
        SidechainSource::Kick,
    ];
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => SidechainSource::DrumBus,
            2 => SidechainSource::Kick,
            _ => SidechainSource::Off,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            SidechainSource::Off => "Off",
            SidechainSource::DrumBus => "Drums",
            SidechainSource::Kick => "Kick",
        }
    }
}

/// One compressor's settings.
///
/// Grouped rather than flattened into [`Params`] because there are two
/// instances -- the synth bus insert and the master glue -- and grouping makes
/// them identical by construction instead of two hand-copied blocks of seven
/// fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorParams {
    pub on: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    pub sidechain: SidechainSource,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            on: false,
            threshold_db: -12.0,
            ratio: 4.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: 0.0,
            sidechain: SidechainSource::Off,
        }
    }
}

/// The atomic mirror of [`CompressorParams`].
#[derive(Debug)]
pub struct SharedCompressor {
    pub on: AtomicBool32,
    pub threshold_db: AtomicF32,
    pub ratio: AtomicF32,
    pub attack_ms: AtomicF32,
    pub release_ms: AtomicF32,
    pub makeup_db: AtomicF32,
    pub sidechain: AtomicEnum,
}

impl SharedCompressor {
    fn new(p: &CompressorParams) -> Self {
        Self {
            on: AtomicBool32::new(p.on),
            threshold_db: AtomicF32::new(p.threshold_db),
            ratio: AtomicF32::new(p.ratio),
            attack_ms: AtomicF32::new(p.attack_ms),
            release_ms: AtomicF32::new(p.release_ms),
            makeup_db: AtomicF32::new(p.makeup_db),
            sidechain: AtomicEnum::new(p.sidechain as u32),
        }
    }

    fn snapshot(&self) -> CompressorParams {
        CompressorParams {
            on: self.on.get(),
            threshold_db: sane(self.threshold_db.get(), -12.0).clamp(-60.0, 0.0),
            ratio: sane(self.ratio.get(), 4.0).clamp(1.0, 20.0),
            attack_ms: sane(self.attack_ms.get(), 10.0).clamp(0.1, 100.0),
            release_ms: sane(self.release_ms.get(), 100.0).clamp(5.0, 1000.0),
            makeup_db: sane(self.makeup_db.get(), 0.0).clamp(0.0, 24.0),
            sidechain: SidechainSource::from_u32(self.sidechain.get()),
        }
    }

    fn apply(&self, p: &CompressorParams) {
        self.on.set(p.on);
        self.threshold_db.set(p.threshold_db);
        self.ratio.set(p.ratio);
        self.attack_ms.set(p.attack_ms);
        self.release_ms.set(p.release_ms);
        self.makeup_db.set(p.makeup_db);
        self.sidechain.set(p.sidechain as u32);
    }
}

/// A one-pole smoother for a continuous parameter.
///
/// Runs at control rate (once per [`crate::BLOCK`]), not per sample: at 48 kHz
/// with a 32-sample block that is 1.5 kHz, far above any knob movement, and 32x
/// cheaper than smoothing every sample.
#[derive(Debug, Clone)]
pub struct Smoothed {
    value: f32,
    target: f32,
    coef: f32,
}

impl Smoothed {
    /// `time_ms` is the time constant: how long to cover ~63% of the distance.
    /// 5-20 ms suits knobs. Too fast and it clicks; too slow and the synth
    /// feels rubbery under the fingers.
    pub fn new(initial: f32, time_ms: f32, control_rate: f32) -> Self {
        let mut s = Self {
            value: initial,
            target: initial,
            coef: 0.0,
        };
        s.set_time(time_ms, control_rate);
        s
    }

    pub fn set_time(&mut self, time_ms: f32, control_rate: f32) {
        let samples = (time_ms / 1000.0) * control_rate;
        self.coef = if samples <= 1.0 {
            0.0
        } else {
            (-1.0 / samples).exp()
        };
    }

    #[inline]
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Jumps straight to a value with no ramp. For patch loads and voice
    /// starts, where there is no previous value worth gliding from.
    #[inline]
    pub fn snap(&mut self, value: f32) {
        self.value = value;
        self.target = value;
    }

    /// Jumps straight to wherever `set_target` last pointed, with no ramp.
    /// For the effects, whose targets are computed from the patch rather than
    /// known to the caller.
    #[inline]
    pub fn snap_to_target(&mut self) {
        self.value = self.target;
    }

    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Advances one control-rate step.
    // Not `Iterator::next`, for the same reason as `Adsr::next`: this is an
    // endless signal, not a sequence that can run out.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        self.value = self.target + (self.value - self.target) * self.coef;
        // Settle exactly, so a parameter that should be zero really is.
        if (self.value - self.target).abs() < 1e-7 {
            self.value = self.target;
        }
        self.value
    }
}

/// Every parameter, as the audio thread sees it after one block-boundary read.
///
/// Plain fields, `Copy`, no atomics: once the audio thread has this it can
/// touch it as often as it likes with no synchronisation cost.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    // --- Oscillator 1 ---
    pub osc1_wave: Waveform,
    pub osc1_level: f32,
    /// Coarse tuning, in semitones.
    pub osc1_semitones: f32,
    /// Fine tuning, in cents. Detuning two oscillators a few cents apart is
    /// what gives a synth its width — they beat against each other slowly.
    pub osc1_detune: f32,

    // --- Oscillator 2 ---
    pub osc2_wave: Waveform,
    pub osc2_level: f32,
    pub osc2_semitones: f32,
    pub osc2_detune: f32,

    /// Duty cycle for [`Waveform::Pulse`], shared by both oscillators.
    pub pulse_width: f32,
    /// A square one octave below osc 1. Cheap weight for basses.
    pub sub_level: f32,
    pub noise_level: f32,

    // --- Filter ---
    pub filter_mode: SvfMode,
    pub filter_slope: Slope,
    pub cutoff: f32,
    pub resonance: f32,
    /// How far the filter envelope moves the cutoff, in octaves. Negative
    /// values close the filter as the envelope opens, which is unusual and
    /// occasionally exactly right.
    pub filter_env_amount: f32,
    /// How much the played note raises the cutoff. At 1.0 the filter tracks the
    /// keyboard exactly, so every note has the same timbre; at 0.0 high notes
    /// are duller than low ones. Around 0.3-0.5 usually sounds most natural.
    pub filter_key_track: f32,
    /// How much velocity opens the filter. Playing harder sounding brighter is
    /// most of what makes a synth feel responsive.
    pub filter_velocity: f32,

    // --- Envelopes ---
    pub amp_env: AdsrSettings,
    pub filter_env: AdsrSettings,

    // --- LFO ---
    pub lfo_wave: LfoWave,
    pub lfo_rate: f32,
    pub lfo_depth: f32,
    pub lfo_target: LfoTarget,
    /// Restart the LFO on every note instead of letting it free-run.
    pub lfo_retrigger: bool,

    // --- Voices ---
    pub voice_mode: VoiceMode,
    pub max_voices: usize,
    /// Portamento time in seconds: how long a new note takes to slide from the
    /// previous pitch. Zero disables it.
    pub glide: f32,
    /// In mono mode, whether overlapping notes retrigger the envelopes.
    pub legato: bool,
    /// Pitch bend in semitones, from the wheel.
    pub pitch_bend: f32,
    /// Mod wheel, 0-1. Scales LFO depth, so a patch can be still until asked.
    pub mod_wheel: f32,

    // --- Output ---
    pub master_gain: f32,
    /// Level of the melody bus alone, applied after the soft clipper so it
    /// sets loudness without changing how hard `drive` is saturating.
    pub synth_gain: f32,
    /// How much of the compressed synth bus reaches the return bus.
    pub synth_send: f32,
    pub comp_synth: CompressorParams,
    pub comp_master: CompressorParams,
    /// Level of the drum bus alone, applied where the rack is summed into the
    /// mix. Independent of `drum_level`, which trims the rack internally.
    pub drum_gain: f32,
    /// Pre-limiter drive. Above 1.0 pushes the output into the soft clipper for
    /// saturation rather than clean gain.
    pub drive: f32,

    // --- Delay ---
    /// Dry/wet blend for the delay. 0.0 bypasses it entirely.
    pub delay_mix: f32,
    /// When true, `delay_division` sets the time and `delay_time` is ignored.
    pub delay_sync: bool,
    /// Free-running delay time in seconds.
    pub delay_time: f32,
    /// Delay time as a musical division, used when `delay_sync` is set.
    pub delay_division: NoteDivision,
    /// How much of each repeat feeds the next. Below 1.0 always.
    pub delay_feedback: f32,
    /// High-frequency loss per repeat. 0.0 is a bright digital delay, 1.0 a
    /// dark tape one.
    pub delay_damping: f32,
    /// Cross the two lines so repeats alternate between the speakers.
    pub delay_ping_pong: bool,

    // --- Reverb ---
    /// Dry/wet blend for the reverb. 0.0 bypasses it entirely.
    pub reverb_mix: f32,
    /// Tail length, as a fraction of the tank's decay range.
    pub reverb_size: f32,
    /// High-frequency absorption inside the tank.
    pub reverb_damping: f32,
    /// Gap before the tail starts, in seconds. This is what makes a space
    /// sound large rather than just long.
    pub reverb_predelay: f32,
    /// 0.0 collapses the tail to mono, 1.0 is the full tap spread.
    pub reverb_width: f32,

    // --- Sequencer ---
    pub seq_playing: bool,
    pub clock_source: ClockSource,
    pub tempo: f32,
    /// Sequencer steps per quarter note. 4 = sixteenth notes.
    pub steps_per_beat: f32,
    /// Pattern length in steps.
    pub seq_length: usize,
    /// Note length as a fraction of one step. Below ~0.9 gives separation
    /// between notes; above 1.0 overlaps them into a legato line.
    pub seq_gate: f32,
    /// Delays every other step, as a fraction of a step. 0.0 is straight,
    /// 0.33 is a hard shuffle.
    pub seq_swing: f32,

    // --- Drums ---
    pub drum_enabled: bool,
    /// Gates the melodic track. Separate from `seq_playing`, which stops the
    /// clock: this mutes one track while the other keeps running.
    pub melody_enabled: bool,
    /// Grid length in steps, independent of `seq_length`. A two-bar drum loop
    /// under a four-bar melody is a groove, not a mistake.
    pub drum_length: usize,
    pub drum_level: f32,
    /// Sends the drum bus through delay and reverb.
    pub drum_to_fx: bool,
    /// How much of the drum send pair reaches the return bus.
    pub drum_send: f32,
    /// Per-pad position in the stereo field, -1.0 hard left to 1.0 hard right.
    pub pad_pan: [f32; PAD_COUNT],
    /// Per-pad contribution to the drum send pair, before `drum_send`.
    pub pad_send: [f32; PAD_COUNT],
    /// Per-pad mix level.
    pub pad_level: [f32; PAD_COUNT],
    /// Per-pad tuning in semitones from the pad's base frequency.
    pub pad_tune: [f32; PAD_COUNT],
    /// Per-pad multiplier on the pad's natural decay. A multiplier rather than
    /// absolute seconds because the pads' decays differ by an order of
    /// magnitude and one absolute range would be unusable at both ends.
    pub pad_decay: [f32; PAD_COUNT],
    /// Silences a pad without disturbing its level.
    pub pad_mute: [bool; PAD_COUNT],

    // --- Generative ---
    pub gen_enabled: bool,
    /// Root note as a pitch class, 0 = C.
    pub gen_root: u8,
    pub gen_scale: Scale,
    /// Lowest octave the generator will write in.
    pub gen_octave: i32,
    /// How many octaves above that it may reach.
    pub gen_range: u32,
    /// Probability that a step has a note rather than a rest.
    pub gen_density: f32,
    /// How far the melody may leap, in scale degrees. Low values make it walk.
    pub gen_max_jump: f32,
    /// How strongly downbeats are pulled toward chord tones (root, third,
    /// fifth). This is most of what separates "random notes in a scale" from
    /// something that sounds composed.
    pub gen_chord_bias: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            osc1_wave: Waveform::Saw,
            osc1_level: 0.8,
            osc1_semitones: 0.0,
            osc1_detune: 0.0,

            osc2_wave: Waveform::Saw,
            osc2_level: 0.5,
            osc2_semitones: 0.0,
            // Seven cents sharp: slow enough to sound like two oscillators
            // rather than one out of tune.
            osc2_detune: 7.0,

            pulse_width: 0.5,
            sub_level: 0.0,
            noise_level: 0.0,

            filter_mode: SvfMode::Lowpass,
            filter_slope: Slope::Db24,
            cutoff: 2000.0,
            resonance: 0.25,
            filter_env_amount: 2.0,
            filter_key_track: 0.35,
            filter_velocity: 0.4,

            amp_env: AdsrSettings {
                attack: 0.005,
                decay: 0.25,
                sustain: 0.7,
                release: 0.25,
            },
            filter_env: AdsrSettings {
                attack: 0.002,
                decay: 0.35,
                sustain: 0.25,
                release: 0.3,
            },

            lfo_wave: LfoWave::Sine,
            lfo_rate: 5.0,
            lfo_depth: 0.0,
            lfo_target: LfoTarget::Cutoff,
            lfo_retrigger: false,

            voice_mode: VoiceMode::Poly,
            max_voices: 16,
            glide: 0.0,
            legato: true,
            pitch_bend: 0.0,
            mod_wheel: 0.0,

            master_gain: 0.5,
            // Unity: the two bus gains are a mixer in front of the existing
            // sound, not a change to it, so a patch that never touches them
            // renders exactly as it did before they existed.
            synth_gain: 1.0,
            synth_send: 1.0,
            comp_synth: CompressorParams::default(),
            comp_master: CompressorParams::default(),
            drum_gain: 1.0,
            drive: 1.0,

            delay_mix: 0.0,
            delay_sync: false,
            delay_time: 0.375,
            delay_division: NoteDivision::Eighth,
            delay_feedback: 0.35,
            delay_damping: 0.3,
            delay_ping_pong: false,

            reverb_mix: 0.0,
            reverb_size: 0.5,
            reverb_damping: 0.5,
            reverb_predelay: 0.02,
            reverb_width: 1.0,

            seq_playing: false,
            clock_source: ClockSource::Internal,
            tempo: 120.0,
            steps_per_beat: 4.0,
            seq_length: 16,
            seq_gate: 0.6,
            seq_swing: 0.0,

            drum_enabled: false,
            melody_enabled: true,
            drum_length: 16,
            drum_level: 0.8,
            drum_to_fx: false,
            drum_send: 0.0,
            pad_pan: [0.0; PAD_COUNT],
            pad_send: [1.0; PAD_COUNT],
            pad_level: [0.8; PAD_COUNT],
            pad_tune: [0.0; PAD_COUNT],
            pad_decay: [1.0; PAD_COUNT],
            pad_mute: [false; PAD_COUNT],

            gen_enabled: true,
            gen_root: 0,
            gen_scale: Scale::MinorPentatonic,
            gen_octave: 3,
            gen_range: 2,
            gen_density: 0.75,
            gen_max_jump: 3.0,
            gen_chord_bias: 0.6,
        }
    }
}

/// Packs a step into one word.
///
/// Bit 31 active, bit 30 accent, bits 8-15 note, bits 0-7 velocity quantised
/// to 8 bits. Velocity to 1/255 is finer than any display or ear needs, and
/// fitting a whole step into a single word is what makes it tear-free: the
/// reader either sees the old step or the new one, never half of each.
fn pack_step(step: &crate::sequencer::Step) -> u32 {
    ((step.active as u32) << 31)
        | ((step.accent as u32) << 30)
        | ((step.note as u32) << 8)
        | ((step.velocity.clamp(0.0, 1.0) * 255.0) as u32)
}

/// Unpacks a step written by [`pack_step`].
fn unpack_step(packed: u32) -> crate::sequencer::Step {
    crate::sequencer::Step {
        active: packed & (1 << 31) != 0,
        accent: packed & (1 << 30) != 0,
        note: ((packed >> 8) & 0xFF) as u8,
        velocity: (packed & 0xFF) as f32 / 255.0,
    }
}

/// The shared, atomically-updatable parameter block.
///
/// Wrap in an `Arc`, hand one clone to the audio thread and keep the other on
/// the control side. Every setter is `&self`, so no locking and no `&mut`
/// plumbing through the ECS.
#[derive(Debug)]
pub struct SharedParams {
    pub osc1_wave: AtomicEnum,
    pub osc1_level: AtomicF32,
    pub osc1_semitones: AtomicF32,
    pub osc1_detune: AtomicF32,

    pub osc2_wave: AtomicEnum,
    pub osc2_level: AtomicF32,
    pub osc2_semitones: AtomicF32,
    pub osc2_detune: AtomicF32,

    pub pulse_width: AtomicF32,
    pub sub_level: AtomicF32,
    pub noise_level: AtomicF32,

    pub filter_mode: AtomicEnum,
    pub filter_slope: AtomicEnum,
    pub cutoff: AtomicF32,
    pub resonance: AtomicF32,
    pub filter_env_amount: AtomicF32,
    pub filter_key_track: AtomicF32,
    pub filter_velocity: AtomicF32,

    pub amp_attack: AtomicF32,
    pub amp_decay: AtomicF32,
    pub amp_sustain: AtomicF32,
    pub amp_release: AtomicF32,

    pub filter_attack: AtomicF32,
    pub filter_decay: AtomicF32,
    pub filter_sustain: AtomicF32,
    pub filter_release: AtomicF32,

    pub lfo_wave: AtomicEnum,
    pub lfo_rate: AtomicF32,
    pub lfo_depth: AtomicF32,
    pub lfo_target: AtomicEnum,
    pub lfo_retrigger: AtomicBool32,

    pub voice_mode: AtomicEnum,
    pub max_voices: AtomicEnum,
    pub glide: AtomicF32,
    pub legato: AtomicBool32,
    pub pitch_bend: AtomicF32,
    pub mod_wheel: AtomicF32,

    pub master_gain: AtomicF32,
    pub synth_gain: AtomicF32,
    pub synth_send: AtomicF32,
    pub comp_synth: SharedCompressor,
    pub comp_master: SharedCompressor,
    pub drum_gain: AtomicF32,
    pub drive: AtomicF32,

    pub delay_mix: AtomicF32,
    pub delay_sync: AtomicBool32,
    pub delay_time: AtomicF32,
    pub delay_division: AtomicEnum,
    pub delay_feedback: AtomicF32,
    pub delay_damping: AtomicF32,
    pub delay_ping_pong: AtomicBool32,

    pub reverb_mix: AtomicF32,
    pub reverb_size: AtomicF32,
    pub reverb_damping: AtomicF32,
    pub reverb_predelay: AtomicF32,
    pub reverb_width: AtomicF32,

    pub seq_playing: AtomicBool32,
    pub clock_source: AtomicEnum,
    pub tempo: AtomicF32,
    pub steps_per_beat: AtomicF32,
    pub seq_length: AtomicEnum,
    pub seq_gate: AtomicF32,
    pub seq_swing: AtomicF32,

    pub drum_enabled: AtomicBool32,
    pub melody_enabled: AtomicBool32,
    pub drum_length: AtomicEnum,
    pub drum_level: AtomicF32,
    pub drum_to_fx: AtomicBool32,
    pub drum_send: AtomicF32,
    pub pad_pan: [AtomicF32; PAD_COUNT],
    pub pad_send: [AtomicF32; PAD_COUNT],
    pub pad_level: [AtomicF32; PAD_COUNT],
    pub pad_tune: [AtomicF32; PAD_COUNT],
    pub pad_decay: [AtomicF32; PAD_COUNT],
    pub pad_mute: [AtomicBool32; PAD_COUNT],

    /// The grid the audio thread is actually playing, mirrored for the UI.
    ///
    /// One `u32` per step holds a whole column — eight pads at four bits each,
    /// which is 32 bits precisely. A column is therefore never read half
    /// written and the UI needs no synchronisation at all.
    ///
    /// Unlike `pattern`, this has no staging twin: drum edits are single cells
    /// and travel as events, so nothing but the audio thread ever writes here.
    pub drum_grid: [AtomicU32; crate::sequencer::MAX_STEPS],
    /// How many columns of the mirror the engine has written.
    pub drum_grid_len: AtomicU32,

    pub gen_enabled: AtomicBool32,
    pub gen_root: AtomicEnum,
    pub gen_scale: AtomicEnum,
    pub gen_octave: AtomicEnum,
    pub gen_range: AtomicEnum,
    pub gen_density: AtomicF32,
    pub gen_max_jump: AtomicF32,
    pub gen_chord_bias: AtomicF32,
    pub gen_seed: AtomicU64,
    /// Bumped by the control side to ask for a fresh pattern. The audio thread
    /// compares it against the last value it saw; a counter rather than a flag
    /// so two requests in one frame cannot collapse into one.
    pub gen_regenerate: AtomicU32,

    /// The current pattern, mirrored for the control side to display.
    ///
    /// The real pattern lives inside the sequencer on the audio thread. Rather
    /// than lock it, the engine publishes a packed copy here whenever it
    /// changes; a UI reads it with no synchronisation at all. One `u32` per
    /// step keeps every step's read atomic, so a step is never seen half
    /// updated.
    pub pattern: [AtomicU32; crate::sequencer::MAX_STEPS],

    /// How many of the steps above the engine has actually written.
    ///
    /// The mirror has to carry its own length. Sizing a read by `seq_length`
    /// instead would let the knob promise steps the engine never published:
    /// growing the pattern would hand the control side empty slots and a save
    /// would store the silence.
    pub pattern_len: AtomicU32,

    /// A pattern staged by the control side, waiting for the audio thread to
    /// adopt it. Packed the same way as the mirror above.
    ///
    /// Deliberately a second array rather than a reuse of that one: the audio
    /// thread owns every write to the mirror, and having both sides write the
    /// same words would be a genuine race. Each array stays one-directional.
    /// The cost of the rule is 256 bytes.
    pub pending_pattern: [AtomicU32; crate::sequencer::MAX_STEPS],
    /// How many steps of `pending_pattern` are meaningful. A pattern carries
    /// its own length, so loading one restores the loop length it was saved at.
    pub pending_length: AtomicU32,
    /// Bumped once the staging array is fully written, and never before. The
    /// audio thread compares it against the last value it saw, so it can only
    /// ever adopt a pattern that is completely there.
    pub pending_request: AtomicU32,

    // --- Read-only telemetry, written by the audio thread ---
    /// Current sequencer step, for UI display.
    pub current_step: AtomicU32,
    /// Current drum step, for UI display. Separate from `current_step`
    /// because the two grids can be different lengths.
    pub drum_position: AtomicU32,
    /// How many voices are sounding, for UI display.
    pub active_voices: AtomicU32,
    /// Peak output level since last read, for a meter.
    pub output_peak: AtomicF32,
}

impl Default for SharedParams {
    fn default() -> Self {
        Self::from_params(&Params::default())
    }
}

impl SharedParams {
    /// Builds a shared block from a plain snapshot. This is how patches load.
    pub fn from_params(p: &Params) -> Self {
        Self {
            osc1_wave: AtomicEnum::new(p.osc1_wave as u32),
            osc1_level: AtomicF32::new(p.osc1_level),
            osc1_semitones: AtomicF32::new(p.osc1_semitones),
            osc1_detune: AtomicF32::new(p.osc1_detune),

            osc2_wave: AtomicEnum::new(p.osc2_wave as u32),
            osc2_level: AtomicF32::new(p.osc2_level),
            osc2_semitones: AtomicF32::new(p.osc2_semitones),
            osc2_detune: AtomicF32::new(p.osc2_detune),

            pulse_width: AtomicF32::new(p.pulse_width),
            sub_level: AtomicF32::new(p.sub_level),
            noise_level: AtomicF32::new(p.noise_level),

            filter_mode: AtomicEnum::new(p.filter_mode as u32),
            filter_slope: AtomicEnum::new(p.filter_slope as u32),
            cutoff: AtomicF32::new(p.cutoff),
            resonance: AtomicF32::new(p.resonance),
            filter_env_amount: AtomicF32::new(p.filter_env_amount),
            filter_key_track: AtomicF32::new(p.filter_key_track),
            filter_velocity: AtomicF32::new(p.filter_velocity),

            amp_attack: AtomicF32::new(p.amp_env.attack),
            amp_decay: AtomicF32::new(p.amp_env.decay),
            amp_sustain: AtomicF32::new(p.amp_env.sustain),
            amp_release: AtomicF32::new(p.amp_env.release),

            filter_attack: AtomicF32::new(p.filter_env.attack),
            filter_decay: AtomicF32::new(p.filter_env.decay),
            filter_sustain: AtomicF32::new(p.filter_env.sustain),
            filter_release: AtomicF32::new(p.filter_env.release),

            lfo_wave: AtomicEnum::new(p.lfo_wave as u32),
            lfo_rate: AtomicF32::new(p.lfo_rate),
            lfo_depth: AtomicF32::new(p.lfo_depth),
            lfo_target: AtomicEnum::new(p.lfo_target as u32),
            lfo_retrigger: AtomicBool32::new(p.lfo_retrigger),

            voice_mode: AtomicEnum::new(p.voice_mode as u32),
            max_voices: AtomicEnum::new(p.max_voices as u32),
            glide: AtomicF32::new(p.glide),
            legato: AtomicBool32::new(p.legato),
            pitch_bend: AtomicF32::new(p.pitch_bend),
            mod_wheel: AtomicF32::new(p.mod_wheel),

            master_gain: AtomicF32::new(p.master_gain),
            synth_gain: AtomicF32::new(p.synth_gain),
            synth_send: AtomicF32::new(p.synth_send),
            comp_synth: SharedCompressor::new(&p.comp_synth),
            comp_master: SharedCompressor::new(&p.comp_master),
            drum_gain: AtomicF32::new(p.drum_gain),
            drive: AtomicF32::new(p.drive),

            delay_mix: AtomicF32::new(p.delay_mix),
            delay_sync: AtomicBool32::new(p.delay_sync),
            delay_time: AtomicF32::new(p.delay_time),
            delay_division: AtomicEnum::new(p.delay_division as u32),
            delay_feedback: AtomicF32::new(p.delay_feedback),
            delay_damping: AtomicF32::new(p.delay_damping),
            delay_ping_pong: AtomicBool32::new(p.delay_ping_pong),

            reverb_mix: AtomicF32::new(p.reverb_mix),
            reverb_size: AtomicF32::new(p.reverb_size),
            reverb_damping: AtomicF32::new(p.reverb_damping),
            reverb_predelay: AtomicF32::new(p.reverb_predelay),
            reverb_width: AtomicF32::new(p.reverb_width),

            seq_playing: AtomicBool32::new(p.seq_playing),
            clock_source: AtomicEnum::new(p.clock_source as u32),
            tempo: AtomicF32::new(p.tempo),
            steps_per_beat: AtomicF32::new(p.steps_per_beat),
            seq_length: AtomicEnum::new(p.seq_length as u32),
            seq_gate: AtomicF32::new(p.seq_gate),
            seq_swing: AtomicF32::new(p.seq_swing),

            drum_enabled: AtomicBool32::new(p.drum_enabled),
            melody_enabled: AtomicBool32::new(p.melody_enabled),
            drum_length: AtomicEnum::new(p.drum_length as u32),
            drum_level: AtomicF32::new(p.drum_level),
            drum_to_fx: AtomicBool32::new(p.drum_to_fx),
            drum_send: AtomicF32::new(p.drum_send),
            pad_pan: core::array::from_fn(|i| AtomicF32::new(p.pad_pan[i])),
            pad_send: core::array::from_fn(|i| AtomicF32::new(p.pad_send[i])),
            pad_level: core::array::from_fn(|i| AtomicF32::new(p.pad_level[i])),
            pad_tune: core::array::from_fn(|i| AtomicF32::new(p.pad_tune[i])),
            pad_decay: core::array::from_fn(|i| AtomicF32::new(p.pad_decay[i])),
            pad_mute: core::array::from_fn(|i| AtomicBool32::new(p.pad_mute[i])),

            // A default column rather than a zero word: the velocity nibble
            // is meaningful for silent cells too, so all-zeroes would decode
            // as eight cells at the softest level rather than as an untouched
            // grid. The audio thread overwrites this on its first block.
            drum_grid: core::array::from_fn(|_| AtomicU32::new(pack_column(&Column::default()))),
            drum_grid_len: AtomicU32::new(p.drum_length as u32),

            gen_enabled: AtomicBool32::new(p.gen_enabled),
            gen_root: AtomicEnum::new(p.gen_root as u32),
            gen_scale: AtomicEnum::new(p.gen_scale as u32),
            gen_octave: AtomicEnum::new(p.gen_octave as u32),
            gen_range: AtomicEnum::new(p.gen_range),
            gen_density: AtomicF32::new(p.gen_density),
            gen_max_jump: AtomicF32::new(p.gen_max_jump),
            gen_chord_bias: AtomicF32::new(p.gen_chord_bias),
            gen_seed: AtomicU64::new(0x5EED_1234_ABCD_0001),
            gen_regenerate: AtomicU32::new(0),

            pattern: core::array::from_fn(|_| AtomicU32::new(0)),
            pattern_len: AtomicU32::new(p.seq_length as u32),
            pending_pattern: core::array::from_fn(|_| AtomicU32::new(0)),
            pending_length: AtomicU32::new(0),
            pending_request: AtomicU32::new(0),

            current_step: AtomicU32::new(0),
            drum_position: AtomicU32::new(0),
            active_voices: AtomicU32::new(0),
            output_peak: AtomicF32::new(0.0),
        }
    }

    /// Reads every parameter into a plain struct. Called once per block by the
    /// audio thread, and validated as it goes so no out-of-range value from the
    /// control side can reach the DSP.
    pub fn snapshot(&self) -> Params {
        Params {
            osc1_wave: Waveform::from_u32(self.osc1_wave.get()),
            osc1_level: self.osc1_level.get().clamp(0.0, 1.0),
            osc1_semitones: self.osc1_semitones.get().clamp(-36.0, 36.0),
            osc1_detune: self.osc1_detune.get().clamp(-100.0, 100.0),

            osc2_wave: Waveform::from_u32(self.osc2_wave.get()),
            osc2_level: self.osc2_level.get().clamp(0.0, 1.0),
            osc2_semitones: self.osc2_semitones.get().clamp(-36.0, 36.0),
            osc2_detune: self.osc2_detune.get().clamp(-100.0, 100.0),

            pulse_width: self.pulse_width.get().clamp(0.05, 0.95),
            sub_level: self.sub_level.get().clamp(0.0, 1.0),
            noise_level: self.noise_level.get().clamp(0.0, 1.0),

            filter_mode: SvfMode::from_u32(self.filter_mode.get()),
            filter_slope: Slope::from_u32(self.filter_slope.get()),
            cutoff: self.cutoff.get().clamp(20.0, 20000.0),
            resonance: self.resonance.get().clamp(0.0, 1.0),
            filter_env_amount: self.filter_env_amount.get().clamp(-8.0, 8.0),
            filter_key_track: self.filter_key_track.get().clamp(0.0, 1.0),
            filter_velocity: self.filter_velocity.get().clamp(0.0, 1.0),

            amp_env: AdsrSettings {
                attack: self.amp_attack.get().clamp(0.0, 20.0),
                decay: self.amp_decay.get().clamp(0.0, 20.0),
                sustain: self.amp_sustain.get().clamp(0.0, 1.0),
                release: self.amp_release.get().clamp(0.001, 20.0),
            },
            filter_env: AdsrSettings {
                attack: self.filter_attack.get().clamp(0.0, 20.0),
                decay: self.filter_decay.get().clamp(0.0, 20.0),
                sustain: self.filter_sustain.get().clamp(0.0, 1.0),
                release: self.filter_release.get().clamp(0.001, 20.0),
            },

            lfo_wave: LfoWave::from_u32(self.lfo_wave.get()),
            lfo_rate: self.lfo_rate.get().clamp(0.01, 40.0),
            lfo_depth: self.lfo_depth.get().clamp(0.0, 1.0),
            lfo_target: LfoTarget::from_u32(self.lfo_target.get()),
            lfo_retrigger: self.lfo_retrigger.get(),

            voice_mode: VoiceMode::from_u32(self.voice_mode.get()),
            max_voices: (self.max_voices.get() as usize).clamp(1, crate::MAX_VOICES),
            glide: self.glide.get().clamp(0.0, 5.0),
            legato: self.legato.get(),
            pitch_bend: self.pitch_bend.get().clamp(-24.0, 24.0),
            mod_wheel: self.mod_wheel.get().clamp(0.0, 1.0),

            master_gain: self.master_gain.get().clamp(0.0, 2.0),
            synth_gain: self.synth_gain.get().clamp(0.0, 2.0),
            synth_send: clamp01(self.synth_send.get()),
            comp_synth: self.comp_synth.snapshot(),
            comp_master: self.comp_master.snapshot(),
            drum_gain: self.drum_gain.get().clamp(0.0, 2.0),
            drive: self.drive.get().clamp(0.1, 20.0),

            delay_mix: clamp01(self.delay_mix.get()),
            delay_sync: self.delay_sync.get(),
            delay_time: sane(self.delay_time.get(), 0.375).clamp(0.001, 2.0),
            delay_division: NoteDivision::from_u32(self.delay_division.get()),
            // Strictly below 1.0. At 1.0 the loop is a perfect integrator and
            // the repeats never stop.
            delay_feedback: clamp01(self.delay_feedback.get()).min(0.95),
            delay_damping: clamp01(self.delay_damping.get()),
            delay_ping_pong: self.delay_ping_pong.get(),

            reverb_mix: clamp01(self.reverb_mix.get()),
            reverb_size: clamp01(self.reverb_size.get()),
            reverb_damping: clamp01(self.reverb_damping.get()),
            reverb_predelay: sane(self.reverb_predelay.get(), 0.02).clamp(0.0, 0.25),
            reverb_width: clamp01(self.reverb_width.get()),

            seq_playing: self.seq_playing.get(),
            clock_source: ClockSource::from_u32(self.clock_source.get()),
            tempo: self.tempo.get().clamp(20.0, 300.0),
            steps_per_beat: self.steps_per_beat.get().clamp(0.25, 16.0),
            seq_length: (self.seq_length.get() as usize).clamp(1, crate::sequencer::MAX_STEPS),
            seq_gate: self.seq_gate.get().clamp(0.05, 2.0),
            seq_swing: self.seq_swing.get().clamp(0.0, 0.75),

            drum_enabled: self.drum_enabled.get(),
            melody_enabled: self.melody_enabled.get(),
            drum_length: (self.drum_length.get() as usize).clamp(1, crate::sequencer::MAX_STEPS),
            drum_level: clamp01(self.drum_level.get()),
            drum_to_fx: self.drum_to_fx.get(),
            drum_send: clamp01(self.drum_send.get()),
            pad_pan: core::array::from_fn(|i| sane(self.pad_pan[i].get(), 0.0).clamp(-1.0, 1.0)),
            pad_send: core::array::from_fn(|i| clamp01(self.pad_send[i].get())),
            pad_level: core::array::from_fn(|i| clamp01(self.pad_level[i].get())),
            pad_tune: core::array::from_fn(|i| sane(self.pad_tune[i].get(), 0.0).clamp(-12.0, 12.0)),
            pad_decay: core::array::from_fn(|i| sane(self.pad_decay[i].get(), 1.0).clamp(0.25, 4.0)),
            pad_mute: core::array::from_fn(|i| self.pad_mute[i].get()),

            gen_enabled: self.gen_enabled.get(),
            gen_root: (self.gen_root.get() % 12) as u8,
            gen_scale: Scale::from_u32(self.gen_scale.get()),
            gen_octave: (self.gen_octave.get() as i32).clamp(0, 8),
            gen_range: self.gen_range.get().clamp(1, 5),
            gen_density: self.gen_density.get().clamp(0.0, 1.0),
            gen_max_jump: self.gen_max_jump.get().clamp(1.0, 12.0),
            gen_chord_bias: self.gen_chord_bias.get().clamp(0.0, 1.0),
        }
    }

    /// Writes a whole patch at once.
    ///
    /// Not atomic as a group: the audio thread may snapshot halfway through and
    /// see a mix of old and new. That is harmless — every field is individually
    /// valid, and the worst case is one block of a slightly wrong patch.
    /// Blocking the audio thread to avoid it would be a far worse trade.
    pub fn apply(&self, p: &Params) {
        self.osc1_wave.set(p.osc1_wave as u32);
        self.osc1_level.set(p.osc1_level);
        self.osc1_semitones.set(p.osc1_semitones);
        self.osc1_detune.set(p.osc1_detune);

        self.osc2_wave.set(p.osc2_wave as u32);
        self.osc2_level.set(p.osc2_level);
        self.osc2_semitones.set(p.osc2_semitones);
        self.osc2_detune.set(p.osc2_detune);

        self.pulse_width.set(p.pulse_width);
        self.sub_level.set(p.sub_level);
        self.noise_level.set(p.noise_level);

        self.filter_mode.set(p.filter_mode as u32);
        self.filter_slope.set(p.filter_slope as u32);
        self.cutoff.set(p.cutoff);
        self.resonance.set(p.resonance);
        self.filter_env_amount.set(p.filter_env_amount);
        self.filter_key_track.set(p.filter_key_track);
        self.filter_velocity.set(p.filter_velocity);

        self.amp_attack.set(p.amp_env.attack);
        self.amp_decay.set(p.amp_env.decay);
        self.amp_sustain.set(p.amp_env.sustain);
        self.amp_release.set(p.amp_env.release);

        self.filter_attack.set(p.filter_env.attack);
        self.filter_decay.set(p.filter_env.decay);
        self.filter_sustain.set(p.filter_env.sustain);
        self.filter_release.set(p.filter_env.release);

        self.lfo_wave.set(p.lfo_wave as u32);
        self.lfo_rate.set(p.lfo_rate);
        self.lfo_depth.set(p.lfo_depth);
        self.lfo_target.set(p.lfo_target as u32);
        self.lfo_retrigger.set(p.lfo_retrigger);

        self.voice_mode.set(p.voice_mode as u32);
        self.max_voices.set(p.max_voices as u32);
        self.glide.set(p.glide);
        self.legato.set(p.legato);
        self.pitch_bend.set(p.pitch_bend);
        self.mod_wheel.set(p.mod_wheel);

        self.master_gain.set(p.master_gain);
        self.synth_gain.set(p.synth_gain);
        self.synth_send.set(p.synth_send);
        self.comp_synth.apply(&p.comp_synth);
        self.comp_master.apply(&p.comp_master);
        self.drum_gain.set(p.drum_gain);
        self.drive.set(p.drive);

        self.delay_mix.set(p.delay_mix);
        self.delay_sync.set(p.delay_sync);
        self.delay_time.set(p.delay_time);
        self.delay_division.set(p.delay_division as u32);
        self.delay_feedback.set(p.delay_feedback);
        self.delay_damping.set(p.delay_damping);
        self.delay_ping_pong.set(p.delay_ping_pong);

        self.reverb_mix.set(p.reverb_mix);
        self.reverb_size.set(p.reverb_size);
        self.reverb_damping.set(p.reverb_damping);
        self.reverb_predelay.set(p.reverb_predelay);
        self.reverb_width.set(p.reverb_width);

        self.seq_playing.set(p.seq_playing);
        self.clock_source.set(p.clock_source as u32);
        self.tempo.set(p.tempo);
        self.steps_per_beat.set(p.steps_per_beat);
        self.seq_length.set(p.seq_length as u32);
        self.seq_gate.set(p.seq_gate);
        self.seq_swing.set(p.seq_swing);

        self.drum_enabled.set(p.drum_enabled);
        self.melody_enabled.set(p.melody_enabled);
        self.drum_length.set(p.drum_length as u32);
        self.drum_level.set(p.drum_level);
        self.drum_to_fx.set(p.drum_to_fx);
        self.drum_send.set(p.drum_send);
        for i in 0..PAD_COUNT {
            self.pad_pan[i].set(p.pad_pan[i]);
            self.pad_send[i].set(p.pad_send[i]);
            self.pad_level[i].set(p.pad_level[i]);
            self.pad_tune[i].set(p.pad_tune[i]);
            self.pad_decay[i].set(p.pad_decay[i]);
            self.pad_mute[i].set(p.pad_mute[i]);
        }

        self.gen_enabled.set(p.gen_enabled);
        self.gen_root.set(p.gen_root as u32);
        self.gen_scale.set(p.gen_scale as u32);
        self.gen_octave.set(p.gen_octave as u32);
        self.gen_range.set(p.gen_range);
        self.gen_density.set(p.gen_density);
        self.gen_max_jump.set(p.gen_max_jump);
        self.gen_chord_bias.set(p.gen_chord_bias);
    }

    /// Publishes one step into the mirror. Called by the audio thread.
    pub fn publish_step(&self, index: usize, step: &crate::sequencer::Step) {
        if index < self.pattern.len() {
            self.pattern[index].store(pack_step(step), REL);
        }
    }

    /// Records how many steps the mirror now holds.
    ///
    /// Stored after the steps themselves, so a reader that picks up the new
    /// length has usually already seen the new data. Everything here is
    /// `Relaxed`, so that is a tendency rather than a guarantee — the cost of
    /// losing the race is one frame of a stale step in the grid.
    pub fn publish_len(&self, len: usize) {
        self.pattern_len.store(len as u32, REL);
    }

    /// Reads one step back out of the mirror.
    pub fn read_step(&self, index: usize) -> crate::sequencer::Step {
        if index >= self.pattern.len() {
            return crate::sequencer::Step::default();
        }
        unpack_step(self.pattern[index].load(REL))
    }

    /// Reads the whole pattern, as far as the current length.
    pub fn read_pattern(&self) -> crate::sequencer::Pattern {
        let len = (self.pattern_len.load(REL) as usize).clamp(1, crate::sequencer::MAX_STEPS);
        let steps = core::array::from_fn(|i| self.read_step(i));
        crate::sequencer::Pattern::new(steps, len)
    }

    /// Publishes one column of the grid. Called by the audio thread.
    pub fn publish_drum_column(&self, index: usize, column: &Column) {
        if index < self.drum_grid.len() {
            self.drum_grid[index].store(pack_column(column), REL);
        }
    }

    /// Records how many columns the mirror now holds. Stored after the columns
    /// themselves, for the reason `publish_len` explains.
    pub fn publish_drum_len(&self, len: usize) {
        self.drum_grid_len.store(len as u32, REL);
    }

    /// Reads one column back out of the mirror.
    pub fn read_drum_column(&self, index: usize) -> Column {
        if index >= self.drum_grid.len() {
            return Column::default();
        }
        unpack_column(self.drum_grid[index].load(REL))
    }

    /// Reads the whole grid, as far as the current length.
    pub fn read_drum_grid(&self) -> DrumPattern {
        let len = (self.drum_grid_len.load(REL) as usize).clamp(1, crate::sequencer::MAX_STEPS);
        let columns = core::array::from_fn(|i| self.read_drum_column(i));
        DrumPattern::new(columns, len)
    }

    /// Stages a pattern for the sequencer to adopt, and asks for it. Called by
    /// the control side, from anywhere, without waiting for anyone.
    ///
    /// The request counter goes last. Until it moves the audio thread has no
    /// reason to look at the array, so a half-written pattern is never one it
    /// could adopt.
    pub fn queue_pattern(&self, pattern: &crate::sequencer::Pattern) {
        for (slot, step) in self.pending_pattern.iter().zip(pattern.iter()) {
            slot.store(pack_step(step), REL);
        }
        self.pending_length.store(pattern.len() as u32, REL);
        self.pending_request.fetch_add(1, REL);
    }

    /// Reads the staged pattern back. Called by the audio thread once it sees
    /// the request counter move.
    pub fn read_pending_pattern(&self) -> crate::sequencer::Pattern {
        let len = (self.pending_length.load(REL) as usize).clamp(1, crate::sequencer::MAX_STEPS);
        let steps = core::array::from_fn(|i| unpack_step(self.pending_pattern[i].load(REL)));
        crate::sequencer::Pattern::new(steps, len)
    }

    /// Asks the sequencer for a new generated pattern at the next boundary.
    pub fn regenerate(&self) {
        self.gen_regenerate.fetch_add(1, REL);
    }

    /// Reads the output peak meter and resets it, so each read reports the peak
    /// since the previous one rather than since startup.
    pub fn take_peak(&self) -> f32 {
        let peak = self.output_peak.get();
        self.output_peak.set(0.0);
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_a_patch() {
        let mut p = Params::default();
        p.cutoff = 3456.0;
        p.resonance = 0.8;
        p.osc1_wave = Waveform::Pulse;
        p.voice_mode = VoiceMode::Mono;
        p.gen_scale = Scale::Dorian;
        p.synth_gain = 0.6;
        p.drum_gain = 1.4;

        let shared = SharedParams::default();
        shared.apply(&p);
        let back = shared.snapshot();

        assert_eq!(back.synth_gain, 0.6);
        assert_eq!(back.drum_gain, 1.4);
        assert_eq!(back.cutoff, 3456.0);
        assert_eq!(back.resonance, 0.8);
        assert_eq!(back.osc1_wave, Waveform::Pulse);
        assert_eq!(back.voice_mode, VoiceMode::Mono);
        assert_eq!(back.gen_scale, Scale::Dorian);
    }

    /// The control side is untrusted: a UI bug or a bad patch file must not be
    /// able to hand the DSP a negative cutoff or a NaN.
    #[test]
    fn snapshot_clamps_hostile_values() {
        let shared = SharedParams::default();
        shared.cutoff.set(-5000.0);
        shared.resonance.set(99.0);
        shared.max_voices.set(9999);
        shared.tempo.set(0.0);
        shared.synth_gain.set(-1.0);
        shared.drum_gain.set(f32::INFINITY);

        let p = shared.snapshot();
        assert!(p.synth_gain >= 0.0);
        assert!(p.drum_gain <= 2.0);
        assert!(p.cutoff >= 20.0);
        assert!(p.resonance <= 1.0);
        assert!(p.max_voices <= crate::MAX_VOICES);
        assert!(p.tempo >= 20.0);
    }

    #[test]
    fn smoothed_converges_without_overshoot() {
        let mut s = Smoothed::new(0.0, 10.0, 1500.0);
        s.set_target(1.0);
        let mut prev = 0.0;
        for _ in 0..1000 {
            let v = s.next();
            assert!(v >= prev - 1e-6, "went backwards");
            assert!(v <= 1.0 + 1e-6, "overshot to {v}");
            prev = v;
        }
        assert!((s.value() - 1.0).abs() < 1e-4);
    }

    /// The packed mirror must survive a round trip: the UI draws from it, and a
    /// note that comes back wrong would show the wrong pattern.
    #[test]
    fn the_pattern_mirror_round_trips() {
        use crate::sequencer::Step;
        let shared = SharedParams::default();

        for (index, (active, note, velocity, accent)) in [
            (true, 60u8, 1.0f32, true),
            (false, 0, 0.0, false),
            (true, 127, 0.5, false),
            (true, 36, 0.75, true),
        ]
        .into_iter()
        .enumerate()
        {
            let step = Step {
                active,
                note,
                velocity,
                accent,
            };
            shared.publish_step(index, &step);
            let back = shared.read_step(index);

            assert_eq!(back.active, active);
            assert_eq!(back.note, note);
            assert_eq!(back.accent, accent);
            // Velocity is quantised to 8 bits on the way through.
            assert!((back.velocity - velocity).abs() < 1.0 / 255.0 + 1e-6);
        }
    }

    #[test]
    fn read_pattern_reports_the_published_length_not_the_knob() {
        let shared = SharedParams::default();

        shared.publish_len(8);
        assert_eq!(shared.read_pattern().len(), 8);
        shared.publish_len(64);
        assert_eq!(shared.read_pattern().len(), 64);

        // The knob is not the authority. It can promise more steps than the
        // engine has published, and a read that believed it would hand back
        // slots that were never written -- silence, saved into a slot as if
        // it were music.
        shared.seq_length.set(64);
        shared.publish_len(8);
        assert_eq!(shared.read_pattern().len(), 8);

        // Out of range indices must be inert rather than panicking.
        shared.publish_step(9999, &crate::sequencer::Step::default());
        assert!(!shared.read_step(9999).active);
    }

    /// The staging array is how a pattern saved on the control side reaches the
    /// audio thread. A step that came back wrong would play the wrong melody.
    #[test]
    fn a_staged_pattern_round_trips() {
        use crate::sequencer::{Pattern, Step, MAX_STEPS};
        let shared = SharedParams::default();

        let mut steps = [Step::default(); MAX_STEPS];
        steps[0] = Step {
            active: true,
            note: 60,
            velocity: 1.0,
            accent: true,
        };
        steps[3] = Step {
            active: true,
            note: 67,
            velocity: 0.5,
            accent: false,
        };
        shared.queue_pattern(&Pattern::new(steps, 8));

        let back = shared.read_pending_pattern();
        assert_eq!(back.len(), 8);
        assert!(back[0].active && back[0].accent);
        assert_eq!(back[0].note, 60);
        assert_eq!(back[3].note, 67);
        assert!((back[3].velocity - 0.5).abs() < 1.0 / 255.0 + 1e-6);
        assert!(!back[1].active);
    }

    /// The counter is bumped last, after every step is in place, so the audio
    /// thread cannot adopt a pattern it caught halfway through being written.
    /// A counter rather than a flag, so two loads in one frame cannot collapse
    /// into one.
    #[test]
    fn staging_a_pattern_bumps_the_request_counter() {
        let shared = SharedParams::default();
        let before = shared.pending_request.load(REL);
        shared.queue_pattern(&crate::sequencer::Pattern::default());
        shared.queue_pattern(&crate::sequencer::Pattern::default());
        assert_eq!(shared.pending_request.load(REL) - before, 2);
    }

    #[test]
    fn atomic_f32_round_trips() {
        let a = AtomicF32::new(0.0);
        for v in [-1.5f32, 0.0, 1.0, 12345.678, f32::MIN, f32::MAX] {
            a.set(v);
            assert_eq!(a.get(), v);
        }
    }

    #[test]
    fn the_effects_default_to_silent() {
        let params = Params::default();
        // A patch nobody has touched must sound exactly as it did before the
        // effects existed. Zero mix on both is what guarantees that.
        assert_eq!(params.delay_mix, 0.0);
        assert_eq!(params.reverb_mix, 0.0);
    }

    #[test]
    fn effect_parameters_survive_a_round_trip() {
        let mut params = Params::default();
        params.delay_mix = 0.4;
        params.delay_sync = true;
        params.delay_time = 0.25;
        params.delay_division = NoteDivision::QuarterDot;
        params.delay_feedback = 0.6;
        params.delay_damping = 0.7;
        params.delay_ping_pong = true;
        params.reverb_mix = 0.3;
        params.reverb_size = 0.9;
        params.reverb_damping = 0.2;
        params.reverb_predelay = 0.05;
        params.reverb_width = 0.8;

        let shared = SharedParams::from_params(&params);
        let back = shared.snapshot();

        assert_eq!(back.delay_mix, 0.4);
        assert!(back.delay_sync);
        assert_eq!(back.delay_time, 0.25);
        assert_eq!(back.delay_division, NoteDivision::QuarterDot);
        assert_eq!(back.delay_feedback, 0.6);
        assert_eq!(back.delay_damping, 0.7);
        assert!(back.delay_ping_pong);
        assert_eq!(back.reverb_mix, 0.3);
        assert_eq!(back.reverb_size, 0.9);
        assert_eq!(back.reverb_damping, 0.2);
        assert_eq!(back.reverb_predelay, 0.05);
        assert_eq!(back.reverb_width, 0.8);

        // `apply` writes back into the same atomics.
        let blank = SharedParams::from_params(&Params::default());
        blank.apply(&params);
        assert_eq!(blank.snapshot().delay_feedback, 0.6);
        assert_eq!(blank.snapshot().reverb_size, 0.9);
    }

    #[test]
    fn effect_parameters_are_clamped_on_the_way_out() {
        let shared = SharedParams::from_params(&Params::default());
        shared.delay_mix.set(9.0);
        shared.delay_time.set(-1.0);
        shared.delay_feedback.set(2.0);
        shared.delay_damping.set(-0.5);
        shared.reverb_mix.set(f32::NAN);
        shared.reverb_size.set(50.0);
        shared.reverb_predelay.set(10.0);
        shared.reverb_width.set(-3.0);

        let params = shared.snapshot();
        assert_eq!(params.delay_mix, 1.0);
        assert_eq!(params.delay_time, 0.001);
        // Strictly below 1.0: at 1.0 the feedback loop never decays.
        assert!(params.delay_feedback <= 0.95);
        assert_eq!(params.delay_damping, 0.0);
        assert!(params.reverb_mix.is_finite());
        assert_eq!(params.reverb_size, 1.0);
        assert_eq!(params.reverb_predelay, 0.25);
        assert_eq!(params.reverb_width, 0.0);
    }

    /// The drum knobs have to survive the trip out to the atomics and back,
    /// and nonsense has to be clamped on the way: `snapshot` is the only thing
    /// standing between a bad value and the audio thread.
    #[test]
    fn drum_params_round_trip_and_clamp() {
        let shared = SharedParams::default();
        assert!(!shared.snapshot().drum_enabled);

        shared.drum_enabled.set(true);
        shared.drum_level.set(4.0);
        shared.drum_length.set(999);
        shared.pad_tune[2].set(-90.0);
        shared.pad_decay[2].set(0.0);
        shared.pad_mute[5].set(true);

        let p = shared.snapshot();
        assert!(p.drum_enabled);
        assert_eq!(p.drum_level, 1.0);
        assert_eq!(p.drum_length, crate::sequencer::MAX_STEPS);
        assert_eq!(p.pad_tune[2], -12.0);
        assert_eq!(p.pad_decay[2], 0.25);
        assert!(p.pad_mute[5]);
        assert!(!p.pad_mute[0]);

        // And back out again, which is what loading a patch does.
        let restored = SharedParams::default();
        restored.apply(&p);
        assert!(restored.snapshot().pad_mute[5]);
    }

    /// The grid mirror is how the UI sees what the audio thread is playing.
    #[test]
    fn the_drum_mirror_round_trips_a_column() {
        use crate::drums::{Cell, Column};

        let shared = SharedParams::default();
        let mut column = Column::default();
        column[1] = Cell { active: true, velocity: 1.0 };

        shared.publish_drum_column(3, &column);
        shared.publish_drum_len(16);

        let grid = shared.read_drum_grid();
        assert_eq!(grid.len(), 16);
        assert!(grid.get(3, 1).active);
        assert!(!grid.get(3, 0).active);
        assert!(!grid.get(4, 1).active);
    }

    #[test]
    fn the_new_bus_params_default_to_todays_routing() {
        let p = Params::default();
        // Today's engine sends the synth through the effects and keeps drums
        // out of them. These two defaults are what reproduce that.
        assert_eq!(p.synth_send, 1.0);
        assert_eq!(p.drum_send, 0.0);
        assert_eq!(p.pad_pan, [0.0; PAD_COUNT]);
        assert_eq!(p.pad_send, [1.0; PAD_COUNT]);
    }

    #[test]
    fn bus_params_round_trip_and_clamp() {
        let mut p = Params::default();
        p.synth_send = 0.25;
        p.drum_send = 0.5;
        p.pad_pan[2] = -1.0;
        p.pad_send[3] = 0.125;

        let shared = SharedParams::default();
        shared.apply(&p);
        let back = shared.snapshot();

        assert_eq!(back.synth_send, 0.25);
        assert_eq!(back.drum_send, 0.5);
        assert_eq!(back.pad_pan[2], -1.0);
        assert_eq!(back.pad_send[3], 0.125);

        // Hostile values from the control side are clamped, not trusted.
        shared.synth_send.set(9.0);
        shared.drum_send.set(f32::NAN);
        shared.pad_pan[0].set(-4.0);
        shared.pad_send[0].set(2.0);
        let back = shared.snapshot();
        assert_eq!(back.synth_send, 1.0);
        assert_eq!(back.drum_send, 0.0);
        assert_eq!(back.pad_pan[0], -1.0);
        assert_eq!(back.pad_send[0], 1.0);
    }

    #[test]
    fn compressor_params_default_to_bypassed() {
        let p = Params::default();
        for c in [p.comp_synth, p.comp_master] {
            assert!(!c.on);
            assert_eq!(c.threshold_db, -12.0);
            assert_eq!(c.ratio, 4.0);
            assert_eq!(c.attack_ms, 10.0);
            assert_eq!(c.release_ms, 100.0);
            assert_eq!(c.makeup_db, 0.0);
            assert_eq!(c.sidechain, SidechainSource::Off);
        }
    }

    #[test]
    fn compressor_params_round_trip_and_clamp() {
        let mut p = Params::default();
        p.comp_synth.on = true;
        p.comp_synth.threshold_db = -24.0;
        p.comp_synth.sidechain = SidechainSource::Kick;
        p.comp_master.ratio = 2.0;

        let shared = SharedParams::default();
        shared.apply(&p);
        let back = shared.snapshot();

        assert!(back.comp_synth.on);
        assert_eq!(back.comp_synth.threshold_db, -24.0);
        assert_eq!(back.comp_synth.sidechain, SidechainSource::Kick);
        assert_eq!(back.comp_master.ratio, 2.0);
        // The master is untouched by the synth compressor's settings.
        assert!(!back.comp_master.on);

        shared.comp_synth.threshold_db.set(12.0);
        shared.comp_synth.ratio.set(f32::NAN);
        shared.comp_master.attack_ms.set(0.0);
        shared.comp_master.release_ms.set(9999.0);
        let back = shared.snapshot();
        assert_eq!(back.comp_synth.threshold_db, 0.0);
        assert_eq!(back.comp_synth.ratio, 4.0);
        assert_eq!(back.comp_master.attack_ms, 0.1);
        assert_eq!(back.comp_master.release_ms, 1000.0);
    }
}
