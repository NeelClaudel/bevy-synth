//! The drum rack: the grid, the kit knobs and the per-pad controls.

use egui::Ui;
use bevy_synth::{Synth, SynthTelemetry};
use synth_core::{Cell, Pad, PAD_COUNT};

use crate::widgets::{self, palette, KnobSpec};
use crate::{SynthUi, integer};

/// The three velocity levels shift-click cycles through.
///
/// Each is exact in the grid's three-bit packing — 0.375, 0.75 and 1.0 are
/// levels 2, 5 and 7 of eight — so a cycled cell round-trips through the
/// mirror unchanged instead of drifting a step on every edit.
const VELOCITIES: [f32; 3] = [0.375, 0.75, 1.0];

/// The velocity a shift-click on this cell should land on next.
///
/// An inactive cell starts the cycle at its softest step, so the whole range
/// is reachable without a plain click first. An active cell advances to the
/// next of the three levels, wrapping past the loudest back to the softest.
/// A velocity that is not exactly one of the three falls back to the
/// softest rather than panicking or freezing the cycle — see the test below
/// for why that fallback is load-bearing rather than defensive-only.
fn next_velocity(cell: Cell) -> f32 {
    if !cell.active {
        return VELOCITIES[0];
    }
    VELOCITIES
        .iter()
        .position(|v| (*v - cell.velocity).abs() < 0.01)
        .map_or(VELOCITIES[0], |i| VELOCITIES[(i + 1) % VELOCITIES.len()])
}

pub(crate) fn drums(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi) {
    let p = &synth.params;
    widgets::section(ui, "DRUMS", palette::DRUM, |ui| {
        ui.horizontal(|ui| {
            let mut enabled = p.drum_enabled.get();
            if ui
                .checkbox(&mut enabled, "Enable")
                .on_hover_text("off by default, so an existing patch sounds exactly as it did")
                .changed()
            {
                p.drum_enabled.set(enabled);
            }

            ui.label(
                egui::RichText::new("Length")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            integer(ui, &p.drum_length, 1..=64, " steps");
        });

        ui.add_space(4.0);

        let muted: [bool; PAD_COUNT] = core::array::from_fn(|i| p.pad_mute[i].get());
        // Bound to a local so the snapshot lives for the whole draw, and so
        // the shift-click arm below reads the same grid the user clicked on.
        let grid = synth.drum_grid();
        let hit = widgets::drum_grid(
            ui,
            &grid,
            telemetry.drum_step as usize,
            p.seq_playing.get(),
            &muted,
            state.selected_pad,
        );

        match hit {
            Some(widgets::DrumHit::Mute(pad)) => {
                p.pad_mute[pad].set(!muted[pad]);
            }
            Some(widgets::DrumHit::Select(pad)) => {
                state.selected_pad = pad;
            }
            Some(widgets::DrumHit::Cell {
                step,
                pad,
                shift: false,
            }) => {
                synth.toggle_drum_cell(step, pad);
            }
            Some(widgets::DrumHit::Cell {
                step,
                pad,
                shift: true,
            }) => {
                let mut cell = grid.get(step, pad);
                cell.velocity = next_velocity(cell);
                cell.active = true;
                synth.set_drum_cell(step, pad, cell);
            }
            None => {}
        }

        ui.add_space(4.0);
        ui.separator();

        // One pad's controls, chosen by clicking its name in the grid.
        // `DrumHit::Select` never hands back anything outside 0..PAD_COUNT
        // today, but `selected_pad` is plain UI state with no invariant of
        // its own enforcing that — a saved-session field or a future second
        // writer could hand it a stale value, and indexing pad_level/tune/
        // decay with that would panic instead of just showing the wrong pad.
        let pad = state.selected_pad.min(PAD_COUNT - 1);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(Pad::from_u32(pad as u32).name())
                    .color(palette::TEXT)
                    .size(10.0)
                    .strong(),
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Level", 0.0..=1.0)
                    .colour(palette::DRUM)
                    .default(0.8)
                    .size(36.0),
                &p.pad_level[pad],
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Tune", -12.0..=12.0)
                    .colour(palette::DRUM)
                    .unit("st")
                    .default(0.0)
                    .size(36.0),
                &p.pad_tune[pad],
            )
            .on_hover_text("semitones from the pad's own base frequency");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Decay", 0.25..=4.0)
                    .colour(palette::DRUM)
                    .unit("x")
                    .default(1.0)
                    .size(36.0),
                &p.pad_decay[pad],
            )
            .on_hover_text("multiplier on the pad's natural decay, not a time in seconds");
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(active: bool, velocity: f32) -> Cell {
        Cell { active, velocity }
    }

    #[test]
    fn an_inactive_cell_starts_at_the_softest_level() {
        assert_eq!(next_velocity(cell(false, 0.0)), VELOCITIES[0]);
    }

    #[test]
    fn each_level_advances_to_the_next_and_the_last_wraps_to_the_first() {
        assert_eq!(next_velocity(cell(true, VELOCITIES[0])), VELOCITIES[1]);
        assert_eq!(next_velocity(cell(true, VELOCITIES[1])), VELOCITIES[2]);
        assert_eq!(next_velocity(cell(true, VELOCITIES[2])), VELOCITIES[0]);
    }

    #[test]
    fn an_off_table_velocity_still_lands_on_a_valid_level() {
        // Not hypothetical: the grid quantises to eight levels and this cycle
        // visits three of them, so the other five all reach here — set through
        // `Synth::set_drum_cell`, or carried in from a grid written before the
        // cycle existed. Without the fallback a shift-click on such a cell
        // would do nothing, forever.
        let landed = next_velocity(cell(true, 0.625));
        assert!(
            VELOCITIES.contains(&landed),
            "an off-table velocity must fall back onto the cycle, got {landed}"
        );
    }

    /// The cycle and the grid's default have to agree, or the very first
    /// shift-click on an untouched cell jumps somewhere arbitrary instead of
    /// stepping to the next level.
    #[test]
    fn the_grid_default_is_on_the_cycle() {
        assert!(
            VELOCITIES.contains(&Cell::default().velocity),
            "Cell::default().velocity is {}, which the cycle never visits",
            Cell::default().velocity
        );
    }
}
