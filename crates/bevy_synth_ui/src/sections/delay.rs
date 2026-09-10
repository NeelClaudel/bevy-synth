//! The delay line.

use egui::Ui;
use bevy_synth::Synth;
use synth_core::NoteDivision;

use crate::widgets::{self, palette, KnobSpec};
use crate::dropdown;

pub(crate) fn delay_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "DELAY", palette::FX, |ui| {
        let mut sync = p.delay_sync.get();
        if ui
            .checkbox(&mut sync, "Sync to tempo")
            .on_hover_text("Lock the repeats to the sequencer clock, internal or MIDI")
            .changed()
        {
            p.delay_sync.set(sync);
        }

        ui.horizontal(|ui| {
            // Time and Division share one slot: only one of them is doing
            // anything at a time, and showing both invites the user to set the
            // one that is being ignored.
            if sync {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("Division")
                            .size(10.0)
                            .color(palette::TEXT_DIM),
                    );
                    dropdown::<NoteDivision>(ui, "delay_division", &p.delay_division);
                });
            } else {
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Time", 0.001..=2.0)
                        .log()
                        .colour(palette::FX)
                        .unit("s")
                        .default(0.375),
                    &p.delay_time,
                );
            }

            widgets::knob_param(
                ui,
                &KnobSpec::new("Feedback", 0.0..=0.95)
                    .colour(palette::FX)
                    .default(0.35),
                &p.delay_feedback,
            );
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Damping", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(0.3),
                &p.delay_damping,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Mix", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(0.0),
                &p.delay_mix,
            );
        });

        let mut ping_pong = p.delay_ping_pong.get();
        if ui
            .checkbox(&mut ping_pong, "Ping pong")
            .on_hover_text("Repeats alternate between the speakers")
            .changed()
        {
            p.delay_ping_pong.set(ping_pong);
        }
    });
}
