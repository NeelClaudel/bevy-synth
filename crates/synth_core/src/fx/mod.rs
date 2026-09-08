//! The effects stage: a stereo delay and a plate reverb, in that order.

mod delay;
mod line;

pub use delay::NoteDivision;
pub use line::DelayLine;
