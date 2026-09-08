//! A synthesizer plugin for Bevy.
//!
//! Adds [`SynthPlugin`] to a Bevy app and you get a [`Synth`] resource: a
//! polyphonic subtractive synth running on its own audio thread, playable from
//! systems, from a MIDI keyboard, or from its own generative sequencer.
//!
//! ```no_run
//! use bevy_app::prelude::*;
//! use bevy_synth::{Synth, SynthPlugin};
//!
//! fn main() {
//!     App::new()
//!         .add_plugins(SynthPlugin::default())
//!         .add_systems(Startup, play_a_chord)
//!         .run();
//! }
//!
//! fn play_a_chord(synth: bevy_ecs::system::Res<Synth>) {
//!     for note in [60, 64, 67] {
//!         synth.note_on(note, 0.8);
//!     }
//! }
//! ```
//!
//! # Why the synth is a resource and not a component
//!
//! There is one audio device and one audio thread. Modelling that as an entity
//! with components would suggest you can have several, and the second one would
//! fail to open the device. A resource says what is true: there is exactly one.
//!
//! Per-*sound* state does belong on entities — which enemy is playing which
//! note — but that is game state, and it drives the synth rather than being it.
//!
//! # What runs where
//!
//! Nothing in this crate generates a sample. Systems here only write atomics
//! and push events; all synthesis happens on the audio thread inside
//! [`synth_core::Engine`]. A frame spike will never glitch the audio, and audio
//! load will never slow a frame.

use std::sync::Arc;

use bevy_app::{App, Plugin, PostUpdate};
use bevy_ecs::prelude::*;

use synth_audio::SynthBuilder;
use synth_core::{Event, SharedParams};

pub use synth_audio;
pub use synth_core;

#[cfg(feature = "keyboard")]
pub mod keyboard;

/// How the plugin should connect to MIDI hardware.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MidiMode {
    /// Do not touch MIDI at all.
    #[default]
    Disabled,
    /// Connect to the first available input port, if there is one.
    FirstAvailable,
    /// Connect to the first port whose name contains this string,
    /// case-insensitively.
    Matching(String),
}

/// Adds a synthesizer to the app.
#[derive(Default)]
pub struct SynthPlugin {
    /// Parameters to start from. Useful for loading a patch before any sound is
    /// made; leave as `None` for the defaults.
    pub params: Option<Arc<SharedParams>>,
    pub midi: MidiMode,
    /// Start the sequencer's transport immediately.
    pub autoplay: bool,
}

impl SynthPlugin {
    /// A synth that connects to the first MIDI port it finds.
    pub fn with_midi() -> Self {
        Self {
            midi: MidiMode::FirstAvailable,
            ..Default::default()
        }
    }
}

impl Plugin for SynthPlugin {
    fn build(&self, app: &mut App) {
        let params = self
            .params
            .clone()
            .unwrap_or_else(|| Arc::new(SharedParams::default()));

        let mut builder = SynthBuilder::new().params(params.clone());

        // MIDI gets its own event queue straight into the engine, rather than
        // being forwarded through the ECS. A keypress that waits for the next
        // frame picks up to 16 ms of latency, which is enough to make a
        // keyboard feel unresponsive.
        #[cfg(feature = "midi")]
        let midi_connection = match &self.midi {
            MidiMode::Disabled => None,
            MidiMode::FirstAvailable => match synth_audio::MidiInput::open_first() {
                Ok((connection, consumer)) => {
                    builder = builder.event_source(consumer);
                    Some(connection)
                }
                Err(e) => {
                    // No MIDI is an entirely normal state. Say so once and
                    // carry on: a game must still run without a keyboard
                    // plugged in.
                    bevy_log::info!("bevy_synth: no MIDI input connected ({e})");
                    None
                }
            },
            MidiMode::Matching(needle) => match synth_audio::MidiInput::open_matching(needle) {
                Ok((connection, consumer)) => {
                    builder = builder.event_source(consumer);
                    Some(connection)
                }
                Err(e) => {
                    bevy_log::info!("bevy_synth: no MIDI port matching {needle:?} ({e})");
                    None
                }
            },
        };

        match builder.start() {
            Ok(synth) => {
                bevy_log::info!(
                    "bevy_synth: audio running at {} Hz, {} channels",
                    synth.sample_rate,
                    synth.channels
                );
                if self.autoplay {
                    synth.events.push(Event::ClockStart);
                }
                app.insert_resource(Synth {
                    inner: Some(synth),
                    params,
                    #[cfg(feature = "midi")]
                    _midi: midi_connection,
                });
            }
            Err(e) => {
                // Failing to open audio must not stop the game from running.
                // The resource is still inserted, so systems that play notes
                // keep compiling and running; they just make no sound.
                bevy_log::warn!("bevy_synth: could not start audio ({e}); running silent");
                app.insert_resource(Synth {
                    inner: None,
                    params,
                    #[cfg(feature = "midi")]
                    _midi: midi_connection,
                });
            }
        }

        app.init_resource::<SynthTelemetry>()
            .add_systems(PostUpdate, read_telemetry);
    }
}

