//! `synth_core` — the real-time half of the synth.
//!
//! Nothing in this crate knows about Bevy, cpal or MIDI hardware. It is pure
//! DSP plus a lock-free parameter bridge, so it can be driven from an audio
//! callback, an offline renderer or a unit test without change.
//!
//! # The two clocks
//!
//! A synth has two rates and mixing them up is the classic mistake:
//!
//! * **Audio rate** (44100 / 48000 Hz) — everything in [`Engine::process`].
//! * **Control rate** — the game loop, UI knobs, note triggers. Runs at 60 Hz
//!   with unpredictable jitter.
//!
//! The control side never generates samples. It writes to [`SharedParams`]
//! (atomics, no locks) and pushes [`Event`]s. The audio side reads them at a
//! block boundary and smooths them. That is the whole contract.
//!
//! # Real-time rules obeyed here
//!
//! No allocation, no locking, no `panic!`, no syscalls anywhere below
//! [`Engine::process`]. Voice storage is a fixed array; every buffer is
//! preallocated at construction.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod clock;
pub mod drums;
pub mod engine;
pub mod env;
pub mod event;
pub mod filter;
pub mod fx;
pub mod lfo;
pub mod note;
pub mod osc;
pub mod params;
pub mod rng;
pub mod scale;
pub mod sequencer;
pub mod voice;

pub use clock::{Clock, ClockSource, ClockView};
pub use drums::{Cell, DrumPattern, DrumRack, DrumVoice, Pad, PAD_COUNT};
pub use engine::{Engine, MAX_VOICES};
pub use env::{Adsr, AdsrSettings, EnvStage};
pub use event::{Event, EventQueue};
pub use filter::{Svf, SvfMode};
pub use fx::{FxChain, NoteDivision};
pub use lfo::{Lfo, LfoTarget};
pub use note::{midi_to_hz, Note};
pub use osc::{Oscillator, Waveform};
pub use params::{Params, SharedParams, Smoothed, VoiceMode};
pub use rng::Rng;
pub use scale::{Scale, ScaleQuantizer};
pub use sequencer::{GenerativeSettings, Pattern, Sequencer, Step};

/// Control-rate block size, in samples.
///
/// Parameters are re-read and smoothed once per block instead of per sample.
/// 32 samples is ~0.7 ms at 48 kHz: far below anything audible as lag, but 32x
/// less parameter overhead. It also bounds sequencer timing error to under a
/// millisecond, which is tighter than a human can play.
pub const BLOCK: usize = 32;
