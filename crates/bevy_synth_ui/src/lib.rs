//! An egui control panel for [`bevy_synth`].
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_egui::EguiPlugin;
//! use bevy_synth::SynthPlugin;
//! use bevy_synth_ui::SynthUiPlugin;
//!
//! App::new()
//!     .add_plugins(DefaultPlugins)
//!     .add_plugins(EguiPlugin::default())
//!     .add_plugins(SynthPlugin::default())
//!     .add_plugins(SynthUiPlugin::default())
//!     .run();
//! ```
//!
//! # What the panel is for
//!
//! Two things, and they pull in different directions. It is a patch editor —
//! somewhere to design a sound by ear — and it is a debug overlay for a running
//! game, showing what the audio thread is actually doing. The layout serves the
//! first; the meters, voice count and step highlight serve the second.
//!
//! # Why it never blocks the audio thread
//!
//! Every control writes an atomic and returns. Nothing here locks, and nothing
//! here waits for the audio thread. A UI that stalls drops a frame; a UI that
//! made the audio thread wait would drop the audio, which is far worse and far
//! more noticeable.

use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass};
use egui::{Ui, Vec2};

use bevy_synth::{Synth, SynthTelemetry};
use synth_core::env::AdsrSettings;
use synth_core::filter::{Slope, SvfMode};
use synth_core::lfo::{LfoTarget, LfoWave};
use synth_core::params::{AtomicEnum, ClockSource, VoiceMode};
use synth_core::{Cell, NoteDivision, Pad, Scale, Waveform, PAD_COUNT};

pub mod presets;
pub mod widgets;

use widgets::{palette, KnobSpec};

/// Adds the synth control panel.
#[derive(Default)]
pub struct SynthUiPlugin {
    /// Whether the panel starts visible.
    pub open: bool,
}

impl SynthUiPlugin {
    /// A panel that is visible from the start.
    pub fn open() -> Self {
        Self { open: true }
    }
}

impl Plugin for SynthUiPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(SynthUi {
            open: self.open,
            ..Default::default()
        })
        // The panel must run in egui's own pass, not `Update`: the context is
        // only valid between egui's begin and end frame.
        .add_systems(EguiPrimaryContextPass, panel);
    }
}

/// Panel state that is the UI's own, not the synth's.
#[derive(Resource)]
pub struct SynthUi {
    pub open: bool,
    /// Lowest note shown on the on-screen keyboard.
    pub keyboard_base: u8,
    pub keyboard_octaves: u32,
    /// The note the pointer is currently holding down on the keyboard.
    held_from_piano: Option<u8>,
    /// Notes the panel believes are sounding, for keyboard highlighting.
    ///
    /// Tracked here rather than read back from the engine because the engine
    /// deliberately exposes no per-voice detail: doing so would mean either a
    /// lock or a much larger telemetry block, to light up a few keys.
    sounding: Vec<u8>,
    meter_hold: f32,
    /// Patterns put aside to switch between. Held here rather than in the
    /// synth because they are a composing aid, not part of the sound: the
    /// engine has one pattern, and these are the ones waiting their turn.
    ///
    /// They live as long as the app does. Saving them to disk would mean
    /// deciding where a game's save data goes, which is the game's business
    /// and not the panel's.
    slots: [Option<synth_core::Pattern>; SLOTS],
    /// The slot last loaded or saved, shown highlighted.
    active_slot: Option<usize>,
    /// The pad whose knobs are showing.
    ///
    /// One pad's controls at a time. The alternative is twenty-four knobs on
    /// screen at once, which is the wall of sliders this panel's own design
    /// notes exist to avoid.
    selected_pad: usize,
}

/// Enough to hold a verse, a chorus and a couple of alternatives, which is as
/// much as a row of buttons can show without becoming a file browser.
const SLOTS: usize = 4;

impl Default for SynthUi {
    fn default() -> Self {
        Self {
            open: true,
            // C3, so two octaves reaches middle C and the octave above it.
            keyboard_base: 48,
            keyboard_octaves: 3,
            held_from_piano: None,
            sounding: Vec::new(),
            meter_hold: 0.0,
            slots: [None; SLOTS],
            active_slot: None,
            selected_pad: 0,
        }
    }
}

