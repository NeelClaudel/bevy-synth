//! The stereo delay, and the note divisions it can lock to.

/// How long one delay repeat lasts, in musical time.
///
/// Ordered longest to shortest, which is how they read in a dropdown and how
/// a musician thinks about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum NoteDivision {
    Whole = 0,
    Half = 1,
    QuarterDot = 2,
    Quarter = 3,
    EighthDot = 4,
    #[default]
    Eighth = 5,
    EighthTriplet = 6,
    Sixteenth = 7,
    SixteenthTriplet = 8,
}

impl NoteDivision {
    pub const ALL: [NoteDivision; 9] = [
        NoteDivision::Whole,
        NoteDivision::Half,
        NoteDivision::QuarterDot,
        NoteDivision::Quarter,
        NoteDivision::EighthDot,
        NoteDivision::Eighth,
        NoteDivision::EighthTriplet,
        NoteDivision::Sixteenth,
        NoteDivision::SixteenthTriplet,
    ];

    pub fn from_u32(value: u32) -> Self {
        match value {
            0 => NoteDivision::Whole,
            1 => NoteDivision::Half,
            2 => NoteDivision::QuarterDot,
            3 => NoteDivision::Quarter,
            4 => NoteDivision::EighthDot,
            5 => NoteDivision::Eighth,
            6 => NoteDivision::EighthTriplet,
            7 => NoteDivision::Sixteenth,
            8 => NoteDivision::SixteenthTriplet,
            _ => NoteDivision::Eighth,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            NoteDivision::Whole => "1/1",
            NoteDivision::Half => "1/2",
            NoteDivision::QuarterDot => "1/4.",
            NoteDivision::Quarter => "1/4",
            NoteDivision::EighthDot => "1/8.",
            NoteDivision::Eighth => "1/8",
            NoteDivision::EighthTriplet => "1/8T",
            NoteDivision::Sixteenth => "1/16",
            NoteDivision::SixteenthTriplet => "1/16T",
        }
    }

    /// Length in beats, where one beat is a quarter note.
    pub fn beats(self) -> f32 {
        match self {
            NoteDivision::Whole => 4.0,
            NoteDivision::Half => 2.0,
            NoteDivision::QuarterDot => 1.5,
            NoteDivision::Quarter => 1.0,
            NoteDivision::EighthDot => 0.75,
            NoteDivision::Eighth => 0.5,
            NoteDivision::EighthTriplet => 1.0 / 3.0,
            NoteDivision::Sixteenth => 0.25,
            NoteDivision::SixteenthTriplet => 1.0 / 6.0,
        }
    }

    /// Length in seconds at the given tempo.
    ///
    /// The tempo is clamped because it may have come from `Clock::tempo_bpm`,
    /// which reports 0.0 until an external clock has been running long enough
    /// to measure — and 0 BPM means an infinitely long note.
    pub fn seconds(self, tempo_bpm: f32) -> f32 {
        let bpm = if tempo_bpm.is_finite() {
            tempo_bpm.clamp(20.0, 300.0)
        } else {
            120.0
        };
        self.beats() * 60.0 / bpm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divisions_are_the_right_length_at_120_bpm() {
        // At 120 BPM a quarter note is half a second.
        assert_eq!(NoteDivision::Quarter.seconds(120.0), 0.5);
        assert_eq!(NoteDivision::Eighth.seconds(120.0), 0.25);
        assert_eq!(NoteDivision::QuarterDot.seconds(120.0), 0.75);
        assert_eq!(NoteDivision::Whole.seconds(120.0), 2.0);
        assert!((NoteDivision::EighthTriplet.seconds(120.0) - 1.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn division_length_scales_inversely_with_tempo() {
        assert_eq!(NoteDivision::Quarter.seconds(60.0), 1.0);
        assert_eq!(NoteDivision::Quarter.seconds(240.0), 0.25);
    }

    #[test]
    fn an_absurd_tempo_still_gives_a_usable_delay_time() {
        // `Clock::tempo_bpm` returns 0.0 before the first external clock tick
        // arrives. Left alone that is a division by zero and an infinite delay
        // time, so the clamp is load-bearing, not defensive decoration.
        assert!(NoteDivision::Quarter.seconds(0.0).is_finite());
        assert!(NoteDivision::Quarter.seconds(0.0) > 0.0);
        assert!(NoteDivision::Quarter.seconds(f32::NAN).is_finite());
        assert!(NoteDivision::Quarter.seconds(1.0e9).is_finite());
    }

    #[test]
    fn divisions_are_ordered_longest_first() {
        let lengths: Vec<f32> = NoteDivision::ALL.iter().map(|d| d.beats()).collect();
        for pair in lengths.windows(2) {
            assert!(pair[0] > pair[1], "{:?} is not descending", lengths);
        }
    }

    #[test]
    fn every_variant_round_trips_through_u32() {
        for &division in &NoteDivision::ALL {
            assert_eq!(NoteDivision::from_u32(division as u32), division);
            assert!(!division.name().is_empty());
        }
        // Out of range falls back rather than panicking: the value came across
        // an atomic from another thread and cannot be trusted.
        assert_eq!(NoteDivision::from_u32(999), NoteDivision::Eighth);
    }
}
