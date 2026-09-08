//! A ring buffer for delayed reads.
//!
//! Both effects are ultimately made of these. The capacity is rounded up to a
//! power of two so the wrap is a bitmask rather than a modulo or a branch —
//! this runs once per sample per line, and the reverb has ten of them.

/// Denormals cost up to 100x on some CPUs, and a recirculating feedback path
/// — which is what every delay line in `fx/` ultimately is — produces them
/// constantly on the way to zero, sometimes settling into a subnormal fixed
/// point that never reaches zero on its own. Flush to zero, matching the
/// convention in `filter.rs`.
#[inline]
pub(crate) fn flush(x: f32) -> f32 {
    if x.abs() < 1e-20 {
        0.0
    } else {
        x
    }
}

/// A delay line with integer and fractional reads.
///
/// The convention throughout: `read(0)` returns the sample most recently
/// written, `read(n)` the one `n` samples before it.
pub struct DelayLine {
    buffer: Vec<f32>,
    /// `capacity - 1`. Since capacity is a power of two, `index & mask` is the
    /// wrap.
    mask: usize,
    /// Where the *next* write goes.
    write: usize,
}

impl DelayLine {
    /// Allocates a line that can serve reads up to and including `max_delay`.
    ///
    /// Not real-time safe: call from the setup path.
    pub fn new(max_delay: usize) -> Self {
        // `+ 1` so `read(max_delay)` is in range, not one past the end.
        let capacity = (max_delay + 1).max(2).next_power_of_two();
        Self {
            buffer: vec![0.0; capacity],
            mask: capacity - 1,
            write: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Zeroes the line. Used when the sample rate changes or a patch loads, so
    /// stale audio from the old settings does not leak out.
    pub fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }

    #[inline]
    pub fn write(&mut self, value: f32) {
        // Every delay line in both effects funnels through here, so flushing
        // at this single point closes the whole family at once: the tank
        // delays, the all-passes, the diffusers, the pre-delay.
        self.buffer[self.write] = flush(value);
        self.write = (self.write + 1) & self.mask;
    }

    /// Reads `delay` samples into the past. Clamped, never wrapped: asking for
    /// more history than the line holds gives you the oldest sample rather
    /// than a recent one wearing a disguise.
    #[inline]
    pub fn read(&self, delay: usize) -> f32 {
        let delay = delay.min(self.mask);
        // The most recent sample sits one behind the write cursor.
        self.buffer[(self.write + self.buffer.len() - delay - 1) & self.mask]
    }

    /// Reads at a fractional position, interpolating linearly between the two
    /// neighbouring samples.
    ///
    /// Linear interpolation, not something higher order: it costs one multiply
    /// and its high-frequency loss is a mild darkening of the repeats, which is
    /// what a tape delay does anyway.
    ///
    /// Delay values are clamped toward the oldest history: infinity clamps to
    /// `mask`, negative infinity to 0, and NaN to 0 (the newest sample).
    #[inline]
    pub fn read_frac(&self, delay: f32) -> f32 {
        let delay = if delay.is_nan() {
            0.0
        } else {
            delay.clamp(0.0, self.mask as f32)
        };
        let index = delay.floor();
        let frac = delay - index;
        let index = index as usize;
        let a = self.read(index);
        let b = self.read(index + 1);
        a + (b - a) * frac
    }
}

#[cfg(test)]
impl DelayLine {
    /// Peak absolute value currently stored in the line. Test-only: lets the
    /// denormal-flush regression test in `reverb.rs` see past a `PlateReverb`
    /// with no other public window into its recirculating state.
    pub(crate) fn peak_abs(&self) -> f32 {
        self.buffer.iter().fold(0.0_f32, |peak, &x| peak.max(x.abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_a_power_of_two_that_holds_the_requested_delay() {
        let line = DelayLine::new(1000);
        assert!(line.capacity().is_power_of_two());
        assert!(line.capacity() > 1000);
    }

    #[test]
    fn an_integer_read_returns_the_sample_written_there() {
        let mut line = DelayLine::new(16);
        line.write(1.0);
        line.write(2.0);
        line.write(3.0);
        assert_eq!(line.read(0), 3.0);
        assert_eq!(line.read(1), 2.0);
        assert_eq!(line.read(2), 1.0);
    }

    #[test]
    fn a_fractional_read_interpolates_between_neighbours() {
        let mut line = DelayLine::new(16);
        line.write(1.0);
        line.write(2.0);
        line.write(3.0);
        // Halfway between read(1) = 2.0 and read(2) = 1.0.
        assert_eq!(line.read_frac(1.5), 1.5);
        // The integer positions still agree with `read`.
        assert_eq!(line.read_frac(0.0), 3.0);
        assert_eq!(line.read_frac(2.0), 1.0);
    }

    #[test]
    fn reads_stay_correct_after_the_write_cursor_wraps() {
        let mut line = DelayLine::new(8);
        let capacity = line.capacity();
        // Three full laps of the buffer.
        for i in 0..capacity * 3 {
            line.write(i as f32);
        }
        let last = (capacity * 3 - 1) as f32;
        assert_eq!(line.read(0), last);
        assert_eq!(line.read(1), last - 1.0);
        assert_eq!(line.read(capacity - 1), last - (capacity - 1) as f32);
    }

    #[test]
    fn a_read_older_than_the_buffer_clamps_instead_of_aliasing() {
        let mut line = DelayLine::new(8);
        let capacity = line.capacity();
        for i in 0..capacity {
            line.write(i as f32);
        }
        let oldest = line.read(capacity - 1);
        // Asking for more history than exists must not silently wrap around to
        // a recent sample, which would sound like a completely wrong delay.
        assert_eq!(line.read(capacity), oldest);
        assert_eq!(line.read(100_000), oldest);
        assert_eq!(line.read_frac(1.0e9), oldest);
        assert_eq!(line.read_frac(-5.0), line.read(0));
    }

    #[test]
    fn a_fractional_read_with_non_finite_values_clamps_consistently() {
        let mut line = DelayLine::new(8);
        let capacity = line.capacity();
        // Write distinguishable values so tests can't accidentally pass on equal samples.
        for i in 0..capacity {
            line.write((i as f32) * 10.0);
        }
        let newest = line.read(0);
        let oldest = line.read(capacity - 1);
        // Positive infinity should clamp to the oldest sample, not to newest.
        assert_eq!(line.read_frac(f32::INFINITY), oldest);
        // Negative infinity should clamp to the newest sample.
        assert_eq!(line.read_frac(f32::NEG_INFINITY), newest);
        // NaN should clamp to the newest sample and not propagate.
        assert_eq!(line.read_frac(f32::NAN), newest);
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut line = DelayLine::new(8);
        for _ in 0..8 {
            line.write(1.0);
        }
        line.clear();
        assert_eq!(line.read(0), 0.0);
        assert_eq!(line.read(4), 0.0);
    }
}
