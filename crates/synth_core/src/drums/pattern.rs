//! The drum grid: 64 columns of eight cells, and the bit packing that gets a
//! column across the thread boundary in one word.

use crate::drums::voice::PAD_COUNT;
use crate::sequencer::MAX_STEPS;

/// One pad at one step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    /// False means the pad does not fire here.
    pub active: bool,
    /// Strike level, 0.125 to 1.0. Quantised to eight levels by the packing,
    /// which is three more than ghost/normal/accent needs.
    pub velocity: f32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            active: false,
            // Level 5 of the eight the packing quantises to. A default that
            // sits between two levels does not survive its own round trip:
            // 0.8 came back as 0.75, so the value a virgin cell reported and
            // the value it kept were different numbers.
            velocity: 0.75,
        }
    }
}

/// Every pad at one step.
pub type Column = [Cell; PAD_COUNT];

/// Bit 3 of a pad's nibble is the active flag; bits 0-2 are the velocity.
const ACTIVE_BIT: u32 = 0b1000;
const LEVEL_MASK: u32 = 0b0111;
const NIBBLE: u32 = 4;

/// Packs a column into a single `u32`: eight pads, four bits each.
///
/// This is what makes the grid publishable without a lock. One atomic store
/// per column means a reader can never see half a column, so the UI needs no
/// synchronisation at all.
pub fn pack_column(column: &Column) -> u32 {
    let mut bits = 0;
    for (index, cell) in column.iter().enumerate() {
        // Eight levels spanning 0.125..=1.0, so level 7 is full velocity.
        let level = (cell.velocity.clamp(0.0, 1.0) * 8.0 - 1.0).round();
        let level = (level.max(0.0) as u32) & LEVEL_MASK;
        // The level is written whether or not the cell fires, exactly as
        // `pack_step` writes the melodic velocity regardless of `active`.
        // Skipping it would zero the nibble of every silent cell, so toggling
        // one off and on again came back at the default rather than at the
        // level it was programmed at.
        let active = if cell.active { ACTIVE_BIT } else { 0 };
        bits |= (active | level) << (index as u32 * NIBBLE);
    }
    bits
}

/// Reverses [`pack_column`]. A silent cell comes back at the level it was
/// programmed at, not at the default: that is what lets the UI toggle a cell
/// off and on again without losing the velocity somebody set.
pub fn unpack_column(bits: u32) -> Column {
    let mut column = Column::default();
    for (index, cell) in column.iter_mut().enumerate() {
        let nibble = (bits >> (index as u32 * NIBBLE)) & 0b1111;
        cell.active = nibble & ACTIVE_BIT != 0;
        cell.velocity = ((nibble & LEVEL_MASK) + 1) as f32 / 8.0;
    }
    column
}

/// The whole grid, as a plain value.
///
/// Returned by copy for the same reason [`crate::sequencer::Pattern`] is: the
/// real grid lives on the audio thread, and lending a reference to it would
/// need a lock. Four kilobytes on the stack is far cheaper than that lock,
/// and the UI gets a snapshot that cannot change underneath it mid-frame.
#[derive(Debug, Clone, Copy)]
pub struct DrumPattern {
    columns: [Column; MAX_STEPS],
    len: usize,
}

