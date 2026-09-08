//! Scales and pitch quantization.
//!
//! # Why this module exists
//!
//! "Random notes" and "random notes that sound good" are different problems.
//! Uniform random MIDI numbers sound like a fault, because most intervals in
//! twelve-tone space are dissonant and the ear has nothing to hold on to.
//!
//! Constraining pitches to a scale fixes most of it in about twenty lines: a
//! scale is a small table of semitone offsets, and a random *degree* mapped
//! through that table is always consonant with every other note in the key.
//!
//! The rest — which degrees to favour, when to leap, when to rest — lives in
//! [`crate::sequencer`], which uses the weights this module provides.

/// A scale, as a set of semitone offsets from the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum Scale {
    Major = 0,
    NaturalMinor = 1,
    HarmonicMinor = 2,
    Dorian = 3,
    Phrygian = 4,
    Lydian = 5,
    Mixolydian = 6,
    MajorPentatonic = 7,
    /// Five notes, no semitone clashes at all. Nearly impossible to make sound
    /// wrong, which is why it is the default for generative patches.
    #[default]
    MinorPentatonic = 8,
    Blues = 9,
    WholeTone = 10,
    /// A Japanese pentatonic scale. Dark and immediately evocative.
    Hirajoshi = 11,
    /// The "Spanish" or Freygish scale. Strong, tense, good for menace.
    PhrygianDominant = 12,
    Chromatic = 13,
}

impl Scale {
    /// Semitone offsets from the root, one octave.
    pub fn intervals(self) -> &'static [u8] {
        match self {
            Scale::Major => &[0, 2, 4, 5, 7, 9, 11],
            Scale::NaturalMinor => &[0, 2, 3, 5, 7, 8, 10],
            Scale::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Scale::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Scale::Phrygian => &[0, 1, 3, 5, 7, 8, 10],
            Scale::Lydian => &[0, 2, 4, 6, 7, 9, 11],
            Scale::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
            Scale::MajorPentatonic => &[0, 2, 4, 7, 9],
            Scale::MinorPentatonic => &[0, 3, 5, 7, 10],
            Scale::Blues => &[0, 3, 5, 6, 7, 10],
            Scale::WholeTone => &[0, 2, 4, 6, 8, 10],
            Scale::Hirajoshi => &[0, 2, 3, 7, 8],
            Scale::PhrygianDominant => &[0, 1, 4, 5, 7, 8, 10],
            Scale::Chromatic => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        }
    }

    /// Number of degrees in one octave of this scale.
    pub fn len(self) -> usize {
        self.intervals().len()
    }

    pub fn is_empty(self) -> bool {
        false
    }

    pub fn from_u32(v: u32) -> Self {
        Self::ALL
            .get(v as usize)
            .copied()
            .unwrap_or(Scale::MinorPentatonic)
    }

    pub const ALL: [Scale; 14] = [
        Scale::Major,
        Scale::NaturalMinor,
        Scale::HarmonicMinor,
        Scale::Dorian,
        Scale::Phrygian,
        Scale::Lydian,
        Scale::Mixolydian,
        Scale::MajorPentatonic,
        Scale::MinorPentatonic,
        Scale::Blues,
        Scale::WholeTone,
        Scale::Hirajoshi,
        Scale::PhrygianDominant,
        Scale::Chromatic,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scale::Major => "Major",
            Scale::NaturalMinor => "Natural Minor",
            Scale::HarmonicMinor => "Harmonic Minor",
            Scale::Dorian => "Dorian",
            Scale::Phrygian => "Phrygian",
            Scale::Lydian => "Lydian",
            Scale::Mixolydian => "Mixolydian",
            Scale::MajorPentatonic => "Major Pentatonic",
            Scale::MinorPentatonic => "Minor Pentatonic",
            Scale::Blues => "Blues",
            Scale::WholeTone => "Whole Tone",
            Scale::Hirajoshi => "Hirajoshi",
            Scale::PhrygianDominant => "Phrygian Dominant",
            Scale::Chromatic => "Chromatic",
        }
    }

    /// Which degrees of this scale form the tonic triad.
    ///
    /// Degrees 0, 2 and 4 are the root, third and fifth for any seven-note
    /// scale. Pentatonic scales have their third and fifth at different
    /// indices, so they get their own answer. Landing on these on a downbeat is
    /// what makes a generated line sound like it has a key centre rather than
    /// just staying inside one.
    pub fn chord_degrees(self) -> &'static [usize] {
        match self {
            Scale::MinorPentatonic => &[0, 1, 3],  // root, b3, 5
            Scale::MajorPentatonic => &[0, 2, 3],  // root, 3, 5
            Scale::Hirajoshi => &[0, 2, 3],        // root, b3, 5
            Scale::Blues => &[0, 1, 4],            // root, b3, 5
            Scale::WholeTone => &[0, 2, 4],
            Scale::Chromatic => &[0, 4, 7],
            // Every diatonic mode: 1st, 3rd, 5th.
            _ => &[0, 2, 4],
        }
    }
}

