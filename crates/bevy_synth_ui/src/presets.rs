//! Starting-point patches.
//!
//! A synth panel with forty knobs and no presets is intimidating: there is no
//! way in except turning things and hoping. A handful of recognisable sounds
//! gives you somewhere to start and, more usefully, something to take apart —
//! load "Acid Bass", look at what resonance and filter decay are set to, and
//! the knobs stop being abstract.
//!
//! # What a preset does and does not touch
//!
//! Presets set the *patch*: oscillators, filter, envelopes, LFO, voice mode,
//! output, and effects. They deliberately leave the sequencer and generator
//! alone — tempo, key, scale and pattern length belong to the piece you are
//! writing, not to the sound, and having them reset every time you auditioned
//! a patch would be infuriating.

use bevy_egui::egui;
use egui::Ui;

use bevy_synth::Synth;
use synth_core::filter::{Slope, SvfMode};
use synth_core::lfo::{LfoTarget, LfoWave};
use synth_core::params::{SharedParams, VoiceMode};
use synth_core::{NoteDivision, Waveform};

use crate::widgets::{self, palette};

/// A named patch.
pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    pub apply: fn(&SharedParams),
}

pub const ALL: &[Preset] = &[
    Preset {
        name: "Init",
        description: "One saw, gentle filter. The blank page.",
        apply: init,
    },
    Preset {
        name: "Warm Pad",
        description: "Slow attack, detuned saws, drifting filter.",
        apply: warm_pad,
    },
    Preset {
        name: "Acid Bass",
        description: "Mono, high resonance, short filter decay, glide.",
        apply: acid_bass,
    },
    Preset {
        name: "Pluck",
        description: "Fast decay, filter envelope, no sustain.",
        apply: pluck,
    },
    Preset {
        name: "Brass",
        description: "Slow-ish filter attack, pulse waves, drive.",
        apply: brass,
    },
    Preset {
        name: "Glass Bell",
        description: "Wide detune, bandpass, long release.",
        apply: glass_bell,
    },
    Preset {
        name: "Sub Bass",
        description: "Sine plus sub, lowpass, nothing else.",
        apply: sub_bass,
    },
    Preset {
        name: "Wind",
        description: "Noise through a resonant bandpass, no oscillators.",
        apply: wind,
    },
];

