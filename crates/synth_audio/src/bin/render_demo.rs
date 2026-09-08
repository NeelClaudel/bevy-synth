//! Renders a few demo patches to WAV files, without needing a sound card.
//!
//! Run with `cargo run -p synth_audio --bin render-demo -- <output directory>`.
//! Useful for checking a patch on a headless machine, and for hearing what the
//! generative sequencer does before wiring any of it into a game.

use std::sync::Arc;

use synth_audio::offline::{levels, render, write_wav};
use synth_core::{
    event::channel, filter::Slope, Engine, Event, LfoTarget, Scale, SharedParams, SvfMode,
    VoiceMode, Waveform,
};

const SAMPLE_RATE: f32 = 48000.0;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    std::fs::create_dir_all(&dir).expect("could not create the output directory");

    let demos: Vec<(&str, fn(&SharedParams), f32)> = vec![
        ("01-generative-pentatonic", generative_pentatonic, 12.0),
        ("02-acid-bassline", acid_bassline, 12.0),
        ("03-filter-sweep-chord", filter_sweep_chord, 8.0),
        ("04-filter-modes", filter_modes, 8.0),
        ("05-pwm-pad", pwm_pad, 10.0),
    ];

    for (name, setup, seconds) in demos {
        let params = Arc::new(SharedParams::default());
        setup(&params);

        let (events, consumer) = channel(1024);
        let mut engine = Engine::new(SAMPLE_RATE, params.clone(), consumer);

        // Some demos want notes played rather than sequenced.
        match name {
            "03-filter-sweep-chord" => {
                for note in [48, 55, 60, 63, 67] {
                    events.push(Event::NoteOn {
                        note,
                        velocity: 0.9,
                    });
                }
            }
            "04-filter-modes" => {
                events.push(Event::NoteOn {
                    note: 45,
                    velocity: 1.0,
                });
            }
            "05-pwm-pad" => {
                for note in [50, 57, 62, 66] {
                    events.push(Event::NoteOn {
                        note,
                        velocity: 0.7,
                    });
                }
            }
            _ => {
                events.push(Event::ClockStart);
            }
        }

        let mut samples = render(&mut engine, seconds);

        // The filter-modes demo sweeps through every response while one note
        // sustains, so you can hear what each tap sounds like back to back.
        if name == "04-filter-modes" {
            samples.clear();
            for mode in [
                SvfMode::Lowpass,
                SvfMode::Highpass,
                SvfMode::Bandpass,
                SvfMode::Notch,
            ] {
                params.filter_mode.set(mode as u32);
                samples.extend(render(&mut engine, 2.0));
            }
        }

        let (peak, rms) = levels(&samples);
        let path = format!("{dir}/{name}.wav");
        write_wav(&path, &samples, SAMPLE_RATE).expect("could not write the WAV file");
        println!("{path}: {:.1}s, peak {peak:.3}, rms {rms:.3}", seconds);
    }
}

/// The headline feature: random notes that stay in key and sound composed.
fn generative_pentatonic(p: &SharedParams) {
    p.gen_scale.set(Scale::MinorPentatonic as u32);
    p.gen_root.set(9); // A
    p.gen_octave.set(3);
    p.gen_range.set(2);
    p.gen_density.set(0.8);
    p.gen_chord_bias.set(0.7);
    p.gen_max_jump.set(3.0);
    p.tempo.set(112.0);
    p.seq_swing.set(0.12);
    p.seq_gate.set(0.55);

    p.osc1_wave.set(Waveform::Saw as u32);
    p.osc2_wave.set(Waveform::Saw as u32);
    p.osc2_detune.set(9.0);
    p.cutoff.set(1400.0);
    p.resonance.set(0.4);
    p.filter_env_amount.set(2.2);
    p.amp_decay.set(0.35);
    p.amp_sustain.set(0.25);
    p.amp_release.set(0.25);
    p.master_gain.set(0.45);
}

/// Mono, high resonance, short filter envelope, plenty of glide: the sound the
/// whole mono/legato/glide path exists for.
fn acid_bassline(p: &SharedParams) {
    p.voice_mode.set(VoiceMode::Mono as u32);
    p.legato.set(true);
    p.glide.set(0.06);

    p.gen_scale.set(Scale::MinorPentatonic as u32);
    p.gen_root.set(0);
    p.gen_octave.set(1);
    p.gen_range.set(2);
    p.gen_density.set(0.9);
    p.gen_max_jump.set(4.0);
    p.tempo.set(130.0);
    p.seq_gate.set(0.85);

    p.osc1_wave.set(Waveform::Saw as u32);
    p.osc2_level.set(0.0);
    p.sub_level.set(0.3);
    p.filter_slope.set(Slope::Db24 as u32);
    p.cutoff.set(300.0);
    p.resonance.set(0.9);
    p.filter_env_amount.set(3.5);
    p.filter_decay.set(0.22);
    p.filter_sustain.set(0.0);
    p.amp_sustain.set(0.9);
    p.amp_release.set(0.1);
    p.drive.set(2.5);
    p.master_gain.set(0.5);
}

/// A held chord under a slow filter envelope: tests polyphony and the envelope
/// shape at once.
fn filter_sweep_chord(p: &SharedParams) {
    p.seq_playing.set(false);
    p.osc1_wave.set(Waveform::Saw as u32);
    p.osc2_wave.set(Waveform::Saw as u32);
    p.osc2_detune.set(-8.0);
    p.cutoff.set(180.0);
    p.resonance.set(0.7);
    p.filter_env_amount.set(4.5);
    p.filter_attack.set(2.0);
    p.filter_decay.set(3.0);
    p.filter_sustain.set(0.4);
    p.amp_attack.set(0.4);
    p.amp_sustain.set(1.0);
    p.master_gain.set(0.35);
}

/// One sustained note through each filter mode in turn.
fn filter_modes(p: &SharedParams) {
    p.seq_playing.set(false);
    p.osc1_wave.set(Waveform::Saw as u32);
    p.osc2_level.set(0.0);
    p.cutoff.set(900.0);
    p.resonance.set(0.6);
    p.filter_env_amount.set(0.0);
    p.amp_attack.set(0.05);
    p.amp_sustain.set(1.0);
    p.master_gain.set(0.4);
}

/// Pulse-width modulation on a slow LFO: the classic warm synth-string pad.
fn pwm_pad(p: &SharedParams) {
    p.seq_playing.set(false);
    p.osc1_wave.set(Waveform::Pulse as u32);
    p.osc2_wave.set(Waveform::Pulse as u32);
    p.osc2_detune.set(6.0);
    p.pulse_width.set(0.5);
    p.lfo_target.set(LfoTarget::PulseWidth as u32);
    p.lfo_rate.set(0.35);
    p.lfo_depth.set(0.8);
    p.cutoff.set(2200.0);
    p.resonance.set(0.2);
    p.filter_env_amount.set(1.0);
    p.amp_attack.set(0.8);
    p.amp_decay.set(1.0);
    p.amp_sustain.set(0.8);
    p.amp_release.set(1.5);
    p.master_gain.set(0.35);
}
