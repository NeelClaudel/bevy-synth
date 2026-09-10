//! The step sequencer, its pattern slots and the generative controls.

use egui::Ui;
use bevy_synth::{Synth, SynthTelemetry};
use synth_core::Scale;

use crate::widgets::{self, palette, KnobSpec};
use crate::{SLOTS, SynthUi, dropdown, integer};

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

pub(crate) fn sequencer(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi) {
    let p = &synth.params;
    widgets::section(ui, "SEQUENCER", palette::SEQ, |ui| {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                // Bound to a local so the snapshot lives for the whole draw:
                // `Pattern` derefs to a slice, but only while it is alive.
                let pattern = synth.pattern();
                if let Some(index) = widgets::step_grid(
                    ui,
                    &pattern,
                    telemetry.current_step as usize,
                    p.seq_playing.get(),
                ) {
                    synth.toggle_step(index);
                }
            });

            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    widgets::knob_param(
                        ui,
                        &KnobSpec::new("Gate", 0.05..=1.5)
                            .colour(palette::SEQ)
                            .default(0.6)
                            .size(36.0),
                        &p.seq_gate,
                    );
                    widgets::knob_param(
                        ui,
                        &KnobSpec::new("Swing", 0.0..=0.6)
                            .colour(palette::SEQ)
                            .default(0.0)
                            .size(36.0),
                        &p.seq_swing,
                    );
                    ui.vertical(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Length")
                                .color(palette::TEXT_DIM)
                                .size(10.0),
                        );
                        integer(ui, &p.seq_length, 1..=64, " steps");
                    });
                });

                let mut melody = p.melody_enabled.get();
                if ui
                    .checkbox(&mut melody, "Play melody")
                    .on_hover_text("mute the melodic track without stopping the clock")
                    .changed()
                {
                    p.melody_enabled.set(melody);
                }
            });
        });

        ui.add_space(4.0);
        ui.separator();
        pattern_slots(ui, synth, state);

        ui.add_space(4.0);
        ui.separator();
        ui.label(
            egui::RichText::new("GENERATOR")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        ui.horizontal(|ui| {
            ui.label("Key");
            let root = p.gen_root.get() as usize % 12;
            egui::ComboBox::from_id_salt("gen_root")
                .selected_text(NOTE_NAMES[root])
                .width(52.0)
                .show_ui(ui, |ui| {
                    for (index, name) in NOTE_NAMES.iter().enumerate() {
                        if ui.selectable_label(index == root, *name).clicked() {
                            p.gen_root.set(index as u32);
                        }
                    }
                });
            dropdown::<Scale>(ui, "gen_scale", &p.gen_scale);

            ui.label("Octave");
            integer(ui, &p.gen_octave, 0..=7, "");
            ui.label("Range");
            integer(ui, &p.gen_range, 1..=4, " oct");
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Density", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.75)
                    .size(36.0),
                &p.gen_density,
            )
            .on_hover_text("how often a step has a note rather than a rest");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Jump", 1.0..=12.0)
                    .colour(palette::SEQ)
                    .default(3.0)
                    .size(36.0),
                &p.gen_max_jump,
            )
            .on_hover_text("how far the melody may leap, in scale degrees");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Chord", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.6)
                    .size(36.0),
                &p.gen_chord_bias,
            )
            .on_hover_text("how strongly downbeats land on root, third and fifth");

            ui.vertical(|ui| {
                ui.add_space(8.0);
                if ui
                    .button("↻  New pattern")
                    .on_hover_text("write a fresh melody from these settings")
                    .clicked()
                {
                    // A fresh seed each time, so repeated clicks explore rather
                    // than returning the same tune.
                    synth.regenerate_with_seed(rand_seed());
                }
                ui.label(
                    egui::RichText::new(format!(
                        "seed {:#x}",
                        p.gen_seed.load(std::sync::atomic::Ordering::Relaxed)
                    ))
                    .color(palette::TEXT_DIM)
                    .size(9.0),
                );
            });
        });
    });
}

/// A row of saved patterns to switch between.
///
/// The generator is the point of this synth, but it is happy to throw away a
/// good phrase on the next click. A few slots turn "that one was nice" into
/// something you can come back to, and switching between two of them while the
/// sequencer runs is arranging, not just auditioning.
fn pattern_slots(ui: &mut Ui, synth: &Synth, state: &mut SynthUi) {
    let pattern = synth.pattern();
    let has_notes = pattern.iter().any(|step| step.active);

    // The startup pattern is worth keeping without being asked: it is the one
    // the player is listening to when the panel first opens, and losing it to
    // an idle click on Generate is a poor introduction. Waits for a pattern
    // with something in it, since the first frames can arrive before the
    // engine has generated one.
    if has_notes && state.slots.iter().all(Option::is_none) {
        state.slots[0] = Some(pattern);
        state.active_slot = Some(0);
    }

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("PATTERNS")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        for index in 0..SLOTS {
            let filled = state.slots[index].is_some();
            let active = state.active_slot == Some(index);
            let label = egui::RichText::new(format!("{}", index + 1)).size(12.0);
            let button = egui::Button::selectable(active, label);
            let response = ui
                .add_enabled(filled, button)
                .on_hover_text("play this pattern, from the top of the next loop")
                .on_disabled_hover_text("empty - press Save to put the current pattern here");
            if response.clicked() {
                if let Some(saved) = &state.slots[index] {
                    synth.load_pattern(saved);
                    state.active_slot = Some(index);
                }
            }
        }

        ui.add_space(6.0);
        // Filling the next empty slot is what someone auditioning generated
        // patterns wants: press Save whenever one is good, four times over,
        // without first deciding where it goes. Once they are all full, the
        // one being listened to is the one to replace.
        let target = state
            .slots
            .iter()
            .position(Option::is_none)
            .or(state.active_slot)
            .unwrap_or(0);
        let save = ui
            .add_enabled(has_notes, egui::Button::new("Save"))
            .on_hover_text(format!("store the current pattern in slot {}", target + 1));
        if save.clicked() {
            state.slots[target] = Some(pattern);
            state.active_slot = Some(target);
        }
    });
}

/// A seed with no dependency on `rand`.
///
/// The system clock is plenty: this picks a melody, and nothing about it needs
/// to be unpredictable to an adversary.
pub(crate) fn rand_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED)
        // Mix the low bits up: consecutive nanosecond values differ only in the
        // bottom few bits, and the sequencer's RNG is seeded straight from this.
        .wrapping_mul(0x2545_F491_4F6C_DD1D)
}