/// Enums the panel can render as a selector.
///
/// The synth's enums all already carry a name, an `ALL` list and `u32`
/// conversions; this trait just lets one generic helper drive every selector
/// rather than a dozen near-identical blocks.
trait UiEnum: Copy + PartialEq + 'static {
    fn options() -> &'static [Self];
    fn label(self) -> &'static str;
    fn to_u32(self) -> u32;
    fn from_u32(value: u32) -> Self;
}

macro_rules! ui_enum {
    ($type:ty, $all:expr, $name:expr) => {
        impl UiEnum for $type {
            fn options() -> &'static [Self] {
                $all
            }
            fn label(self) -> &'static str {
                #[allow(clippy::redundant_closure_call)]
                ($name)(self)
            }
            fn to_u32(self) -> u32 {
                self as u32
            }
            fn from_u32(value: u32) -> Self {
                <$type>::from_u32(value)
            }
        }
    };
}

ui_enum!(Waveform, &Waveform::ALL, |w: Waveform| w.name());
ui_enum!(SvfMode, &SvfMode::ALL, |m: SvfMode| m.name());
ui_enum!(LfoWave, &LfoWave::ALL, |w: LfoWave| w.name());
ui_enum!(LfoTarget, &LfoTarget::ALL, |t: LfoTarget| t.name());
ui_enum!(Scale, &Scale::ALL, |s: Scale| s.name());
ui_enum!(NoteDivision, &NoteDivision::ALL, |d: NoteDivision| d.name());
ui_enum!(
    Slope,
    &[Slope::Db12, Slope::Db24],
    |s: Slope| match s {
        Slope::Db12 => "12 dB",
        Slope::Db24 => "24 dB",
    }
);
ui_enum!(
    VoiceMode,
    &[VoiceMode::Mono, VoiceMode::Poly],
    |m: VoiceMode| m.name()
);
ui_enum!(
    ClockSource,
    &[ClockSource::Internal, ClockSource::ExternalMidi],
    |c: ClockSource| match c {
        ClockSource::Internal => "Internal",
        ClockSource::ExternalMidi => "MIDI clock",
    }
);

/// A row of small toggle buttons, for enums with few options.
///
/// Faster than a dropdown for anything under about six choices: every option is
/// visible and one click away, which matters when you are auditioning
/// waveforms by ear rather than picking a known answer.
fn selector<T: UiEnum>(ui: &mut Ui, param: &AtomicEnum) {
    let current = T::from_u32(param.get());
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for &option in T::options() {
            if ui
                .selectable_label(option == current, option.label())
                .clicked()
            {
                param.set(option.to_u32());
            }
        }
    });
}

/// A dropdown, for enums with many options.
fn dropdown<T: UiEnum>(ui: &mut Ui, id: &str, param: &AtomicEnum) {
    let current = T::from_u32(param.get());
    egui::ComboBox::from_id_salt(id)
        .selected_text(current.label())
        .show_ui(ui, |ui| {
            for &option in T::options() {
                if ui
                    .selectable_label(option == current, option.label())
                    .clicked()
                {
                    param.set(option.to_u32());
                }
            }
        });
}

/// An integer parameter as a drag box.
fn integer(ui: &mut Ui, param: &AtomicEnum, range: std::ops::RangeInclusive<u32>, suffix: &str) {
    let mut value = param.get();
    if ui
        .add(
            egui::DragValue::new(&mut value)
                .range(range)
                .suffix(suffix)
                .speed(0.15),
        )
        .changed()
    {
        param.set(value);
    }
}

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

fn panel(
    mut contexts: EguiContexts,
    synth: Res<Synth>,
    telemetry: Res<SynthTelemetry>,
    mut ui_state: ResMut<SynthUi>,
) {
    // No egui context yet (or several) is a normal transient state during
    // startup and window changes, not an error worth crashing the app over.
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };

    if !ui_state.open {
        return;
    }

    let mut open = ui_state.open;
    egui::Window::new("Synth")
        .default_size([980.0, 860.0])
        // Keep the window inside the viewport. Without this it can open — or be
        // dragged — mostly off-screen, with no way to get hold of it again.
        .constrain(true)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);

            // Measured out here, before the scroll area. Inside one, the
            // available width is the scrollable extent rather than the visible
            // one, which for a horizontally scrolling area is effectively
            // unbounded — the columns would read "everything fits" and never
            // wrap.
            let visible_width = ui.available_width();

            // Whatever the window size, everything stays reachable. The columns
            // reflow to fit the width; this catches what is left over, mostly
            // height: the full panel is taller than a laptop screen.
            egui::ScrollArea::both().show(ui, |ui| {
                transport(ui, &synth, &telemetry, &mut ui_state);
                ui.add_space(2.0);

                synth_columns(ui, &synth, visible_width);

                ui.add_space(2.0);
                sequencer(ui, &synth, &telemetry, &mut ui_state);
                ui.add_space(2.0);
                drums(ui, &synth, &telemetry, &mut ui_state);
                ui.add_space(2.0);
                keyboard(ui, &synth, &mut ui_state);
            });
        });
    ui_state.open = open;
}

