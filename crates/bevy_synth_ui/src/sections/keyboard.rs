//! The on-screen keyboard.

use egui::{Ui, Vec2};
use bevy_synth::Synth;

use crate::widgets::{self, palette};
use crate::SynthUi;

pub(crate) fn keyboard(ui: &mut Ui, synth: &Synth, state: &mut SynthUi) {
    widgets::section(ui, "KEYBOARD", palette::OSC, |ui| {
        let (hit, _response) = widgets::piano(
            ui,
            Vec2::new(ui.available_width().min(860.0), 76.0),
            state.keyboard_base,
            state.keyboard_octaves,
            &state.sounding,
        );

        // Compare against what was held last frame. Dragging from one key to
        // the next then releases the old note and starts the new one, which is
        // how a glissando should behave; sending note-on every frame would
        // retrigger the envelope 60 times a second.
        if hit != state.held_from_piano {
            if let Some(previous) = state.held_from_piano.take() {
                synth.note_off(previous);
                state.sounding.retain(|&n| n != previous);
            }
            if let Some(note) = hit {
                synth.note_on(note, 0.85);
                state.sounding.push(note);
                state.held_from_piano = Some(note);
            }
        }

        ui.horizontal(|ui| {
            if ui.small_button("◀ oct").clicked() {
                state.keyboard_base = state.keyboard_base.saturating_sub(12);
            }
            if ui.small_button("oct ▶").clicked() {
                state.keyboard_base = (state.keyboard_base + 12).min(108);
            }
            ui.label(
                egui::RichText::new(format!(
                    "from C{}",
                    (state.keyboard_base as i32 / 12) - 1
                ))
                .color(palette::TEXT_DIM)
                .size(10.0),
            );
        });
    });
}
