//! The drum rack: eight synthesized percussion voices and a grid to fire them.
//!
//! Synthesized rather than sampled because `synth_core` has no dependencies
//! and does no I/O — there is nothing here that could load a wav. That turns
//! out to be a feature: every pad is tunable and stretchable at runtime with
//! no resampler in sight.

pub mod pattern;
mod sequencer;
pub mod voice;

pub use pattern::{pack_column, unpack_column, Cell, Column, DrumPattern};
pub use sequencer::{DrumOutput, DrumSequencer};
pub use voice::{Decay, DrumVoice, Pad, PAD_COUNT};