/// The four columns of synth controls, wrapped onto as many rows as `width`
/// allows.
///
/// Laid out by hand rather than with `horizontal_wrapped`, which wraps
/// individual widgets: each column here is a `vertical` block that claims the
/// whole remaining width as it goes, so wrapping would put every column on a
/// row of its own no matter how wide the window was.
fn synth_columns(ui: &mut Ui, synth: &Synth, width: f32) {
    // About the natural width of one column of sections. It only decides how
    // many columns fit, so guessing low is the safe direction to be wrong in:
    // too few columns per row still fits on screen, too many does not.
    const COLUMN_WIDTH: f32 = 265.0;

    let columns: [&dyn Fn(&mut Ui); 4] = [
        &|ui| {
            oscillators(ui, synth);
            voice_section(ui, synth);
        },
        &|ui| {
            filter_section(ui, synth);
            envelopes(ui, synth);
        },
        &|ui| {
            lfo_section(ui, synth);
            presets::section(ui, synth);
        },
        &|ui| {
            delay_section(ui, synth);
            reverb_section(ui, synth);
        },
    ];

    let per_row = ((width / COLUMN_WIDTH) as usize).clamp(1, columns.len());
    for row in columns.chunks(per_row) {
        ui.horizontal_top(|ui| {
            for column in row {
                ui.vertical(|ui| column(ui));
            }
        });
    }
}

fn transport(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi) {
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
            );
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

fn oscillators(ui: &mut Ui, synth: &Synth) {
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

fn filter_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "FILTER", palette::FILTER, |ui| {
        widgets::filter_display(
            ui,
            Vec2::new(300.0, 74.0),
            SvfMode::from_u32(p.filter_mode.get()),
            Slope::from_u32(p.filter_slope.get()),
            p.cutoff.get(),
            p.resonance.get(),
        );
        ui.add_space(3.0);
        selector::<SvfMode>(ui, &p.filter_mode);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Slope")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            selector::<Slope>(ui, &p.filter_slope);
        });
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Cutoff", 20.0..=20000.0)
                    .log()
                    .colour(palette::FILTER)
                    .unit("Hz")
                    .default(2000.0),
                &p.cutoff,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Reso", 0.0..=1.0)
                    .colour(palette::FILTER)
                    .default(0.25),
                &p.resonance,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Env amt", -6.0..=6.0)
                    .colour(palette::FILTER)
                    .unit("oct")
                    .default(2.0),
                &p.filter_env_amount,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Key trk", 0.0..=1.0)
                    .colour(palette::FILTER)
                    .default(0.35),
                &p.filter_key_track,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Vel", 0.0..=1.0)
                    .colour(palette::FILTER)
                    .default(0.4),
                &p.filter_velocity,
            );
        });
    });
}

fn envelopes(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "ENVELOPES", palette::ENV, |ui| {
        for (title, colour, attack, decay, sustain, release) in [
            (
                "Amplitude",
                palette::ENV,
                &p.amp_attack,
                &p.amp_decay,
                &p.amp_sustain,
                &p.amp_release,
            ),
            (
                "Filter",
                palette::FILTER,
                &p.filter_attack,
                &p.filter_decay,
                &p.filter_sustain,
                &p.filter_release,
            ),
        ] {
            ui.label(egui::RichText::new(title).color(palette::TEXT_DIM).size(9.0));
            widgets::adsr_display(
                ui,
                Vec2::new(300.0, 52.0),
                &AdsrSettings {
                    attack: attack.get(),
                    decay: decay.get(),
                    sustain: sustain.get(),
                    release: release.get(),
                },
                colour,
            );
            ui.horizontal(|ui| {
                // Times are logarithmic: the difference between 1 ms and 10 ms
                // is a whole character of attack, and on a linear knob both sit
                // in the first pixel.
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("A", 0.001..=10.0)
                        .log()
                        .colour(colour)
                        .unit("s")
                        .default(0.005)
                        .size(36.0),
                    attack,
                );
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("D", 0.001..=10.0)
                        .log()
                        .colour(colour)
                        .unit("s")
                        .default(0.25)
                        .size(36.0),
                    decay,
                );
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("S", 0.0..=1.0)
                        .colour(colour)
                        .default(0.7)
                        .size(36.0),
                    sustain,
                );
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("R", 0.001..=10.0)
                        .log()
                        .colour(colour)
                        .unit("s")
                        .default(0.3)
                        .size(36.0),
                    release,
                );
            });
            ui.add_space(4.0);
        }
    });
}

