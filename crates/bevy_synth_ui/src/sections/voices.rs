//! Voice allocation, glide and the pitch/mod wheels.

use egui::Ui;
use bevy_synth::Synth;
use synth_core::params::VoiceMode;

use crate::widgets::{self, palette, KnobSpec};
use crate::{integer, selector};

pub(crate) fn voice_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "VOICES", palette::OSC, |ui| {
        selector::<VoiceMode>(ui, &p.voice_mode);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Max")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            integer(ui, &p.max_voices, 1..=32, "");

            let mut legato = p.legato.get();
            if ui
                .checkbox(&mut legato, "Legato")
                .on_hover_text("overlapping notes glide instead of retriggering")
                .changed()
            {
                p.legato.set(legato);
            }
        });
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Glide", 0.0..=2.0)
                    .colour(palette::OSC)
                    .unit("s")
                    .default(0.0),
                &p.glide,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Bend", -12.0..=12.0)
                    .colour(palette::OSC)
                    .unit("st")
                    .default(0.0),
                &p.pitch_bend,
            );
        });
    });
}