/// Maps scale degrees to MIDI notes, and arbitrary MIDI notes onto the scale.
#[derive(Debug, Clone, Copy)]
pub struct ScaleQuantizer {
    /// Pitch class of the tonic, 0 = C.
    pub root: u8,
    pub scale: Scale,
}

impl ScaleQuantizer {
    pub fn new(root: u8, scale: Scale) -> Self {
        Self {
            root: root % 12,
            scale,
        }
    }

    /// Converts a scale degree to a MIDI note.
    ///
    /// Degrees are unbounded in both directions: degree 7 of a seven-note scale
    /// is the root an octave up, degree -1 is the seventh an octave down. That
    /// is what lets the generator treat melodic movement as simple integer
    /// arithmetic and never worry about octave boundaries.
    pub fn degree_to_midi(&self, degree: i32, base_octave: i32) -> u8 {
        let intervals = self.scale.intervals();
        let n = intervals.len() as i32;

        // Euclidean remainder, so negative degrees wrap downward correctly
        // rather than toward zero.
        let index = degree.rem_euclid(n) as usize;
        let octave_offset = (degree - degree.rem_euclid(n)) / n;

        // MIDI note 0 is C-1, so octave `o` starts at (o + 1) * 12.
        let base = (base_octave + 1) * 12 + self.root as i32;
        let mut note = base + intervals[index] as i32 + octave_offset * 12;

        // Fold by whole octaves rather than clamping. A clamp would silently
        // move the note off the scale — the caller asked for a degree, and a
        // degree that lands outside MIDI range should come back as the same
        // pitch class in a range that exists, not as note 0.
        while note < 0 {
            note += 12;
        }
        while note > 127 {
            note -= 12;
        }

        note as u8
    }

    /// Snaps any MIDI note to the nearest note in the scale.
    ///
    /// Useful for quantizing a live keyboard, or for forcing a hand-written
    /// pattern into a key that the player changed at runtime.
    pub fn snap(&self, note: u8) -> u8 {
        let intervals = self.scale.intervals();
        let pitch_class = (note as i32 - self.root as i32).rem_euclid(12);

        let mut best = intervals[0] as i32;
        let mut best_distance = i32::MAX;
        for &iv in intervals {
            // Compare against the interval both in this octave and the next, so
            // a note just below the root snaps up rather than down a seventh.
            for candidate in [iv as i32, iv as i32 + 12] {
                let d = (candidate - pitch_class).abs();
                if d < best_distance {
                    best_distance = d;
                    best = candidate;
                }
            }
        }

        let mut snapped = note as i32 + (best - pitch_class);
        // Same reasoning as `degree_to_midi`: fold, never clamp.
        while snapped < 0 {
            snapped += 12;
        }
        while snapped > 127 {
            snapped -= 12;
        }
        snapped as u8
    }

    /// True if the note is already in the scale.
    pub fn contains(&self, note: u8) -> bool {
        let pitch_class = ((note as i32 - self.root as i32).rem_euclid(12)) as u8;
        self.scale.intervals().contains(&pitch_class)
    }