fn lfo_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "LFO", palette::LFO, |ui| {
        dropdown::<LfoWave>(ui, "lfo_wave", &p.lfo_wave);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("To")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            dropdown::<LfoTarget>(ui, "lfo_target", &p.lfo_target);
        });
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Rate", 0.05..=40.0)
                    .log()
                    .colour(palette::LFO)
                    .unit("Hz")
                    .default(5.0),
                &p.lfo_rate,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Depth", 0.0..=1.0)
                    .colour(palette::LFO)
                    .default(0.0),
                &p.lfo_depth,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Mod whl", 0.0..=1.0)
                    .colour(palette::LFO)
                    .default(0.0),
                &p.mod_wheel,
            );
        });
        let mut retrigger = p.lfo_retrigger.get();
        if ui
            .checkbox(&mut retrigger, "Retrigger per note")
            .on_hover_text("restart the LFO on every note instead of free-running")
            .changed()
        {
            p.lfo_retrigger.set(retrigger);
        }
    });
}

fn delay_section(ui: &mut Ui, synth: &Synth) {
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

fn reverb_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "REVERB", palette::FX, |ui| {
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Size", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(0.5),
                &p.reverb_size,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Damping", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(0.5),
                &p.reverb_damping,
            );
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Pre-delay", 0.0..=0.25)
                    .colour(palette::FX)
                    .unit("s")
                    .default(0.02),
                &p.reverb_predelay,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Width", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(1.0),
                &p.reverb_width,
            );
        });

        widgets::knob_param(
            ui,
            &KnobSpec::new("Mix", 0.0..=1.0)
                .colour(palette::FX)
                .default(0.0),
            &p.reverb_mix,
        );
    });
}

fn voice_section(ui: &mut Ui, synth: &Synth) {
    let p = &synth.params;
    widgets::section(ui, "VOICES", palette::OSC, |ui| {
        selector::<VoiceMode>(ui, &p.voice_mode);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Max")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            integer(ui, &p.max_voices, 1..=32, "");

            let mut legato = p.legato.get();
            if ui
                .checkbox(&mut legato, "Legato")
                .on_hover_text("overlapping notes glide instead of retriggering")
                .changed()
            {
                p.legato.set(legato);
            }
        });
        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Glide", 0.0..=2.0)
                    .colour(palette::OSC)
                    .unit("s")
                    .default(0.0),
                &p.glide,
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Bend", -12.0..=12.0)
                    .colour(palette::OSC)
                    .unit("st")
                    .default(0.0),
                &p.pitch_bend,
            );
        });
    });
}

