//! Notes, pitch and tuning.

/// Concert A reference, in Hz. A4 is MIDI note 69.
pub const A4_HZ: f32 = 440.0;
pub const A4_MIDI: f32 = 69.0;

/// Converts a MIDI note number to frequency in Hz.
///
/// Takes an `f32` rather than a `u8` on purpose: pitch bend, glide and vibrato
/// all land between notes, and doing the conversion once at the end keeps them
/// all in the same linear-in-semitones space where they add cleanly.
#[inline]
pub fn midi_to_hz(note: f32) -> f32 {
    A4_HZ * ((note - A4_MIDI) / 12.0).exp2()
}

/// Converts a frequency in Hz back to a MIDI note number.
#[inline]
pub fn hz_to_midi(hz: f32) -> f32 {
    if hz <= 0.0 {
        return 0.0;
    }
    A4_MIDI + 12.0 * (hz / A4_HZ).log2()
}

/// Converts a detune amount in cents to a frequency multiplier.
#[inline]
pub fn cents_to_ratio(cents: f32) -> f32 {
    (cents / 1200.0).exp2()
}

/// A note as the control side sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Note {
    /// MIDI note number, 0-127. 60 is middle C.
    pub pitch: u8,
    /// 0.0 to 1.0. Drives amplitude and, optionally, filter brightness.
    pub velocity: f32,
}

impl Note {
    pub fn new(pitch: u8, velocity: f32) -> Self {
        Self {
            pitch,
            velocity: velocity.clamp(0.0, 1.0),
        }
    }

    /// Builds a note from raw MIDI bytes, mapping velocity 0-127 to 0.0-1.0.
    pub fn from_midi(pitch: u8, velocity: u8) -> Self {
        Self::new(pitch, velocity as f32 / 127.0)
    }

    #[inline]
    pub fn hz(self) -> f32 {
        midi_to_hz(self.pitch as f32)
    }

    /// Note name with octave, e.g. `("C", 4)` for middle C.
    pub fn name(self) -> (&'static str, i32) {
        const NAMES: [&str; 12] = [
            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
        ];
        let pc = (self.pitch % 12) as usize;
        // MIDI 60 is C4 in the convention most DAWs use.
        let octave = self.pitch as i32 / 12 - 1;
        (NAMES[pc], octave)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a4_is_440() {
        assert!((midi_to_hz(69.0) - 440.0).abs() < 0.001);
    }

    #[test]
    fn an_octave_doubles() {
        assert!((midi_to_hz(81.0) - 880.0).abs() < 0.001);
        assert!((midi_to_hz(57.0) - 220.0).abs() < 0.001);
    }

    #[test]
    fn hz_round_trips() {
        for note in 0..128 {
            let back = hz_to_midi(midi_to_hz(note as f32));
            assert!((back - note as f32).abs() < 0.001);
        }
    }

    #[test]
    fn middle_c_is_c4() {
        assert_eq!(Note::new(60, 1.0).name(), ("C", 4));
    }
}
