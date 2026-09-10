//! The two oscillators, the sub and the noise source.

use egui::Ui;
use bevy_synth::Synth;
use synth_core::Waveform;

use crate::widgets::{self, palette, KnobSpec};
use crate::selector;

pub(crate) fn oscillators(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "OSCILLATORS", palette::OSC, |ui| {
        ui.label(egui::RichText::new("OSC 1").color(palette::TEXT_DIM).size(9.0));
        selector::<Waveform>(ui, &p.osc1_wave);
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Level", 0.0..=1.0)
                    .colour(palette::OSC)
                    .default(0.8),
                &p.osc1_level,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Semis", -24.0..=24.0)
                    .colour(palette::OSC)
                    .unit("st")
                    .default(0.0),
                &p.osc1_semitones,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Detune", -50.0..=50.0)
                    .colour(palette::OSC)
                    .unit("c")
                    .default(0.0),
                &p.osc1_detune,
            );
        });

        ui.add_space(4.0);
        ui.label(egui::RichText::new("OSC 2").color(palette::TEXT_DIM).size(9.0));
        selector::<Waveform>(ui, &p.osc2_wave);
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Level", 0.0..=1.0)
                    .colour(palette::OSC)
                    .default(0.5),
                &p.osc2_level,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Semis", -24.0..=24.0)
                    .colour(palette::OSC)
                    .unit("st")
                    .default(0.0),
                &p.osc2_semitones,
            );
            widgets::knob_param(
                ui,
                // A few cents of detune is where the width comes from, so the
                // useful range is small and deserves the whole knob.
                &KnobSpec::new("Detune", -50.0..=50.0)
                    .colour(palette::OSC)
                    .unit("c")
                    .default(7.0),
                &p.osc2_detune,
            );
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Sub", 0.0..=1.0)
                    .colour(palette::OSC)
                    .default(0.0),
                &p.sub_level,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Noise", 0.0..=1.0)
                    .colour(palette::OSC)
                    .default(0.0),
                &p.noise_level,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Pulse W", 0.05..=0.95)
                    .colour(palette::OSC)
                    .default(0.5),
                &p.pulse_width,
            );
        });
    });
}