/// The synthesizer, as seen from the ECS.
///
/// Every method takes `&self`, not `&mut self`, because the underlying state is
/// atomic. Systems can take `Res<Synth>` rather than `ResMut<Synth>` and so run
/// in parallel with each other — which matters when a dozen systems all want to
/// make a noise.
#[derive(Resource)]
pub struct Synth {
    inner: Option<synth_audio::Synth>,
    /// Direct access to every parameter, for UI and patch code.
    pub params: Arc<SharedParams>,
    #[cfg(feature = "midi")]
    _midi: Option<synth_audio::MidiInput>,
}

impl Synth {
    /// True if audio actually opened. Worth checking before showing a UI that
    /// implies sound is coming out.
    pub fn is_running(&self) -> bool {
        self.inner.is_some()
    }

    pub fn sample_rate(&self) -> f32 {
        self.inner.as_ref().map(|s| s.sample_rate).unwrap_or(0.0)
    }

    /// Sends an event to the audio thread.
    ///
    /// Returns `false` if the queue was full and the event was dropped, which
    /// in practice means the audio thread has stopped.
    pub fn send(&self, event: Event) -> bool {
        match &self.inner {
            Some(synth) => synth.events.push(event),
            None => false,
        }
    }

    /// Starts a note. Velocity is `0.0..=1.0`.
    pub fn note_on(&self, note: u8, velocity: f32) -> bool {
        self.send(Event::NoteOn { note, velocity })
    }

    pub fn note_off(&self, note: u8) -> bool {
        self.send(Event::NoteOff { note })
    }

    /// Releases every note, respecting release times.
    pub fn all_notes_off(&self) -> bool {
        self.send(Event::AllNotesOff)
    }

    /// Cuts all sound immediately. Clicks; for stuck notes only.
    pub fn panic(&self) -> bool {
        self.send(Event::Panic)
    }

    /// Starts the sequencer from the beginning of the pattern.
    pub fn play(&self) -> bool {
        self.send(Event::ClockStart)
    }

    pub fn stop(&self) -> bool {
        self.send(Event::ClockStop)
    }

    /// Resumes without rewinding.
    pub fn resume(&self) -> bool {
        self.send(Event::ClockContinue)
    }

    /// The sequencer's current pattern.
    ///
    /// Read from a lock-free mirror the audio thread publishes whenever the
    /// pattern changes, so this never blocks and never sees a half-written
    /// step. Returned by value: 512 bytes on the stack costs far less than
    /// synchronising with the audio thread would.
    pub fn pattern(&self) -> synth_core::Pattern {
        self.params.read_pattern()
    }

    /// Overwrites one step of the pattern.
    pub fn set_step(&self, index: usize, step: synth_core::Step) -> bool {
        if index > u8::MAX as usize {
            return false;
        }
        self.send(Event::SetStep {
            index: index as u8,
            step,
        })
    }

    /// Turns one step on or off, keeping its note and velocity.
    ///
    /// Toggling rather than clearing means a step switched off and on again
    /// comes back with the note the generator chose, which is what makes the
    /// grid usable for editing a generated pattern rather than only for
    /// wiping it.
    pub fn toggle_step(&self, index: usize) -> bool {
        let mut step = self.params.read_step(index);
        step.active = !step.active;
        self.set_step(index, step)
    }

    /// Plays a pattern saved earlier, from [`Synth::pattern`].
    ///
    /// The swap happens at the top of the next loop, so switching between
    /// saved patterns while the sequencer runs never lands a new melody
    /// halfway through a bar. Stopped, it takes effect immediately. The
    /// pattern carries the loop length it was saved at, so loading one
    /// restores that too.
    pub fn load_pattern(&self, pattern: &synth_core::Pattern) {
        self.params.queue_pattern(pattern);
    }

    /// Writes a new generated pattern from the current `gen_*` parameters.
    ///
    /// Set `params.gen_seed` first for a specific melody, or leave it to get a
    /// different one each time.
    pub fn regenerate(&self) {
        self.params.regenerate();
    }

    /// Sets the seed and immediately regenerates, so a given seed always yields
    /// the same melody. Handy for making a level's theme reproducible.
    pub fn regenerate_with_seed(&self, seed: u64) {
        self.params
            .gen_seed
            .store(seed, std::sync::atomic::Ordering::Relaxed);
        self.params.regenerate();
    }
}

/// What the audio thread reports back, refreshed once a frame.
///
/// Read this rather than the atomics directly if you want values that stay
/// consistent for the whole frame — two systems reading the atomic could
/// otherwise see different sequencer steps in the same frame and disagree about
/// which beat it is.
#[derive(Resource, Debug, Default, Clone, Copy)]
pub struct SynthTelemetry {
    /// Sequencer step currently playing.
    pub current_step: u32,
    /// Whether the step changed this frame. The cue for beat-synced visuals.
    pub step_changed: bool,
    /// How many voices are sounding.
    pub active_voices: u32,
    /// Peak output level since the last frame, `0.0..=1.0`. Drives a meter, or
    /// anything that should pulse with the music.
    pub peak: f32,
}

fn read_telemetry(synth: Res<Synth>, mut telemetry: ResMut<SynthTelemetry>) {
    use std::sync::atomic::Ordering::Relaxed;

    let step = synth.params.current_step.load(Relaxed);
    telemetry.step_changed = step != telemetry.current_step;
    telemetry.current_step = step;
    telemetry.active_voices = synth.params.active_voices.load(Relaxed);
    // `take_peak` resets the meter, so each frame reports the peak since the
    // last one rather than an all-time high that never falls.
    telemetry.peak = synth.params.take_peak();
}
