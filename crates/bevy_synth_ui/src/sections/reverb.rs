//! The reverb.

use egui::Ui;
use bevy_synth::Synth;

use crate::widgets::{self, palette, KnobSpec};

pub(crate) fn reverb_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "REVERB", palette::FX, |ui| {
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Size", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(0.5),
                &p.reverb_size,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Damping", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(0.5),
                &p.reverb_damping,
            );
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Pre-delay", 0.0..=0.25)
                    .colour(palette::FX)
                    .unit("s")
                    .default(0.02),
                &p.reverb_predelay,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Width", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(1.0),
                &p.reverb_width,
            );
        });

        widgets::knob_param(
            ui,
            &KnobSpec::new("Mix", 0.0..=1.0)
                .colour(palette::FX)
                .default(0.0),
            &p.reverb_mix,
        );
    });
}