impl DrumPattern {
    pub fn new(columns: [Column; MAX_STEPS], len: usize) -> Self {
        Self {
            columns,
            len: len.min(MAX_STEPS),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn set_len(&mut self, len: usize) {
        self.len = len.min(MAX_STEPS);
    }

    /// Out-of-range indices return an inactive cell rather than panicking:
    /// some callers are on the audio thread, where a wrong cell is survivable
    /// and a panic is not.
    pub fn get(&self, step: usize, pad: usize) -> Cell {
        self.columns
            .get(step)
            .and_then(|column| column.get(pad))
            .copied()
            .unwrap_or_default()
    }

    pub fn set(&mut self, step: usize, pad: usize, cell: Cell) {
        if let Some(slot) = self
            .columns
            .get_mut(step)
            .and_then(|column| column.get_mut(pad))
        {
            *slot = cell;
        }
    }

    pub fn toggle(&mut self, step: usize, pad: usize) {
        let mut cell = self.get(step, pad);
        cell.active = !cell.active;
        self.set(step, pad, cell);
    }
}

impl core::ops::Deref for DrumPattern {
    type Target = [Column];
    fn deref(&self) -> &[Column] {
        &self.columns[..self.len]
    }
}

impl Default for DrumPattern {
    fn default() -> Self {
        Self {
            columns: [Column::default(); MAX_STEPS],
            len: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Does anything inside the active length fire? Only the tests ask, so it
    /// lives here rather than on the pattern.
    fn any_active(grid: &DrumPattern) -> bool {
        (0..grid.len()).any(|step| (0..PAD_COUNT).any(|pad| grid.get(step, pad).active))
    }

    /// The whole point of the packing: a column survives the round trip
    /// through a single `u32`, which is what crosses the thread boundary.
    #[test]
    fn a_column_survives_the_round_trip() {
        let mut column = Column::default();
        column[0] = Cell { active: true, velocity: 1.0 };
        column[2] = Cell { active: true, velocity: 0.5 };
        column[7] = Cell { active: false, velocity: 0.75 };

        let back = unpack_column(pack_column(&column));

        assert!(back[0].active && back[2].active);
        assert!(!back[1].active && !back[7].active);
        // Velocity quantises to eight levels, so allow half a level of drift.
        assert!((back[0].velocity - 1.0).abs() < 0.07);
        assert!((back[2].velocity - 0.5).abs() < 0.07);
    }

    /// A silent cell still carries a level. The UI toggles cells off and on
    /// constantly, and every one of those round trips goes through the packing,
    /// so a velocity dropped here is a velocity the player loses for good.
    #[test]
    fn a_silent_cell_keeps_the_velocity_it_was_programmed_at() {
        let mut column = Column::default();
        column[0] = Cell { active: true, velocity: 1.0 };
        column[1] = Cell { active: false, velocity: 0.25 };
        column[2] = Cell { active: true, velocity: 0.375 };
        column[3] = Cell { active: false, velocity: 0.875 };
        column[4] = Cell { active: false, velocity: 0.125 };

        let back = unpack_column(pack_column(&column));

        for (index, (before, after)) in column.iter().zip(back.iter()).enumerate() {
            assert_eq!(after, before, "cell {index} did not survive the round trip");
        }
    }

    /// The default has to be one of the eight levels the packing can hold, or
    /// a cell nobody has touched reports one velocity and stores another.
    #[test]
    fn the_default_velocity_is_a_representable_level() {
        let mut column = Column::default();
        column[0].active = true;

        let back = unpack_column(pack_column(&column));

        assert_eq!(back[0].velocity, Cell::default().velocity);
        assert_eq!(back[1], Cell::default());
    }

    /// Every level has to be reachable, or the quantiser has an off-by-one.
    #[test]
    fn every_velocity_level_round_trips() {
        for level in 0..8u32 {
            let velocity = (level + 1) as f32 / 8.0;
            let mut column = Column::default();
            column[3] = Cell { active: true, velocity };

            let back = unpack_column(pack_column(&column));
            assert!(
                (back[3].velocity - velocity).abs() < 1.0e-6,
                "level {level} came back as {}",
                back[3].velocity
            );
        }
    }

    /// An untouched grid fires nothing, and a toggle is its own inverse.
    #[test]
    fn toggling_a_cell_turns_it_on_and_off_again() {
        let mut grid = DrumPattern::default();
        grid.set_len(16);
        assert!(!any_active(&grid));

        grid.toggle(0, 0);
        assert!(grid.get(0, 0).active);
        assert!(any_active(&grid));

        grid.toggle(0, 0);
        assert!(!grid.get(0, 0).active);
        assert!(!any_active(&grid));
    }

    /// Out-of-range indices come from the UI and from packed data. They must
    /// not panic: some of these calls happen on the audio thread.
    #[test]
    fn out_of_range_access_is_inert() {
        let mut grid = DrumPattern::default();
        grid.set(999, 0, Cell { active: true, velocity: 1.0 });
        grid.set(0, 99, Cell { active: true, velocity: 1.0 });
        grid.toggle(999, 99);
        assert!(!any_active(&grid));
        assert!(!grid.get(999, 99).active);
        assert_eq!(DrumPattern::default().len(), 0);
    }

    /// `len` is the one guard between a corrupted length and an out-of-bounds
    /// slice on the audio thread, so both `new` and `set_len` must clamp it —
    /// exactly at `MAX_STEPS` and past it, not just for values that already fit.
    #[test]
    fn length_is_clamped_to_max_steps() {
        let columns = [Column::default(); MAX_STEPS];

        let at_max = DrumPattern::new(columns, MAX_STEPS);
        assert_eq!(at_max.len(), MAX_STEPS);
        assert_eq!(at_max.iter().count(), MAX_STEPS);

        let over_max = DrumPattern::new(columns, 1000);
        assert_eq!(over_max.len(), MAX_STEPS);
        assert_eq!(over_max.iter().count(), MAX_STEPS);

        let mut grid = DrumPattern::default();
        grid.set_len(1000);
        assert_eq!(grid.len(), MAX_STEPS);
        assert_eq!(grid.iter().count(), MAX_STEPS);
    }
}
