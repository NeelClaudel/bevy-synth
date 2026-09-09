//! The drum rack: eight synthesized percussion voices and a grid to fire them.
//!
//! Synthesized rather than sampled because `synth_core` has no dependencies
//! and does no I/O — there is nothing here that could load a wav. That turns
//! out to be a feature: every pad is tunable and stretchable at runtime with
//! no resampler in sight.

pub mod voice;

pub use voice::{Decay, DrumVoice, Pad, PAD_COUNT};
