//! The transport strip: play, tempo, clock source and the output meters.

use egui::{Ui, Vec2};
use bevy_synth::{Synth, SynthTelemetry};
use synth_core::params::ClockSource;

use crate::widgets::{self, palette, KnobSpec};
use crate::{SynthUi, dropdown};

pub(crate) fn transport(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi) {
    widgets::section(ui, "TRANSPORT", palette::ACCENT, |ui| {
        ui.horizontal(|ui| {
            let playing = synth.params.seq_playing.get();

            if ui
                .selectable_label(playing, if playing { "■ Stop" } else { "▶ Play" })
                .clicked()
            {
                if playing {
                    synth.stop();
                } else {
                    synth.play();
                }
            }

            if ui.button("Panic").on_hover_text("cut all sound now").clicked() {
                synth.panic();
                state.sounding.clear();
                state.held_from_piano = None;
            }

            ui.separator();

            ui.label("Clock");
            dropdown::<ClockSource>(ui, "clock_source", &synth.params.clock_source);

            // A tempo box is pointless when a DAW is driving: show what is
            // actually arriving instead of a number that has no effect.
            let external =
                ClockSource::from_u32(synth.params.clock_source.get()) == ClockSource::ExternalMidi;
            ui.add_enabled_ui(!external, |ui| {
                let mut tempo = synth.params.tempo.get();
                if ui
                    .add(
                        egui::DragValue::new(&mut tempo)
                            .range(20.0..=300.0)
                            .suffix(" BPM")
                            .speed(0.5),
                    )
                    .changed()
                {
                    synth.params.tempo.set(tempo);
                }
            });
            if external {
                ui.label(
                    egui::RichText::new("following MIDI clock")
                        .color(palette::TEXT_DIM)
                        .size(10.0),
                );
            }

            ui.separator();

            ui.label("Steps/beat");
            let mut steps_per_beat = synth.params.steps_per_beat.get();
            if ui
                .add(
                    egui::DragValue::new(&mut steps_per_beat)
                        .range(1.0..=8.0)
                        .speed(0.05)
                        .fixed_decimals(0),
                )
                .changed()
            {
                synth.params.steps_per_beat.set(steps_per_beat.round());
            }

            ui.separator();

            ui.label(
                egui::RichText::new(format!("{} voices", telemetry.active_voices))
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );

            if !synth.is_running() {
                ui.label(
                    egui::RichText::new("no audio device")
                        .color(palette::DANGER)
                        .size(10.0),
                );
            }
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Master", 0.0..=1.5)
                    .colour(palette::ACCENT)
                    .default(0.5)
                    .size(36.0),
                &synth.params.master_gain,
            )
            .on_hover_text("level of the whole mix, synth and drums together");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Synth", 0.0..=1.5)
                    .colour(palette::ACCENT)
                    .default(1.0)
                    .size(36.0),
                &synth.params.synth_gain,
            )
            .on_hover_text("level of the melody bus alone, under the master");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Drive", 0.5..=10.0)
                    .log()
                    .colour(palette::ACCENT)
                    .default(1.0)
                    .size(36.0),
                &synth.params.drive,
            );
            ui.vertical(|ui| {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("output")
                        .color(palette::TEXT_DIM)
                        .size(9.0),
                );
                widgets::level_meter(
                    ui,
                    Vec2::new(220.0, 12.0),
                    telemetry.peak,
                    &mut state.meter_hold,
                );
            });
        });
    });
}
