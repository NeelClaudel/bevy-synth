//! The two compressors: one across the synth bus, one across the master.

use egui::Ui;

use bevy_synth::{Synth, SynthTelemetry};
use synth_core::params::{SharedCompressor, SidechainSource};

use crate::selector;
use crate::widgets::{self, KnobSpec, palette};

/// Both compressors, stacked, in one section.
///
/// They are the same six controls twice, so they share a drawing function and
/// differ only in caption and colour. Keeping them together rather than
/// filing each next to the bus it compresses is deliberate: the decision you
/// are making is how much of the squashing happens per-instrument and how
/// much on the sum, and that is a comparison, not two settings.
pub(crate) fn compressor_section(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry) {
    let p = &synth.params;
    widgets::section(ui, "COMPRESSOR", palette::FX, |ui| {
        one(ui, "synth bus", palette::OSC, &p.comp_synth, telemetry.comp_synth_gr);
        ui.add_space(6.0);
        ui.separator();
        one(ui, "master", palette::ACCENT, &p.comp_master, telemetry.comp_master_gr);
    });
}

/// One compressor: a bypass, six knobs' worth of settings and a GR readout.
fn one(ui: &mut Ui, name: &str, colour: egui::Color32, c: &SharedCompressor, gr: f32) {
    ui.horizontal(|ui| {
        let mut on = c.on.get();
        if ui.checkbox(&mut on, name).changed() {
            c.on.set(on);
        }

        // A number, not a bar. The reading that matters is "how many dB am I
        // taking off", and a meter that only moves is harder to answer that
        // with than the figure itself.
        ui.label(
            egui::RichText::new(gain_reduction_label(gr))
                .color(palette::TEXT_DIM)
                .size(10.0)
                .monospace(),
        )
        .on_hover_text("gain reduction being applied right now");
    });

    ui.horizontal(|ui| {
        widgets::knob_param(
            ui,
            &KnobSpec::new("Thresh", -60.0..=0.0)
                .colour(colour)
                .unit("dB")
                .default(-12.0)
                .size(36.0),
            &c.threshold_db,
        )
        .on_hover_text("level above which the compressor starts working");
        widgets::knob_param(
            ui,
            &KnobSpec::new("Ratio", 1.0..=20.0)
                .log()
                .colour(colour)
                .unit(":1")
                .default(4.0)
                .size(36.0),
            &c.ratio,
        );
        widgets::knob_param(
            ui,
            &KnobSpec::new("Makeup", 0.0..=24.0)
                .colour(colour)
                .unit("dB")
                .default(0.0)
                .size(36.0),
            &c.makeup_db,
        )
        .on_hover_text("gain added after the compressor, to put back what it took");
    });

    ui.horizontal(|ui| {
        widgets::knob_param(
            ui,
            &KnobSpec::new("Attack", 0.1..=100.0)
                .log()
                .colour(colour)
                .unit("ms")
                .default(10.0)
                .size(36.0),
            &c.attack_ms,
        );
        widgets::knob_param(
            ui,
            &KnobSpec::new("Release", 5.0..=1000.0)
                .log()
                .colour(colour)
                .unit("ms")
                .default(100.0)
                .size(36.0),
            &c.release_ms,
        );
    });

    ui.label(
        egui::RichText::new("Sidechain")
            .color(palette::TEXT_DIM)
            .size(10.0),
    );
    selector::<SidechainSource>(ui, &c.sidechain);
}

/// The GR readout.
///
/// The compressor reports a positive count of decibels removed; the meter
/// shows it as the negative number a level meter would, and shows nothing at
/// all rather than a rounded "0.0 dB" when it is not working.
fn gain_reduction_label(gr_db: f32) -> String {
    if gr_db < 0.05 {
        "--".to_string()
    } else {
        format!("-{gr_db:.1} dB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reduction_reads_as_a_dash_rather_than_zero_decibels() {
        // "0.0 dB" and "not compressing" look the same at a glance and mean
        // different things; only one of them should be able to appear.
        assert_eq!(gain_reduction_label(0.0), "--");
    }

    #[test]
    fn reduction_reads_as_negative_decibels() {
        // The compressor reports a positive count of dB removed; a meter
        // showing "4.5 dB" reads as a boost.
        assert_eq!(gain_reduction_label(4.5), "-4.5 dB");
    }

    #[test]
    fn a_reduction_too_small_to_see_still_reads_as_a_dash() {
        assert_eq!(gain_reduction_label(0.02), "--");
    }
}