/// The presets section of the panel.
pub fn section(ui: &mut Ui, synth: &Synth) {
    widgets::section(ui, "PRESETS", palette::ACCENT, |ui| {
        egui::Grid::new("presets")
            .num_columns(2)
            .spacing([4.0, 4.0])
            .show(ui, |ui| {
                for (index, preset) in ALL.iter().enumerate() {
                    if ui
                        .button(preset.name)
                        .on_hover_text(preset.description)
                        .clicked()
                    {
                        (preset.apply)(&synth.params);
                    }
                    if index % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
        ui.label(
            egui::RichText::new("patch only — tempo, key and pattern are left alone")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );
    });
}

/// Settings every preset starts from, so a patch never inherits half of the
/// previous one. Without this, loading "Sub Bass" after "Wind" would leave the
/// noise oscillator up and nobody would know why the bass hissed.
fn reset(p: &SharedParams) {
    p.osc1_wave.set(Waveform::Saw as u32);
    p.osc1_level.set(0.8);
    p.osc1_semitones.set(0.0);
    p.osc1_detune.set(0.0);

    p.osc2_wave.set(Waveform::Saw as u32);
    p.osc2_level.set(0.0);
    p.osc2_semitones.set(0.0);
    p.osc2_detune.set(0.0);

    p.pulse_width.set(0.5);
    p.sub_level.set(0.0);
    p.noise_level.set(0.0);

    p.filter_mode.set(SvfMode::Lowpass as u32);
    p.filter_slope.set(Slope::Db24 as u32);
    p.cutoff.set(2000.0);
    p.resonance.set(0.2);
    p.filter_env_amount.set(0.0);
    p.filter_key_track.set(0.35);
    p.filter_velocity.set(0.3);

    p.amp_attack.set(0.005);
    p.amp_decay.set(0.25);
    p.amp_sustain.set(0.7);
    p.amp_release.set(0.3);

    p.filter_attack.set(0.002);
    p.filter_decay.set(0.3);
    p.filter_sustain.set(0.3);
    p.filter_release.set(0.3);

    p.lfo_target.set(LfoTarget::None as u32);
    p.lfo_wave.set(LfoWave::Sine as u32);
    p.lfo_rate.set(5.0);
    p.lfo_depth.set(0.0);
    p.mod_wheel.set(0.0);

    p.voice_mode.set(VoiceMode::Poly as u32);
    p.max_voices.set(16);
    p.glide.set(0.0);
    p.legato.set(true);
    p.pitch_bend.set(0.0);

    p.drive.set(1.0);
    p.master_gain.set(0.5);

    p.delay_mix.set(0.0);
    p.delay_sync.set(false);
    p.delay_time.set(0.375);
    p.delay_division.set(NoteDivision::Eighth as u32);
    p.delay_feedback.set(0.35);
    p.delay_damping.set(0.3);
    p.delay_ping_pong.set(false);

    p.reverb_mix.set(0.0);
    p.reverb_size.set(0.5);
    p.reverb_damping.set(0.5);
    p.reverb_predelay.set(0.02);
    p.reverb_width.set(1.0);
}

fn init(p: &SharedParams) {
    reset(p);
}

fn warm_pad(p: &SharedParams) {
    reset(p);
    p.osc2_level.set(0.7);
    // Two saws a few cents apart beat slowly against each other. That beating
    // is the entire difference between a pad and a buzz.
    p.osc2_detune.set(9.0);
    p.sub_level.set(0.2);

    p.cutoff.set(1400.0);
    p.resonance.set(0.15);
    p.filter_env_amount.set(1.2);
    p.filter_attack.set(1.2);
    p.filter_decay.set(2.0);
    p.filter_sustain.set(0.5);

    p.amp_attack.set(0.9);
    p.amp_decay.set(1.5);
    p.amp_sustain.set(0.8);
    p.amp_release.set(1.8);

    p.lfo_target.set(LfoTarget::Cutoff as u32);
    p.lfo_wave.set(LfoWave::SmoothRandom as u32);
    p.lfo_rate.set(0.2);
    p.lfo_depth.set(0.3);

    p.master_gain.set(0.35);

    p.reverb_mix.set(0.4);
    p.reverb_size.set(0.8);
    p.reverb_damping.set(0.45);
    p.reverb_predelay.set(0.035);
    p.delay_mix.set(0.15);
    p.delay_sync.set(true);
    p.delay_division.set(NoteDivision::Quarter as u32);
    p.delay_feedback.set(0.3);
    p.delay_damping.set(0.6);
}

fn acid_bass(p: &SharedParams) {
    reset(p);
    p.voice_mode.set(VoiceMode::Mono as u32);
    p.legato.set(true);
    // Glide plus legato is what makes the slides between notes; without both
    // it is just a mono bass.
    p.glide.set(0.055);

    p.osc1_wave.set(Waveform::Saw as u32);
    p.sub_level.set(0.3);

    p.cutoff.set(280.0);
    p.resonance.set(0.9);
    p.filter_env_amount.set(3.6);
    p.filter_attack.set(0.001);
    p.filter_decay.set(0.22);
    p.filter_sustain.set(0.0);

    p.amp_attack.set(0.002);
    p.amp_sustain.set(0.9);
    p.amp_release.set(0.08);

    p.drive.set(2.5);
    p.master_gain.set(0.5);
}

fn pluck(p: &SharedParams) {
    reset(p);
    p.osc2_level.set(0.5);
    p.osc2_detune.set(6.0);

    p.cutoff.set(700.0);
    p.resonance.set(0.5);
    p.filter_env_amount.set(3.0);
    p.filter_decay.set(0.18);
    p.filter_sustain.set(0.0);

    p.amp_attack.set(0.002);
    p.amp_decay.set(0.5);
    // Zero sustain means the note dies on its own, and the voice frees itself
    // at the end of decay rather than holding silence.
    p.amp_sustain.set(0.0);
    p.amp_release.set(0.25);

    p.filter_velocity.set(0.6);
    p.master_gain.set(0.5);

    p.delay_mix.set(0.35);
    p.delay_sync.set(true);
    p.delay_division.set(NoteDivision::Eighth as u32);
    p.delay_feedback.set(0.45);
    p.delay_damping.set(0.4);
    p.delay_ping_pong.set(true);
    p.reverb_mix.set(0.18);
    p.reverb_size.set(0.55);
}

fn brass(p: &SharedParams) {
    reset(p);
    p.osc1_wave.set(Waveform::Pulse as u32);
    p.osc2_wave.set(Waveform::Saw as u32);
    p.osc2_level.set(0.6);
    p.osc2_detune.set(-7.0);
    p.pulse_width.set(0.42);

    p.cutoff.set(600.0);
    p.resonance.set(0.35);
    p.filter_env_amount.set(2.6);
    // A filter attack slower than the amplitude attack is what gives brass its
    // characteristic swell into the note.
    p.filter_attack.set(0.09);
    p.filter_decay.set(0.5);
    p.filter_sustain.set(0.55);

    p.amp_attack.set(0.02);
    p.amp_sustain.set(0.85);
    p.amp_release.set(0.25);

    p.drive.set(1.8);
    p.master_gain.set(0.4);
}

fn glass_bell(p: &SharedParams) {
    reset(p);
    p.osc1_wave.set(Waveform::Sine as u32);
    p.osc2_wave.set(Waveform::Triangle as u32);
    p.osc2_level.set(0.6);
    // A wide, inharmonic interval. Bells are inharmonic; a fifth or an octave
    // would just sound like two notes.
    p.osc2_semitones.set(19.0);
    p.osc2_detune.set(4.0);

    p.filter_mode.set(SvfMode::Bandpass as u32);
    p.filter_slope.set(Slope::Db12 as u32);
    p.cutoff.set(2600.0);
    p.resonance.set(0.55);
    p.filter_env_amount.set(1.5);
    p.filter_decay.set(1.2);
    p.filter_sustain.set(0.1);

    p.amp_attack.set(0.001);
    p.amp_decay.set(2.5);
    p.amp_sustain.set(0.0);
    p.amp_release.set(2.0);

    p.master_gain.set(0.45);

    p.reverb_mix.set(0.45);
    p.reverb_size.set(0.85);
    p.reverb_damping.set(0.2);
    p.reverb_predelay.set(0.01);
    p.reverb_width.set(1.0);
}

fn sub_bass(p: &SharedParams) {
    reset(p);
    p.voice_mode.set(VoiceMode::Mono as u32);
    p.osc1_wave.set(Waveform::Sine as u32);
    p.osc1_level.set(0.9);
    p.sub_level.set(0.5);

    p.cutoff.set(320.0);
    p.resonance.set(0.05);
    p.filter_env_amount.set(0.0);
    // No key tracking: a sub should be the same weight at every pitch, not
    // brighter as it goes up.
    p.filter_key_track.set(0.0);

    p.amp_attack.set(0.008);
    p.amp_sustain.set(1.0);
    p.amp_release.set(0.12);

    p.master_gain.set(0.5);
}

fn wind(p: &SharedParams) {
    reset(p);
    // No oscillators at all: the pitch comes entirely from where the resonant
    // bandpass sits, which is what makes it whistle rather than hum.
    p.osc1_level.set(0.0);
    p.osc2_level.set(0.0);
    p.noise_level.set(0.8);

    p.filter_mode.set(SvfMode::Bandpass as u32);
    p.filter_slope.set(Slope::Db24 as u32);
    p.cutoff.set(900.0);
    p.resonance.set(0.85);
    p.filter_key_track.set(1.0);
    p.filter_env_amount.set(0.6);
    p.filter_attack.set(0.6);

    p.amp_attack.set(0.7);
    p.amp_decay.set(1.0);
    p.amp_sustain.set(0.7);
    p.amp_release.set(1.2);

    p.lfo_target.set(LfoTarget::Cutoff as u32);
    p.lfo_wave.set(LfoWave::SmoothRandom as u32);
    p.lfo_rate.set(0.45);
    p.lfo_depth.set(0.5);

    p.master_gain.set(0.4);

    p.reverb_mix.set(0.55);
    p.reverb_size.set(0.95);
    p.reverb_damping.set(0.35);
    p.reverb_predelay.set(0.05);
}

#[cfg(test)]
mod tests {
    use super::*;
    use synth_core::params::Params;

    /// Every preset must leave the synth in a state that actually makes a
    /// sound. A patch with every level at zero is a silent bug that is easy to
    /// ship and hard to notice.
    #[test]
    fn every_preset_produces_audio() {
        use std::sync::Arc;
        use synth_core::event::channel;
        use synth_core::{Engine, Event};

        for preset in ALL {
            let params = Arc::new(SharedParams::default());
            (preset.apply)(&params);
            // Presets are patches; the sequencer stays out of it.
            params.seq_playing.set(false);

            let (events, consumer) = channel(64);
            let mut engine = Engine::new(48000.0, params.clone(), consumer);
            events.push(Event::NoteOn {
                note: 55,
                velocity: 1.0,
            });

            let mut out = vec![0.0f32; 48000 * 2];
            engine.process(&mut out);

            let peak = out.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
            assert!(
                peak > 0.01,
                "preset {:?} was silent (peak {peak})",
                preset.name
            );
            assert!(
                out.iter().all(|s| s.is_finite()),
                "preset {:?} produced a non-finite sample",
                preset.name
            );
        }
    }

    /// Loading a preset must not leave anything from the previous one behind.
    #[test]
    fn presets_fully_reset_the_patch() {
        let params = SharedParams::default();

        // Load the noisiest patch, then the cleanest.
        (ALL.iter().find(|p| p.name == "Wind").unwrap().apply)(&params);
        assert!(params.noise_level.get() > 0.5);

        (ALL.iter().find(|p| p.name == "Sub Bass").unwrap().apply)(&params);
        assert_eq!(
            params.noise_level.get(),
            0.0,
            "noise leaked from the previous patch"
        );
    }

    #[test]
    fn presets_leave_the_sequencer_alone() {
        let params = SharedParams::default();
        params.tempo.set(174.0);
        params.gen_root.set(7);
        params.seq_length.set(32);

        for preset in ALL {
            (preset.apply)(&params);
        }

        assert_eq!(params.tempo.get(), 174.0);
        assert_eq!(params.gen_root.get(), 7);
        assert_eq!(params.seq_length.get(), 32);
    }

    #[test]
    fn effects_do_not_leak_between_presets() {
        let params = SharedParams::from_params(&Params::default());

        // A thoroughly wet patch.
        params.delay_mix.set(0.9);
        params.delay_sync.set(true);
        params.delay_time.set(1.5);
        params.delay_division.set(NoteDivision::Whole as u32);
        params.delay_feedback.set(0.9);
        params.delay_damping.set(0.9);
        params.delay_ping_pong.set(true);
        params.reverb_mix.set(0.9);
        params.reverb_size.set(0.99);
        params.reverb_damping.set(0.9);
        params.reverb_predelay.set(0.2);
        params.reverb_width.set(0.1);

        // Init is the dry one. Loading it must leave nothing behind, or the
        // synth quietly sounds different depending on what you loaded before.
        let init = ALL.iter().find(|p| p.name == "Init").unwrap();
        (init.apply)(&params);

        let defaults = Params::default();
        let after = params.snapshot();
        assert_eq!(after.delay_mix, defaults.delay_mix);
        assert_eq!(after.delay_sync, defaults.delay_sync);
        assert_eq!(after.delay_time, defaults.delay_time);
        assert_eq!(after.delay_division, defaults.delay_division);
        assert_eq!(after.delay_feedback, defaults.delay_feedback);
        assert_eq!(after.delay_damping, defaults.delay_damping);
        assert_eq!(after.delay_ping_pong, defaults.delay_ping_pong);
        assert_eq!(after.reverb_mix, defaults.reverb_mix);
        assert_eq!(after.reverb_size, defaults.reverb_size);
        assert_eq!(after.reverb_damping, defaults.reverb_damping);
        assert_eq!(after.reverb_predelay, defaults.reverb_predelay);
        assert_eq!(after.reverb_width, defaults.reverb_width);
    }

    #[test]
    fn some_presets_use_the_effects() {
        let params = SharedParams::from_params(&Params::default());
        let wet = ALL
            .iter()
            .filter(|preset| {
                (preset.apply)(&params);
                let p = params.snapshot();
                p.delay_mix > 0.0 || p.reverb_mix > 0.0
            })
            .count();
        assert!(wet >= 3, "only {wet} presets use the effects");
    }
}