fn sequencer(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi) {
    let p = &synth.params;
    widgets::section(ui, "SEQUENCER", palette::SEQ, |ui| {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                // Bound to a local so the snapshot lives for the whole draw:
                // `Pattern` derefs to a slice, but only while it is alive.
                let pattern = synth.pattern();
                if let Some(index) = widgets::step_grid(
                    ui,
                    &pattern,
                    telemetry.current_step as usize,
                    p.seq_playing.get(),
                ) {
                    synth.toggle_step(index);
                }
            });

            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    widgets::knob_param(
                        ui,
                        &KnobSpec::new("Gate", 0.05..=1.5)
                            .colour(palette::SEQ)
                            .default(0.6)
                            .size(36.0),
                        &p.seq_gate,
                    );
                    widgets::knob_param(
                        ui,
                        &KnobSpec::new("Swing", 0.0..=0.6)
                            .colour(palette::SEQ)
                            .default(0.0)
                            .size(36.0),
                        &p.seq_swing,
                    );
                    ui.vertical(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Length")
                                .color(palette::TEXT_DIM)
                                .size(10.0),
                        );
                        integer(ui, &p.seq_length, 1..=64, " steps");
                    });
                });

                let mut melody = p.melody_enabled.get();
                if ui
                    .checkbox(&mut melody, "Play melody")
                    .on_hover_text("mute the melodic track without stopping the clock")
                    .changed()
                {
                    p.melody_enabled.set(melody);
                }
            });
        });

        ui.add_space(4.0);
        ui.separator();
        pattern_slots(ui, synth, state);

        ui.add_space(4.0);
        ui.separator();
        ui.label(
            egui::RichText::new("GENERATOR")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        ui.horizontal(|ui| {
            ui.label("Key");
            let root = p.gen_root.get() as usize % 12;
            egui::ComboBox::from_id_salt("gen_root")
                .selected_text(NOTE_NAMES[root])
                .width(52.0)
                .show_ui(ui, |ui| {
                    for (index, name) in NOTE_NAMES.iter().enumerate() {
                        if ui.selectable_label(index == root, *name).clicked() {
                            p.gen_root.set(index as u32);
                        }
                    }
                });
            dropdown::<Scale>(ui, "gen_scale", &p.gen_scale);

            ui.label("Octave");
            integer(ui, &p.gen_octave, 0..=7, "");
            ui.label("Range");
            integer(ui, &p.gen_range, 1..=4, " oct");
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Density", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.75)
                    .size(36.0),
                &p.gen_density,
            )
            .on_hover_text("how often a step has a note rather than a rest");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Jump", 1.0..=12.0)
                    .colour(palette::SEQ)
                    .default(3.0)
                    .size(36.0),
                &p.gen_max_jump,
            )
            .on_hover_text("how far the melody may leap, in scale degrees");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Chord", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.6)
                    .size(36.0),
                &p.gen_chord_bias,
            )
            .on_hover_text("how strongly downbeats land on root, third and fifth");

            ui.vertical(|ui| {
                ui.add_space(8.0);
                if ui
                    .button("↻  New pattern")
                    .on_hover_text("write a fresh melody from these settings")
                    .clicked()
                {
                    // A fresh seed each time, so repeated clicks explore rather
                    // than returning the same tune.
                    synth.regenerate_with_seed(rand_seed());
                }
                ui.label(
                    egui::RichText::new(format!(
                        "seed {:#x}",
                        p.gen_seed.load(std::sync::atomic::Ordering::Relaxed)
                    ))
                    .color(palette::TEXT_DIM)
                    .size(9.0),
                );
            });
        });
    });
}

/// A row of saved patterns to switch between.
///
/// The generator is the point of this synth, but it is happy to throw away a
/// good phrase on the next click. A few slots turn "that one was nice" into
/// something you can come back to, and switching between two of them while the
/// sequencer runs is arranging, not just auditioning.
fn pattern_slots(ui: &mut Ui, synth: &Synth, state: &mut SynthUi) {
    let pattern = synth.pattern();
    let has_notes = pattern.iter().any(|step| step.active);

    // The startup pattern is worth keeping without being asked: it is the one
    // the player is listening to when the panel first opens, and losing it to
    // an idle click on Generate is a poor introduction. Waits for a pattern
    // with something in it, since the first frames can arrive before the
    // engine has generated one.
    if has_notes && state.slots.iter().all(Option::is_none) {
        state.slots[0] = Some(pattern);
        state.active_slot = Some(0);
    }

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("PATTERNS")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        for index in 0..SLOTS {
            let filled = state.slots[index].is_some();
            let active = state.active_slot == Some(index);
            let label = egui::RichText::new(format!("{}", index + 1)).size(12.0);
            let button = egui::Button::selectable(active, label);
            let response = ui
                .add_enabled(filled, button)
                .on_hover_text("play this pattern, from the top of the next loop")
                .on_disabled_hover_text("empty - press Save to put the current pattern here");
            if response.clicked() {
                if let Some(saved) = &state.slots[index] {
                    synth.load_pattern(saved);
                    state.active_slot = Some(index);
                }
            }
        }

        ui.add_space(6.0);
        // Filling the next empty slot is what someone auditioning generated
        // patterns wants: press Save whenever one is good, four times over,
        // without first deciding where it goes. Once they are all full, the
        // one being listened to is the one to replace.
        let target = state
            .slots
            .iter()
            .position(Option::is_none)
            .or(state.active_slot)
            .unwrap_or(0);
        let save = ui
            .add_enabled(has_notes, egui::Button::new("Save"))
            .on_hover_text(format!("store the current pattern in slot {}", target + 1));
        if save.clicked() {
            state.slots[target] = Some(pattern);
            state.active_slot = Some(target);
        }
    });
}

