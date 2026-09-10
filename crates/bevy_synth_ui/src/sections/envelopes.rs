//! The amplitude and filter envelopes.

use egui::{Ui, Vec2};
use bevy_synth::Synth;
use synth_core::env::AdsrSettings;

use crate::widgets::{self, palette, KnobSpec};

pub(crate) fn envelopes(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "ENVELOPES", palette::ENV, |ui| {
        for (title, colour, attack, decay, sustain, release) in [
            (
                "Amplitude",
                palette::ENV,
                &p.amp_attack,
                &p.amp_decay,
                &p.amp_sustain,
                &p.amp_release,
            ),
            (
                "Filter",
                palette::FILTER,
                &p.filter_attack,
                &p.filter_decay,
                &p.filter_sustain,
                &p.filter_release,
            ),
        ] {
            ui.label(egui::RichText::new(title).color(palette::TEXT_DIM).size(9.0));
            widgets::adsr_display(
                ui,
                Vec2::new(300.0, 52.0),
                &AdsrSettings {
                    attack: attack.get(),
                    decay: decay.get(),
                    sustain: sustain.get(),
                    release: release.get(),
                },
                colour,
            );
            ui.horizontal(|ui| {
                // Times are logarithmic: the difference between 1 ms and 10 ms
                // is a whole character of attack, and on a linear knob both sit
                // in the first pixel.
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("A", 0.001..=10.0)
                        .log()
                        .colour(colour)
                        .unit("s")
                        .default(0.005)
                        .size(36.0),
                    attack,
                );
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("D", 0.001..=10.0)
                        .log()
                        .colour(colour)
                        .unit("s")
                        .default(0.25)
                        .size(36.0),
                    decay,
                );
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("S", 0.0..=1.0)
                        .colour(colour)
                        .default(0.7)
                        .size(36.0),
                    sustain,
                );
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("R", 0.001..=10.0)
                        .log()
                        .colour(colour)
                        .unit("s")
                        .default(0.3)
                        .size(36.0),
                    release,
                );
            });
            ui.add_space(4.0);
        }
    });
}
