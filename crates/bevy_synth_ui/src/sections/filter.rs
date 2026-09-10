//! The state-variable filter and its response curve.

use egui::{Ui, Vec2};
use bevy_synth::Synth;
use synth_core::filter::{Slope, SvfMode};

use crate::widgets::{self, palette, KnobSpec};
use crate::selector;

pub(crate) fn filter_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "FILTER", palette::FILTER, |ui| {
        widgets::filter_display(
            ui,
            Vec2::new(300.0, 74.0),
            SvfMode::from_u32(p.filter_mode.get()),
            Slope::from_u32(p.filter_slope.get()),
            p.cutoff.get(),
            p.resonance.get(),
        );
        ui.add_space(3.0);
        selector::<SvfMode>(ui, &p.filter_mode);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Slope")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            selector::<Slope>(ui, &p.filter_slope);
        });
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Cutoff", 20.0..=20000.0)
                    .log()
                    .colour(palette::FILTER)
                    .unit("Hz")
                    .default(2000.0),
                &p.cutoff,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Reso", 0.0..=1.0)
                    .colour(palette::FILTER)
                    .default(0.25),
                &p.resonance,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Env amt", -6.0..=6.0)
                    .colour(palette::FILTER)
                    .unit("oct")
                    .default(2.0),
                &p.filter_env_amount,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Key trk", 0.0..=1.0)
                    .colour(palette::FILTER)
                    .default(0.35),
                &p.filter_key_track,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Vel", 0.0..=1.0)
                    .colour(palette::FILTER)
                    .default(0.4),
                &p.filter_velocity,
            );
        });
    });
}