/// The three velocity levels shift-click cycles through.
///
/// Each is exact in the grid's three-bit packing — 0.375, 0.75 and 1.0 are
/// levels 2, 5 and 7 of eight — so a cycled cell round-trips through the
/// mirror unchanged instead of drifting a step on every edit.
const VELOCITIES: [f32; 3] = [0.375, 0.75, 1.0];

/// The velocity a shift-click on this cell should land on next.
///
/// An inactive cell starts the cycle at its softest step, so the whole range
/// is reachable without a plain click first. An active cell advances to the
/// next of the three levels, wrapping past the loudest back to the softest.
/// A velocity that is not exactly one of the three falls back to the
/// softest rather than panicking or freezing the cycle — see the test below
/// for why that fallback is load-bearing rather than defensive-only.
fn next_velocity(cell: Cell) -> f32 {
    if !cell.active {
        return VELOCITIES[0];
    }
    VELOCITIES
        .iter()
        .position(|v| (*v - cell.velocity).abs() < 0.01)
        .map_or(VELOCITIES[0], |i| VELOCITIES[(i + 1) % VELOCITIES.len()])
}

fn drums(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi) {
    let p = &synth.params;
    widgets::section(ui, "DRUMS", palette::DRUM, |ui| {
        ui.horizontal(|ui| {
            let mut enabled = p.drum_enabled.get();
            if ui
                .checkbox(&mut enabled, "Enable")
                .on_hover_text("off by default, so an existing patch sounds exactly as it did")
                .changed()
            {
                p.drum_enabled.set(enabled);
            }

            let mut to_fx = p.drum_to_fx.get();
            if ui
                .checkbox(&mut to_fx, "Through FX")
                .on_hover_text("send the drum bus through delay and reverb instead of past them")
                .changed()
            {
                p.drum_to_fx.set(to_fx);
            }

            ui.label(
                egui::RichText::new("Length")
                    .color(palette::TEXT_DIM)
                    .size(10.0),
            );
            integer(ui, &p.drum_length, 1..=64, " steps");
        });

        ui.add_space(4.0);

        let muted: [bool; PAD_COUNT] = core::array::from_fn(|i| p.pad_mute[i].get());
        // Bound to a local so the snapshot lives for the whole draw, and so
        // the shift-click arm below reads the same grid the user clicked on.
        let grid = synth.drum_grid();
        let hit = widgets::drum_grid(
            ui,
            &grid,
            telemetry.drum_step as usize,
            p.seq_playing.get(),
            &muted,
            state.selected_pad,
        );

        match hit {
            Some(widgets::DrumHit::Mute(pad)) => {
                p.pad_mute[pad].set(!muted[pad]);
            }
            Some(widgets::DrumHit::Select(pad)) => {
                state.selected_pad = pad;
            }
            Some(widgets::DrumHit::Cell {
                step,
                pad,
                shift: false,
            }) => {
                synth.toggle_drum_cell(step, pad);
            }
            Some(widgets::DrumHit::Cell {
                step,
                pad,
                shift: true,
            }) => {
                let mut cell = grid.get(step, pad);
                cell.velocity = next_velocity(cell);
                cell.active = true;
                synth.set_drum_cell(step, pad, cell);
            }
            None => {}
        }

        ui.add_space(4.0);
        ui.separator();

        // One pad's controls, chosen by clicking its name in the grid.
        // `DrumHit::Select` never hands back anything outside 0..PAD_COUNT
        // today, but `selected_pad` is plain UI state with no invariant of
        // its own enforcing that — a saved-session field or a future second
        // writer could hand it a stale value, and indexing pad_level/tune/
        // decay with that would panic instead of just showing the wrong pad.
        let pad = state.selected_pad.min(PAD_COUNT - 1);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(Pad::from_u32(pad as u32).name())
                    .color(palette::TEXT)
                    .size(10.0)
                    .strong(),
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Level", 0.0..=1.0)
                    .colour(palette::DRUM)
                    .default(0.8)
                    .size(36.0),
                &p.pad_level[pad],
            );
            widgets::knob_param(
                ui,
                &KnobSpec::new("Tune", -12.0..=12.0)
                    .colour(palette::DRUM)
                    .unit("st")
                    .default(0.0)
                    .size(36.0),
                &p.pad_tune[pad],
            )
            .on_hover_text("semitones from the pad's own base frequency");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Decay", 0.25..=4.0)
                    .colour(palette::DRUM)
                    .unit("x")
                    .default(1.0)
                    .size(36.0),
                &p.pad_decay[pad],
            )
            .on_hover_text("multiplier on the pad's natural decay, not a time in seconds");

            ui.separator();
            widgets::knob_param(
                ui,
                &KnobSpec::new("Bus", 0.0..=1.0)
                    .colour(palette::DRUM)
                    .default(0.8)
                    .size(36.0),
                &p.drum_level,
            )
            .on_hover_text("level of the whole rack, after the per-pad levels");
        });
    });
}

