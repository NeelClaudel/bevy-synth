//! The effects stage: a stereo delay and a plate reverb, in that order.

mod delay;
mod line;
mod reverb;

pub use delay::{NoteDivision, StereoDelay};
pub use line::DelayLine;
pub use reverb::PlateReverb;
