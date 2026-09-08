//! Offline rendering, for tests, previews and bouncing audio to disk.
//!
//! Rendering without a sound card is how the DSP gets tested on CI, and how you
//! check a patch by looking at it rather than by ear. It runs the exact same
//! [`Engine`] the live stream does, just faster than real time.

use std::io::{self, Write};
use std::path::Path;

use synth_core::Engine;

/// Renders `seconds` of audio into a buffer, as fast as the machine allows.
pub fn render(engine: &mut Engine, seconds: f32) -> Vec<f32> {
    let count = (engine.sample_rate() * seconds.max(0.0)) as usize;
    let mut out = vec![0.0f32; count];
    engine.process(&mut out);
    out
}

/// Writes mono `f32` samples to a 16-bit PCM WAV file.
///
/// Hand-rolled rather than pulled from a crate: a WAV header is 44 bytes of
/// well-documented struct, and this is the only file format the project needs.
pub fn write_wav(path: impl AsRef<Path>, samples: &[f32], sample_rate: f32) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;

    let channels: u16 = 1;
    let bits: u16 = 16;
    let rate = sample_rate as u32;
    let byte_rate = rate * channels as u32 * (bits / 8) as u32;
    let block_align = channels * bits / 8;
    let data_bytes = (samples.len() * 2) as u32;

    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_bytes).to_le_bytes())?;
    file.write_all(b"WAVE")?;

    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?; // PCM header size
    file.write_all(&1u16.to_le_bytes())?; // format: uncompressed PCM
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&bits.to_le_bytes())?;

    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;

    for &sample in samples {
        // Scale by 32767, not 32768: the positive side of a signed 16-bit range
        // stops one short, and scaling by 32768 would wrap a full-scale +1.0
        // sample around to a loud negative click.
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        file.write_all(&value.to_le_bytes())?;
    }

    Ok(())
}

/// Peak and RMS of a buffer, in linear amplitude. Useful for asserting that a
/// patch actually made a sound without listening to it.
pub fn levels(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let peak = samples.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum / samples.len() as f64).sqrt() as f32;
    (peak, rms)
}
