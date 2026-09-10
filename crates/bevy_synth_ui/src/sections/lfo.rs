//! The low-frequency oscillator and its destination.

use egui::Ui;
use bevy_synth::Synth;
use synth_core::lfo::{LfoTarget, LfoWave};

use crate::widgets::{self, palette, KnobSpec};
use crate::dropdown;

pub(crate) fn lfo_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "LFO", palette::LFO, |ui| {
        dropdown::<LfoWave>(ui, "lfo_wave", &p.lfo_wave);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("To")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            dropdown::<LfoTarget>(ui, "lfo_target", &p.lfo_target);
        });
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Rate", 0.05..=40.0)
                    .log()
                    .colour(palette::LFO)
                    .unit("Hz")
                    .default(5.0),
                &p.lfo_rate,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Depth", 0.0..=1.0)
                    .colour(palette::LFO)
                    .default(0.0),
                &p.lfo_depth,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Mod whl", 0.0..=1.0)
                    .colour(palette::LFO)
                    .default(0.0),
                &p.mod_wheel,
            );
        });
        let mut retrigger = p.lfo_retrigger.get();
        if ui
            .checkbox(&mut retrigger, "Retrigger per note")
            .on_hover_text("restart the LFO on every note instead of free-running")
            .changed()
        {
            p.lfo_retrigger.set(retrigger);
        }
    });
}
