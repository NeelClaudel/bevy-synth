//! A headless Bevy app that jams with itself.
//!
//! Run with `cargo run -p bevy_synth --example generative_jam`. It starts the
//! generative sequencer, then changes key, scale and patch every few bars, so
//! you can hear what the pitch constraints actually do. Ctrl-C to stop.
//!
//! Headless on purpose: it needs no window, no renderer and no assets, so it is
//! the shortest path from `cargo run` to a sound. Everything here works the
//! same in a real game with a window.
//!
//! If no audio device is available (a container, a CI runner), the plugin logs a
//! warning and the app runs silently rather than failing — which is what a game
//! should do.

use std::time::Duration;

use bevy_app::{App, ScheduleRunnerPlugin, Startup, TaskPoolPlugin, Update};
use bevy_ecs::prelude::*;

use bevy_synth::synth_core::{filter::Slope, LfoTarget, Scale, VoiceMode, Waveform};
use bevy_synth::{Synth, SynthPlugin, SynthTelemetry};

fn main() {
    App::new()
        .add_plugins(TaskPoolPlugin::default())
        .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
            1.0 / 60.0,
        )))
        .add_plugins(bevy_log::LogPlugin::default())
        .add_plugins(SynthPlugin::default())
        .init_resource::<Jam>()
        .add_systems(Startup, setup)
        .add_systems(Update, (advance_the_jam, report_the_beat))
        .run();
}

/// The sections the jam cycles through, in order.
const SECTIONS: &[Section] = &[
    Section {
        name: "A minor pentatonic, plucky",
        root: 9,
        scale: Scale::MinorPentatonic,
        tempo: 108.0,
        density: 0.75,
        chord_bias: 0.7,
    },
    Section {
        name: "D dorian, flowing",
        root: 2,
        scale: Scale::Dorian,
        tempo: 108.0,
        density: 0.9,
        chord_bias: 0.45,
    },
    Section {
        name: "E phrygian dominant, tense",
        root: 4,
        scale: Scale::PhrygianDominant,
        tempo: 120.0,
        density: 0.8,
        chord_bias: 0.8,
    },
    Section {
        name: "C hirajoshi, sparse",
        root: 0,
        scale: Scale::Hirajoshi,
        tempo: 92.0,
        density: 0.55,
        chord_bias: 0.6,
    },
];

struct Section {
    name: &'static str,
    root: u8,
    scale: Scale,
    tempo: f32,
    density: f32,
    chord_bias: f32,
}

#[derive(Resource, Default)]
struct Jam {
    frames: u32,
    section: usize,
    last_step: u32,
}

/// Builds the patch. Everything here is just setting atomics; the audio thread
/// picks the changes up at its next block boundary.
fn setup(synth: Res<Synth>) {
    let p = &synth.params;

    // Two detuned saws is the workhorse synth sound: the beating between them
    // is what makes it sound wide rather than thin.
    p.osc1_wave.set(Waveform::Saw as u32);
    p.osc2_wave.set(Waveform::Saw as u32);
    p.osc2_detune.set(8.0);
    p.sub_level.set(0.25);

    p.filter_slope.set(Slope::Db24 as u32);
    p.cutoff.set(900.0);
    p.resonance.set(0.45);
    // A positive envelope amount plus a short decay is the classic plucked
    // filter attack: bright at the start, dark as it dies.
    p.filter_env_amount.set(2.8);
    p.filter_attack.set(0.002);
    p.filter_decay.set(0.35);
    p.filter_sustain.set(0.15);
    p.filter_key_track.set(0.4);
    p.filter_velocity.set(0.5);

    p.amp_attack.set(0.004);
    p.amp_decay.set(0.4);
    p.amp_sustain.set(0.25);
    p.amp_release.set(0.3);

    // A slow drift on the cutoff so a long passage never sits perfectly still.
    p.lfo_target.set(LfoTarget::Cutoff as u32);
    p.lfo_wave.set(bevy_synth::synth_core::lfo::LfoWave::SmoothRandom as u32);
    p.lfo_rate.set(0.2);
    p.lfo_depth.set(0.25);

    p.voice_mode.set(VoiceMode::Poly as u32);
    p.max_voices.set(8);
    p.seq_swing.set(0.14);
    p.seq_gate.set(0.55);
    p.seq_length.set(16);
    p.drive.set(1.4);
    p.master_gain.set(0.45);

    apply_section(&synth, &SECTIONS[0]);
    synth.play();

    bevy_log::info!("playing: {}", SECTIONS[0].name);
    bevy_log::info!("audio running: {}", synth.is_running());
}

/// Moves to the next section every eight seconds.
fn advance_the_jam(synth: Res<Synth>, mut jam: ResMut<Jam>) {
    jam.frames += 1;

    // 60 fps * 8 seconds. Counting frames rather than using `Time` keeps this
    // example free of another dependency; a real game would use `Time`.
    if jam.frames % (60 * 8) != 0 {
        return;
    }

    jam.section = (jam.section + 1) % SECTIONS.len();
    let section = &SECTIONS[jam.section];
    apply_section(&synth, section);
    bevy_log::info!("playing: {}", section.name);
}

fn apply_section(synth: &Synth, section: &Section) {
    let p = &synth.params;
    p.gen_root.set(section.root as u32);
    p.gen_scale.set(section.scale as u32);
    p.gen_density.set(section.density);
    p.gen_chord_bias.set(section.chord_bias);
    p.tempo.set(section.tempo);
    p.gen_octave.set(3);
    p.gen_range.set(2);
    p.gen_max_jump.set(3.0);

    // A fresh melody for the new key. Seeding from the section index means the
    // same section always plays the same tune, so the piece has a structure you
    // can recognise on the second time round.
    synth.regenerate_with_seed(0x51D_0000 + section.root as u64);
}

/// Shows how to drive visuals from the music: this prints, but the same
/// `step_changed` flag would trigger an animation or a particle burst.
fn report_the_beat(telemetry: Res<SynthTelemetry>, mut jam: ResMut<Jam>) {
    if !telemetry.step_changed {
        return;
    }
    jam.last_step = telemetry.current_step;

    // Downbeats only, or it is a wall of text.
    if telemetry.current_step % 4 == 0 {
        let meter = "#".repeat((telemetry.peak * 40.0) as usize);
        bevy_log::info!(
            "step {:2}  voices {}  |{}",
            telemetry.current_step,
            telemetry.active_voices,
            meter
        );
    }
}
