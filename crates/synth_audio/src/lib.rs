//! Audio output and MIDI input for [`synth_core`].
//!
//! `synth_core` is deliberately free of I/O. This crate supplies the two ends
//! it needs in a real program: a cpal output stream that calls
//! [`synth_core::Engine::process`], and a midir connection that turns hardware
//! MIDI into engine events.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use synth_audio::Synth;
//! use synth_core::Event;
//!
//! let synth = Synth::start()?;
//! synth.params.cutoff.set(1200.0);
//! synth.events.push(Event::NoteOn { note: 60, velocity: 0.9 });
//! # Ok(())
//! # }
//! ```

pub mod host;
#[cfg(feature = "midi")]
pub mod midi;
pub mod offline;

pub use host::{Synth, SynthBuilder, SynthError};
#[cfg(feature = "midi")]
pub use midi::{list_ports, MidiInput, PortSelector};
pub use offline::{render, write_wav};

// Re-exported so callers need only depend on this crate.
pub use synth_core;