fn keyboard(ui: &mut Ui, synth: &Synth, state: &mut SynthUi) {
    widgets::section(ui, "KEYBOARD", palette::OSC, |ui| {
        let (hit, _response) = widgets::piano(
            ui,
            Vec2::new(ui.available_width().min(860.0), 76.0),
            state.keyboard_base,
            state.keyboard_octaves,
            &state.sounding,
        );

        // Compare against what was held last frame. Dragging from one key to
        // the next then releases the old note and starts the new one, which is
        // how a glissando should behave; sending note-on every frame would
        // retrigger the envelope 60 times a second.
        if hit != state.held_from_piano {
            if let Some(previous) = state.held_from_piano.take() {
                synth.note_off(previous);
                state.sounding.retain(|&n| n != previous);
            }
            if let Some(note) = hit {
                synth.note_on(note, 0.85);
                state.sounding.push(note);
                state.held_from_piano = Some(note);
            }
        }

        ui.horizontal(|ui| {
            if ui.small_button("◀ oct").clicked() {
                state.keyboard_base = state.keyboard_base.saturating_sub(12);
            }
            if ui.small_button("oct ▶").clicked() {
                state.keyboard_base = (state.keyboard_base + 12).min(108);
            }
            ui.label(
                egui::RichText::new(format!(
                    "from C{}",
                    (state.keyboard_base as i32 / 12) - 1
                ))
                .color(palette::TEXT_DIM)
                .size(10.0),
            );
        });
    });
}

/// A seed with no dependency on `rand`.
///
/// The system clock is plenty: this picks a melody, and nothing about it needs
/// to be unpredictable to an adversary.
fn rand_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED)
        // Mix the low bits up: consecutive nanosecond values differ only in the
        // bottom few bits, and the sequencer's RNG is seeded straight from this.
        .wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(active: bool, velocity: f32) -> Cell {
        Cell { active, velocity }
    }

    #[test]
    fn an_inactive_cell_starts_at_the_softest_level() {
        assert_eq!(next_velocity(cell(false, 0.0)), VELOCITIES[0]);
    }

    #[test]
    fn each_level_advances_to_the_next_and_the_last_wraps_to_the_first() {
        assert_eq!(next_velocity(cell(true, VELOCITIES[0])), VELOCITIES[1]);
        assert_eq!(next_velocity(cell(true, VELOCITIES[1])), VELOCITIES[2]);
        assert_eq!(next_velocity(cell(true, VELOCITIES[2])), VELOCITIES[0]);
    }

    #[test]
    fn an_off_table_velocity_still_lands_on_a_valid_level() {
        // Not hypothetical: `Cell::default().velocity` is 0.8, which is not a
        // member of `VELOCITIES`. It survives today only because every write
        // round-trips through `pack_column`/`unpack_column`, which quantises
        // 0.8 down to 0.75 — `VELOCITIES[1]` — before the UI ever reads it
        // back. This pins the fallback down directly, so a future change to
        // the packing or to `Cell::default` can't silently strand a virgin
        // cell's first shift-click on a value the cycle never visits again.
        let landed = next_velocity(cell(true, Cell::default().velocity));
        assert!(
            VELOCITIES.contains(&landed),
            "an off-table velocity must fall back onto the cycle, got {landed}"
        );
    }
}
