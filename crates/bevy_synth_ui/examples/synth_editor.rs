//! The synth with its full control panel, in a window.
//!
//! Run with `cargo run -p bevy_synth_ui --example synth_editor --release`.
//!
//! Release mode matters: a debug build of the DSP can be slow enough to
//! underrun the audio callback, which sounds like crackling. The workspace
//! already forces `opt-level = 3` for `synth_core` in dev profiles for exactly
//! this reason, but the rest of the engine benefits too.
//!
//! - Play with the mouse on the on-screen keyboard, or the computer keyboard
//!   (`Z S X D C...` for the lower octave, `Q 2 W 3 E...` for the upper, `[`
//!   and `]` to transpose).
//! - Press Play to start the generative sequencer, then "New pattern" for a
//!   fresh melody in the chosen key.
//! - A MIDI keyboard, if one is plugged in, is picked up automatically.

use bevy::prelude::*;
use bevy_egui::EguiPlugin;

use bevy_synth::keyboard::{keyboard_input, KeyboardOctave};
use bevy_synth::synth_core::Scale;
use bevy_synth::{MidiMode, SynthPlugin};
use bevy_synth_ui::SynthUiPlugin;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "bevy_synth".into(),
                resolution: (1000, 760).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(EguiPlugin::default())
        // Connect to a MIDI keyboard if there is one. Absent hardware is not an
        // error: the plugin logs it and carries on.
        .add_plugins(SynthPlugin {
            midi: MidiMode::FirstAvailable,
            ..Default::default()
        })
        .add_plugins(SynthUiPlugin::open())
        .init_resource::<KeyboardOctave>()
        .add_systems(Startup, setup)
        .add_systems(Update, keyboard_input)
        .run();
}

fn setup(mut commands: Commands, synth: Res<bevy_synth::Synth>) {
    // egui renders through Bevy's camera, so there has to be one even though
    // nothing else in this example draws.
    commands.spawn(Camera2d);

    // A sensible starting point for the generator, so pressing Play gives
    // something musical straight away rather than a default C major scale.
    let p = &synth.params;
    p.gen_root.set(9); // A
    p.gen_scale.set(Scale::MinorPentatonic as u32);
    p.gen_octave.set(3);
    p.gen_range.set(2);
    p.tempo.set(108.0);
    p.seq_swing.set(0.12);

    // Start from a patch rather than the bare defaults.
    if let Some(preset) = bevy_synth_ui::presets::ALL
        .iter()
        .find(|preset| preset.name == "Pluck")
    {
        (preset.apply)(p);
    }

    synth.regenerate_with_seed(0x5EED_1234);
}
