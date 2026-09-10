//! The bassline: its voice, its own line, and its own generator.

use egui::Ui;

use bevy_synth::{Synth, SynthTelemetry};
use synth_core::Waveform;

use crate::widgets::{self, BassEdit, KnobSpec, palette};
use crate::SynthUi;
use crate::sections::sequencer::rand_seed;

pub(crate) fn bass(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, _state: &mut SynthUi) {
    let p = &synth.params;

    widgets::section(ui, "BASS", palette::BASS, |ui| {
        let mut enabled = p.bass_enabled.get();
        if ui
            .checkbox(&mut enabled, "Play bassline")
            .on_hover_text("the bass is off until you ask for it, so adding it never changes an existing patch")
            .changed()
        {
            p.bass_enabled.set(enabled);
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Wave");
            // Two of them, not five: a 303 is a saw or a square and nothing
            // else, and the other three would only be there to be wrong.
            let current = Waveform::from_u32(p.bass.wave.get());
            for (wave, name) in [(Waveform::Saw, "Saw"), (Waveform::Pulse, "Pulse")] {
                if ui.selectable_label(current == wave, name).clicked() {
                    p.bass.wave.set(wave as u32);
                }
            }
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Tune", -12.0..=12.0)
                    .colour(palette::BASS)
                    .default(0.0)
                    .size(36.0),
                &p.bass.tune,
            )
            .on_hover_text("transpose, in semitones");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Cutoff", 20.0..=20000.0)
                    .log()
                    .colour(palette::BASS)
                    .default(300.0)
                    .size(36.0),
                &p.bass.cutoff,
            )
            .on_hover_text("where the filter sits before the envelope opens it");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Reso", 0.0..=1.0)
                    .colour(palette::BASS)
                    .default(0.7)
                    .size(36.0),
                &p.bass.resonance,
            )
            .on_hover_text("high is the point of this instrument");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Env Mod", 0.0..=6.0)
                    .colour(palette::BASS)
                    .default(3.0)
                    .size(36.0),
                &p.bass.env_mod,
            )
            .on_hover_text("how far the envelope opens the filter, in octaves");
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Decay", 0.02..=2.0)
                    .log()
                    .colour(palette::BASS)
                    .default(0.3)
                    .size(36.0),
                &p.bass.decay,
            )
            .on_hover_text("how long the filter sweep takes; the single most important knob");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Accent", 0.0..=1.0)
                    .colour(palette::ACCENT)
                    .default(0.5)
                    .size(36.0),
                &p.bass.accent,
            )
            .on_hover_text("how much an accented step adds: level, brightness and envelope depth together");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Slide", 0.01..=0.5)
                    .log()
                    .colour(palette::ACCENT)
                    .default(0.06)
                    .size(36.0),
                &p.bass.slide_time,
            )
            .on_hover_text("how long a tied note takes to reach its new pitch");
        });

        ui.add_space(4.0);
        ui.separator();

        let pattern = synth.bass_pattern();
        if let Some((index, edit)) = widgets::bass_step_grid(
            ui,
            &pattern,
            telemetry.bass_step as usize,
            p.seq_playing.get(),
        ) {
            match edit {
                BassEdit::Toggle => synth.toggle_bass_step(index),
                BassEdit::Slide => synth.toggle_bass_slide(index),
                BassEdit::Accent => synth.toggle_bass_accent(index),
            };
        }
        ui.label(
            egui::RichText::new("click a step to toggle it; the left tab under it ties, the right accents")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        ui.add_space(4.0);
        ui.separator();
        ui.label(
            egui::RichText::new("GENERATOR")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );
        ui.label(
            egui::RichText::new("key and scale come from the sequencer page")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        ui.horizontal(|ui| {
            ui.label("Octave");
            crate::integer(ui, &p.bass_gen_octave, 0..=7, "");
            ui.label("Range");
            crate::integer(ui, &p.bass_gen_range, 1..=4, " oct");
            ui.label("Length");
            crate::integer(ui, &p.bass_seq_length, 1..=64, " steps");
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Density", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.85)
                    .size(36.0),
                &p.bass_gen_density,
            )
            .on_hover_text("how often a step has a note rather than a rest");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Jump", 1.0..=12.0)
                    .colour(palette::SEQ)
                    .default(5.0)
                    .size(36.0),
                &p.bass_gen_max_jump,
            )
            .on_hover_text("how far the line may leap, in scale degrees");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Chord", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.8)
                    .size(36.0),
                &p.bass_gen_chord_bias,
            )
            .on_hover_text("how strongly downbeats land on root, third and fifth");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Slides", 0.0..=1.0)
                    .colour(palette::ACCENT)
                    .default(0.25)
                    .size(36.0),
                &p.bass_slide_chance,
            )
            .on_hover_text("how often the generator ties a step to the one before it");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Accents", 0.0..=1.0)
                    .colour(palette::ACCENT)
                    .default(0.3)
                    .size(36.0),
                &p.bass_accent_chance,
            )
            .on_hover_text("how often it accents a step off the downbeat");

            ui.vertical(|ui| {
                ui.add_space(8.0);
                if ui
                    .button("↻  New bassline")
                    .on_hover_text("write a fresh bass line from these settings")
                    .clicked()
                {
                    synth.regenerate_bass_with_seed(rand_seed());
                }
                ui.label(
                    egui::RichText::new(format!(
                        "seed {:#x}",
                        p.bass_gen_seed.load(std::sync::atomic::Ordering::Relaxed)
                    ))
                    .color(palette::TEXT_DIM)
                    .size(9.0),
                );
            });
        });
    });
}