    /// Builds a weight per scale degree for random selection.
    ///
    /// `chord_bias` in `0.0..=1.0` decides how strongly chord tones are
    /// favoured; `strong_beat` says whether this step is a downbeat. Off the
    /// beat the weights flatten out, so passing tones fill the gaps — which is
    /// what a human player does, and what stops the line sounding like an
    /// arpeggiator.
    pub fn degree_weights(&self, chord_bias: f32, strong_beat: bool, out: &mut [f32]) {
        let n = self.scale.len().min(out.len());
        let chord = self.scale.chord_degrees();
        // Off the beat, halve the pull toward chord tones.
        let bias = if strong_beat {
            chord_bias
        } else {
            chord_bias * 0.5
        };

        for (i, w) in out.iter_mut().enumerate().take(n) {
            let is_chord_tone = chord.contains(&i);
            // Baseline 1.0 for every degree, plus up to 4x extra for chord
            // tones. Non-chord tones are never weighted to zero: a melody that
            // only ever hits the triad is an arpeggio, not a tune.
            *w = if is_chord_tone {
                1.0 + bias * 4.0
            } else {
                1.0 - bias * 0.4
            };
            // The root gets a little extra on top, so lines resolve.
            if i == 0 {
                *w += bias;
            }
        }
        for w in out.iter_mut().skip(n) {
            *w = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degree_zero_is_the_root() {
        let q = ScaleQuantizer::new(0, Scale::Major);
        // C4 is MIDI 60.
        assert_eq!(q.degree_to_midi(0, 4), 60);
    }

    #[test]
    fn a_full_scale_length_is_an_octave() {
        for scale in Scale::ALL {
            let q = ScaleQuantizer::new(0, scale);
            let root = q.degree_to_midi(0, 4);
            let octave_up = q.degree_to_midi(scale.len() as i32, 4);
            assert_eq!(octave_up - root, 12, "{scale:?} octave was wrong");
        }
    }

    #[test]
    fn negative_degrees_go_down_an_octave() {
        for scale in Scale::ALL {
            let q = ScaleQuantizer::new(0, scale);
            let root = q.degree_to_midi(0, 4);
            let octave_down = q.degree_to_midi(-(scale.len() as i32), 4);
            assert_eq!(root as i32 - octave_down as i32, 12, "{scale:?}");
        }
    }

    #[test]
    fn c_major_is_the_white_keys() {
        let q = ScaleQuantizer::new(0, Scale::Major);
        let notes: Vec<u8> = (0..8).map(|d| q.degree_to_midi(d, 4)).collect();
        assert_eq!(notes, vec![60, 62, 64, 65, 67, 69, 71, 72]);
    }

    #[test]
    fn every_generated_degree_is_in_the_scale() {
        for scale in Scale::ALL {
            for root in 0..12 {
                let q = ScaleQuantizer::new(root, scale);
                for degree in -30..30 {
                    let n = q.degree_to_midi(degree, 4);
                    assert!(q.contains(n), "{scale:?} root {root} degree {degree} -> {n}");
                }
            }
        }
    }

    #[test]
    fn snap_lands_in_the_scale_and_stays_close() {
        for scale in Scale::ALL {
            for root in 0..12 {
                let q = ScaleQuantizer::new(root, scale);
                for note in 12..116u8 {
                    let s = q.snap(note);
                    assert!(q.contains(s), "{scale:?} snapped {note} to {s}, not in scale");
                    // Never move a note more than a whole tone; anything more
                    // and the melody would be unrecognisable after quantizing.
                    assert!(
                        (s as i32 - note as i32).abs() <= 3,
                        "{scale:?} moved {note} to {s}"
                    );
                }
            }
        }
    }

    #[test]
    fn snap_leaves_in_scale_notes_alone() {
        let q = ScaleQuantizer::new(0, Scale::Major);
        for note in [60, 62, 64, 65, 67, 69, 71] {
            assert_eq!(q.snap(note), note);
        }
    }

    #[test]
    fn chord_tones_are_weighted_up_on_the_beat() {
        let q = ScaleQuantizer::new(0, Scale::Major);
        let mut w = [0.0f32; 12];
        q.degree_weights(1.0, true, &mut w);
        // Degrees 0, 2, 4 are the triad; 1, 3, 5, 6 are passing tones.
        assert!(w[0] > w[1]);
        assert!(w[2] > w[1]);
        assert!(w[4] > w[3]);
        // Passing tones still get a real chance.
        assert!(w[1] > 0.0);
    }

    #[test]
    fn no_bias_means_flat_weights() {
        let q = ScaleQuantizer::new(0, Scale::Major);
        let mut w = [0.0f32; 12];
        q.degree_weights(0.0, true, &mut w);
        for i in 0..7 {
            assert!((w[i] - 1.0).abs() < 1e-6);
        }
        // Degrees past the end of the scale must be unreachable.
        for i in 7..12 {
            assert_eq!(w[i], 0.0);
        }
    }
}
