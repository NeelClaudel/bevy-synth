//! The mixer: every level in the signal path, in one place.

use egui::{Ui, Vec2};

use bevy_synth::{Synth, SynthTelemetry};

use crate::SynthUi;
use crate::widgets::{self, KnobSpec, palette};

/// The output stage, as three strips and a meter.
///
/// These knobs used to sit wherever the thing they scaled was edited: master
/// and synth level up in the transport bar, the drum levels at the bottom of
/// the pad controls. That put the two halves of a single decision — how loud
/// are the drums against the synth — two sections and a scroll apart, which is
/// the one comparison the knobs exist to make.
///
/// Drive is here rather than with the oscillators because it belongs to this
/// stage: it saturates the synth bus on the way out, and the drums bypass it.
pub(crate) fn mixer(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi) {
    let p = &synth.params;

    widgets::section(ui, "MIXER", palette::ACCENT, |ui| {
        ui.horizontal(|ui| {
            strip(ui, "master", |ui| {
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Level", 0.0..=1.5)
                        .colour(palette::ACCENT)
                        .default(0.5)
                        .size(36.0),
                    &p.master_gain,
                )
                .on_hover_text("level of the whole mix, synth and drums together");
            });

            ui.separator();
            strip(ui, "synth", |ui| {
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Gain", 0.0..=1.5)
                        .colour(palette::OSC)
                        .default(1.0)
                        .size(36.0),
                    &p.synth_gain,
                )
                .on_hover_text("level of the melody bus alone, under the master");
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Drive", 0.5..=10.0)
                        .log()
                        .colour(palette::OSC)
                        .default(1.0)
                        .size(36.0),
                    &p.drive,
                )
                .on_hover_text("saturation on the synth bus; the drums do not pass through it");
            });

            ui.separator();
            strip(ui, "drums", |ui| {
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Gain", 0.0..=1.5)
                        .colour(palette::DRUM)
                        .default(1.0)
                        .size(36.0),
                    &p.drum_gain,
                )
                .on_hover_text("level of the drum bus in the mix, under the master");
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Bus", 0.0..=1.0)
                        .colour(palette::DRUM)
                        .default(0.8)
                        .size(36.0),
                    &p.drum_level,
                )
                .on_hover_text("trim inside the rack, after the per-pad levels");
            });

            ui.separator();
            ui.vertical(|ui| {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("output")
                        .color(palette::TEXT_DIM)
                        .size(9.0),
                );
                widgets::level_meter(
                    ui,
                    Vec2::new(200.0, 12.0),
                    telemetry.peak,
                    &mut state.meter_hold,
                );
            });
        });
    });
}

/// One captioned group of knobs, so two knobs both called "Gain" are still
/// telling you which bus they are on.
fn strip(ui: &mut Ui, name: &str, knobs: impl FnOnce(&mut Ui)) {
    ui.vertical(|ui| {
        ui.label(
            egui::RichText::new(name)
                .color(palette::TEXT_DIM)
                .size(9.0),
        );
        ui.horizontal(|ui| knobs(ui));
    });
}
