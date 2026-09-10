//! One module per panel section. Each draws its own block of the window and
//! writes straight to the shared atomics; none of them hold state of their own
//! beyond what `SynthUi` already carries.
//!
//! The modules are private and their entry points re-exported, so a caller
//! writes `sections::filter_section` rather than `sections::filter::filter_section`.

mod delay;
mod drums;
mod envelopes;
mod filter;
mod keyboard;
mod lfo;
mod mixer;
mod oscillators;
mod reverb;
mod sequencer;
mod transport;
mod voices;

pub(crate) use delay::delay_section;
pub(crate) use drums::drums;
pub(crate) use envelopes::envelopes;
pub(crate) use filter::filter_section;
pub(crate) use keyboard::keyboard;
pub(crate) use lfo::lfo_section;
pub(crate) use mixer::mixer;
pub(crate) use oscillators::oscillators;
pub(crate) use reverb::reverb_section;
pub(crate) use sequencer::sequencer;
pub(crate) use transport::transport;
pub(crate) use voices::voice_section;
