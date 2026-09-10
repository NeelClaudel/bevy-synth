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
use synth_core::filter::{Slope, SvfMode};
use synth_core::lfo::{LfoTarget, LfoWave};
use synth_core::params::{AtomicEnum, ClockSource, SidechainSource, VoiceMode};
use synth_core::{NoteDivision, Scale, Waveform};

pub mod presets;
mod sections;
pub mod widgets;

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

/// The panel's three top-level pages.
///
/// The panel is a patch editor and a pattern editor and a drum machine, and
/// those are three different jobs done at three different times. Stacked on
/// one surface they made a window taller than a laptop screen, so every job
/// was done through a scrollbar. Tabbed, each one fits.
///
/// Transport and mixer stay outside the tabs: they are the controls you reach
/// for *while* doing any of the three.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    /// Oscillators through effects: the sound itself.
    #[default]
    Synth,
    /// The step sequencer and its pattern slots.
    Sequencer,
    /// The drum rack.
    Drums,
}

impl Tab {
    /// Left to right, in the order the signal is usually built up.
    pub const ALL: [Self; 3] = [Self::Synth, Self::Sequencer, Self::Drums];

    /// The label on the tab.
    pub fn name(self) -> &'static str {
        match self {
            Self::Synth => "SYNTH",
            Self::Sequencer => "SEQ",
            Self::Drums => "DRUMS",
        }
    }
}

/// Panel state that is the UI's own, not the synth's.
#[derive(Resource)]
pub struct SynthUi {
    pub open: bool,
    /// The page currently showing.
    pub tab: Tab,
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
            tab: Tab::default(),
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
ui_enum!(SidechainSource, &SidechainSource::ALL, |s: SidechainSource| s
    .name());
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
        .default_size([980.0, 620.0])
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
            // height: a tab's contents can still outgrow a short window.
            egui::ScrollArea::both().show(ui, |ui| {
                sections::transport(ui, &synth, &telemetry, &mut ui_state);
                ui.add_space(2.0);
                sections::mixer(ui, &synth, &telemetry, &mut ui_state);
                ui.add_space(4.0);

                tab_bar(ui, &mut ui_state);
                ui.add_space(4.0);

                match ui_state.tab {
                    Tab::Synth => {
                        synth_columns(ui, &synth, &telemetry, visible_width);
                        ui.add_space(2.0);
                        sections::keyboard(ui, &synth, &mut ui_state);
                    }
                    Tab::Sequencer => sections::sequencer(ui, &synth, &telemetry, &mut ui_state),
                    Tab::Drums => sections::drums(ui, &synth, &telemetry, &mut ui_state),
                }
            });
        });
    ui_state.open = open;
}

/// The row of page buttons under the mixer.
fn tab_bar(ui: &mut Ui, state: &mut SynthUi) {
    ui.horizontal(|ui| {
        for tab in Tab::ALL {
            let selected = state.tab == tab;
            if ui
                .selectable_label(
                    selected,
                    egui::RichText::new(tab.name()).size(11.0).strong(),
                )
                .clicked()
            {
                state.tab = tab;
            }
        }
    });
}

/// The four columns of synth controls, wrapped onto as many rows as `width`
/// allows.
///
/// Laid out by hand rather than with `horizontal_wrapped`, which wraps
/// individual widgets: each column here is a `vertical` block that claims the
/// whole remaining width as it goes, so wrapping would put every column on a
/// row of its own no matter how wide the window was.
fn synth_columns(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, width: f32) {
    // About the natural width of one column of sections. It only decides how
    // many columns fit, so guessing low is the safe direction to be wrong in:
    // too few columns per row still fits on screen, too many does not.
    const COLUMN_WIDTH: f32 = 265.0;

    let columns: [&dyn Fn(&mut Ui); 4] = [
        &|ui| {
            sections::oscillators(ui, synth);
            sections::voice_section(ui, synth);
        },
        &|ui| {
            sections::filter_section(ui, synth);
            sections::envelopes(ui, synth);
        },
        &|ui| {
            sections::lfo_section(ui, synth);
            presets::section(ui, synth);
        },
        &|ui| {
            sections::delay_section(ui, synth);
            sections::reverb_section(ui, synth);
            sections::compressor_section(ui, synth, telemetry);
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
