//! Custom egui widgets for a synthesizer panel.
//!
//! egui's stock `Slider` would work for every parameter here, and the panel
//! would be unusable. A synth has around forty continuous controls; forty
//! horizontal sliders is a wall of identical grey bars nobody can navigate.
//! Knobs are compact, they group into recognisable sections, and their pointer
//! angle is readable at a glance from across a room — which is what you want
//! when you are listening rather than looking.
//!
//! The displays matter for the same reason. An ADSR is four numbers, and four
//! numbers tell you nothing about the shape you will hear; one curve tells you
//! immediately. Same for the filter: the response plot shows what resonance is
//! doing to the corner far better than "0.72" does.

use std::ops::RangeInclusive;

use egui::{
    Align2, Color32, FontId, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2,
};

use synth_core::env::AdsrSettings;
use synth_core::filter::{Slope, SvfMode};
use synth_core::params::AtomicF32;
use synth_core::{DrumPattern, Pad, PAD_COUNT};

/// The panel's colours, in one place so sections can be re-themed at once.
pub mod palette {
    use egui::Color32;

    pub const PANEL: Color32 = Color32::from_rgb(24, 26, 31);
    pub const SECTION: Color32 = Color32::from_rgb(32, 35, 42);
    pub const TRACK: Color32 = Color32::from_rgb(52, 56, 66);
    pub const TEXT: Color32 = Color32::from_rgb(196, 202, 214);
    pub const TEXT_DIM: Color32 = Color32::from_rgb(128, 136, 152);

    /// Oscillators and pitch.
    pub const OSC: Color32 = Color32::from_rgb(120, 190, 255);
    /// Filter.
    pub const FILTER: Color32 = Color32::from_rgb(255, 176, 84);
    /// Envelopes.
    pub const ENV: Color32 = Color32::from_rgb(140, 230, 160);
    /// LFO and modulation.
    pub const LFO: Color32 = Color32::from_rgb(214, 150, 255);
    /// Sequencer and generator.
    pub const SEQ: Color32 = Color32::from_rgb(255, 122, 140);
    /// Effects. Teal — cool and wet against the warm oscillator and filter
    /// sections, which is roughly what the stage does to the sound.
    pub const FX: Color32 = Color32::from_rgb(94, 224, 208);
    /// Drums. Amber and yellow are already spoken for — `FILTER` is
    /// (255, 176, 84) and `ACCENT` is (255, 214, 102) — so the rack takes the
    /// gap between `ENV`'s mint and `ACCENT`'s yellow.
    pub const DRUM: Color32 = Color32::from_rgb(198, 226, 106);

    pub const ACCENT: Color32 = Color32::from_rgb(255, 214, 102);
    pub const DANGER: Color32 = Color32::from_rgb(255, 96, 96);
}

/// How a knob maps pointer movement onto its value range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scaling {
    /// Even across the range. For anything with a natural midpoint: detune,
    /// resonance, blend amounts.
    Linear,
    /// Even in octaves. Essential for frequency and time: on a linear cutoff
    /// knob, the entire bottom four octaves of the audible range live in the
    /// first 5% of the travel, which makes the knob useless for bass.
    Logarithmic,
}

/// Everything a knob needs besides its current value.
pub struct KnobSpec<'a> {
    pub label: &'a str,
    pub range: RangeInclusive<f32>,
    pub scaling: Scaling,
    pub colour: Color32,
    /// Value shown to the user, formatted. Defaults to three significant-ish
    /// digits plus the unit.
    pub unit: &'a str,
    /// Reset target on double-click.
    pub default: f32,
    pub diameter: f32,
}

impl<'a> KnobSpec<'a> {
    pub fn new(label: &'a str, range: RangeInclusive<f32>) -> Self {
        Self {
            label,
            range,
            scaling: Scaling::Linear,
            colour: palette::ACCENT,
            unit: "",
            default: 0.0,
            diameter: 42.0,
        }
    }

    pub fn log(mut self) -> Self {
        self.scaling = Scaling::Logarithmic;
        self
    }

    pub fn colour(mut self, colour: Color32) -> Self {
        self.colour = colour;
        self
    }

    pub fn unit(mut self, unit: &'a str) -> Self {
        self.unit = unit;
        self
    }

    pub fn default(mut self, default: f32) -> Self {
        self.default = default;
        self
    }

    pub fn size(mut self, diameter: f32) -> Self {
        self.diameter = diameter;
        self
    }
}

/// Maps a value to `0.0..=1.0` within the spec's range and scaling.
fn to_normalised(value: f32, spec: &KnobSpec) -> f32 {
    let (lo, hi) = (*spec.range.start(), *spec.range.end());
    match spec.scaling {
        Scaling::Linear => ((value - lo) / (hi - lo)).clamp(0.0, 1.0),
        Scaling::Logarithmic => {
            // Guard against a zero or negative bound: `log2` of either is not a
            // number, and a NaN here would silently poison the whole widget.
            let lo = lo.max(1e-6);
            let hi = hi.max(lo * 1.0001);
            let value = value.clamp(lo, hi);
            ((value / lo).log2() / (hi / lo).log2()).clamp(0.0, 1.0)
        }
    }
}

fn from_normalised(t: f32, spec: &KnobSpec) -> f32 {
    let (lo, hi) = (*spec.range.start(), *spec.range.end());
    let t = t.clamp(0.0, 1.0);
    match spec.scaling {
        Scaling::Linear => lo + (hi - lo) * t,
        Scaling::Logarithmic => {
            let lo = lo.max(1e-6);
            let hi = hi.max(lo * 1.0001);
            lo * (hi / lo).powf(t)
        }
    }
}

/// Formats a value compactly, choosing precision from magnitude.
///
/// A cutoff of "8532.7 Hz" is noise: nobody tunes a filter to a tenth of a
/// hertz. Precision that exceeds what the ear can resolve makes a panel harder
/// to read, not more informative.
fn format_value(value: f32, unit: &str) -> String {
    let magnitude = value.abs();
    let text = if magnitude >= 1000.0 {
        format!("{:.0}", value)
    } else if magnitude >= 100.0 {
        format!("{:.0}", value)
    } else if magnitude >= 10.0 {
        format!("{:.1}", value)
    } else if magnitude >= 1.0 {
        format!("{:.2}", value)
    } else {
        format!("{:.3}", value)
    };
    if unit.is_empty() {
        text
    } else {
        format!("{text} {unit}")
    }
}

/// A rotary knob.
///
/// Drag vertically to turn. Hold Shift for fine control, double-click to reset
/// to the spec's default.
pub fn knob(ui: &mut Ui, spec: &KnobSpec, value: &mut f32) -> Response {
    let size = Vec2::new(spec.diameter, spec.diameter + 26.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click_and_drag());

    let mut normalised = to_normalised(*value, spec);

    if response.dragged() {
        // Vertical drag: up increases. Horizontal drag is deliberately ignored
        // — mixing both axes makes a knob feel vague, and vertical alone is
        // what every plugin host has trained people to expect.
        let delta = -response.drag_delta().y;
        // Full travel over ~200 px, or ~800 px with Shift held for fine work.
        let travel = if ui.input(|i| i.modifiers.shift) {
            800.0
        } else {
            200.0
        };
        normalised = (normalised + delta / travel).clamp(0.0, 1.0);
        *value = from_normalised(normalised, spec);
        response.mark_changed();
    }

    if response.double_clicked() {
        *value = spec.default;
        normalised = to_normalised(*value, spec);
        response.mark_changed();
    }

    // --- Painting ---

    let knob_rect = Rect::from_min_size(rect.min, Vec2::splat(spec.diameter));
    let centre = knob_rect.center();
    let radius = spec.diameter * 0.5 - 3.0;

    // The dial sweeps 270 degrees with a gap at the bottom, so the pointer's
    // direction is never ambiguous — a full 360 degree sweep makes minimum and
    // maximum look identical.
    const START: f32 = std::f32::consts::PI * 0.75;
    const SWEEP: f32 = std::f32::consts::PI * 1.5;

    let painter = ui.painter();

    // Track.
    painter.add(Shape::line(
        arc_points(centre, radius, START, SWEEP, 32),
        Stroke::new(3.0, palette::TRACK),
    ));

    // Filled portion.
    if normalised > 0.001 {
        painter.add(Shape::line(
            arc_points(centre, radius, START, SWEEP * normalised, 32),
            Stroke::new(3.0, spec.colour),
        ));
    }

    // Body and pointer.
    painter.circle_filled(centre, radius - 4.0, palette::SECTION);
    let angle = START + SWEEP * normalised;
    let pointer = Pos2::new(
        centre.x + angle.cos() * (radius - 5.0),
        centre.y + angle.sin() * (radius - 5.0),
    );
    let inner = Pos2::new(
        centre.x + angle.cos() * (radius * 0.35),
        centre.y + angle.sin() * (radius * 0.35),
    );
    painter.line_segment([inner, pointer], Stroke::new(2.0, spec.colour));

    // Label and value.
    let label_colour = if response.hovered() {
        palette::TEXT
    } else {
        palette::TEXT_DIM
    };
    painter.text(
        Pos2::new(rect.center().x, knob_rect.max.y + 7.0),
        Align2::CENTER_CENTER,
        spec.label,
        FontId::proportional(10.0),
        label_colour,
    );
    painter.text(
        Pos2::new(rect.center().x, knob_rect.max.y + 19.0),
        Align2::CENTER_CENTER,
        format_value(*value, spec.unit),
        FontId::monospace(9.5),
        if response.dragged() {
            spec.colour
        } else {
            palette::TEXT_DIM
        },
    );

    response.on_hover_text(format!(
        "{}\ndrag to change, shift for fine, double-click to reset",
        spec.label
    ))
}

/// A knob bound directly to an atomic parameter.
///
/// The panel has around forty of these; going through a local `f32` at every
/// call site would triple the panel's length for no gain.
pub fn knob_param(ui: &mut Ui, spec: &KnobSpec, param: &AtomicF32) -> Response {
    let mut value = param.get();
    let response = knob(ui, spec, &mut value);
    if response.changed() {
        param.set(value);
    }
    response
}

/// Points along an arc, for stroking.
fn arc_points(centre: Pos2, radius: f32, start: f32, sweep: f32, segments: usize) -> Vec<Pos2> {
    (0..=segments)
        .map(|i| {
            let angle = start + sweep * (i as f32 / segments as f32);
            Pos2::new(
                centre.x + angle.cos() * radius,
                centre.y + angle.sin() * radius,
            )
        })
        .collect()
}

/// Draws an ADSR envelope as the shape it will actually produce.
///
/// The segment widths are proportional to their real times, so a long release
/// looks long. `progress` optionally marks how far a currently sounding note
/// has got, which makes the relationship between the knobs and what you are
/// hearing immediate.
pub fn adsr_display(ui: &mut Ui, size: Vec2, settings: &AdsrSettings, colour: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 3.0, palette::PANEL);

    let AdsrSettings {
        attack,
        decay,
        sustain,
        release,
    } = *settings;

    // A fixed hold length so sustain is visible even at zero; without it a
    // patch with a short decay would show a vertical cliff and nothing else.
    let hold = 0.25;
    let total = (attack + decay + hold + release).max(0.001);

    let x_at = |t: f32| rect.left() + rect.width() * (t / total);
    let y_at = |level: f32| rect.bottom() - rect.height() * level.clamp(0.0, 1.0);

    let mut points = vec![Pos2::new(x_at(0.0), y_at(0.0))];

    // Attack and decay are curves, not lines, so draw them as such — the whole
    // point of the display is to show the shape.
    const STEPS: usize = 12;
    for i in 1..=STEPS {
        let t = i as f32 / STEPS as f32;
        // Matches the envelope's near-linear attack shape.
        let level = t.powf(0.75);
        points.push(Pos2::new(x_at(attack * t), y_at(level)));
    }
    for i in 1..=STEPS {
        let t = i as f32 / STEPS as f32;
        // Exponential decay toward the sustain level.
        let level = sustain + (1.0 - sustain) * (-4.0 * t).exp();
        points.push(Pos2::new(x_at(attack + decay * t), y_at(level)));
    }

    points.push(Pos2::new(x_at(attack + decay + hold), y_at(sustain)));

    for i in 1..=STEPS {
        let t = i as f32 / STEPS as f32;
        let level = sustain * (-4.0 * t).exp();
        points.push(Pos2::new(
            x_at(attack + decay + hold + release * t),
            y_at(level),
        ));
    }

    // Fill under the curve, so the shape reads as a body rather than a wire.
    let mut fill = points.clone();
    fill.push(Pos2::new(x_at(total), rect.bottom()));
    fill.push(Pos2::new(rect.left(), rect.bottom()));
    painter.add(Shape::convex_polygon(
        fill,
        colour.gamma_multiply(0.15),
        Stroke::NONE,
    ));

    painter.add(Shape::line(points, Stroke::new(1.6, colour)));

    // Mark the note-off point: the boundary between what you hear while holding
    // a key and what you hear after letting go.
    let release_x = x_at(attack + decay + hold);
    painter.line_segment(
        [
            Pos2::new(release_x, rect.top() + 2.0),
            Pos2::new(release_x, rect.bottom() - 2.0),
        ],
        Stroke::new(1.0, palette::TEXT_DIM.gamma_multiply(0.5)),
    );

    response
}

/// Draws the filter's magnitude response.
///
/// Computed from the analogue prototype of the state-variable filter rather
/// than measured from the running audio: it is exact, costs a few dozen
/// multiplies, and updates the instant a knob moves rather than after an
/// analysis window.
pub fn filter_display(
    ui: &mut Ui,
    size: Vec2,
    mode: SvfMode,
    slope: Slope,
    cutoff: f32,
    resonance: f32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 3.0, palette::PANEL);

    let (min_hz, max_hz) = (20.0f32, 20000.0f32);
    let decades = (max_hz / min_hz).log10();

    // Octave gridlines, so the plot can be read as pitch.
    for &hz in &[100.0f32, 1000.0, 10000.0] {
        let t = (hz / min_hz).log10() / decades;
        let x = rect.left() + rect.width() * t;
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.0, palette::TRACK.gamma_multiply(0.5)),
        );
    }
    // Unity gain line.
    let unity_y = rect.bottom() - rect.height() * 0.5;
    painter.line_segment(
        [
            Pos2::new(rect.left(), unity_y),
            Pos2::new(rect.right(), unity_y),
        ],
        Stroke::new(1.0, palette::TRACK.gamma_multiply(0.5)),
    );

    let k = (2.0 - 2.0 * resonance.clamp(0.0, 1.0)).max(0.025);
    let k2 = (2.0 - 2.0 * (resonance.clamp(0.0, 1.0) * 0.5)).max(0.025);

    let points: Vec<Pos2> = (0..=120)
        .map(|i| {
            let t = i as f32 / 120.0;
            let hz = min_hz * 10f32.powf(decades * t);
            let mut gain = svf_magnitude(hz / cutoff.max(1.0), k, mode);
            if slope == Slope::Db24 {
                // The second stage runs at half the resonance, matching what
                // `Filter::set_params` actually does.
                gain *= svf_magnitude(hz / cutoff.max(1.0), k2, mode);
            }

            // Plot in decibels: linear magnitude squashes everything
            // interesting into the top of the display.
            let db = 20.0 * gain.max(1e-5).log10();
            // -48 dB at the bottom, +24 dB at the top, unity halfway.
            let normalised = ((db + 48.0) / 72.0).clamp(0.0, 1.0);
            Pos2::new(
                rect.left() + rect.width() * t,
                rect.bottom() - rect.height() * normalised,
            )
        })
        .collect();

    painter.add(Shape::line(points, Stroke::new(1.8, palette::FILTER)));

    // Mark the cutoff itself.
    let cutoff_t = (cutoff.clamp(min_hz, max_hz) / min_hz).log10() / decades;
    let cutoff_x = rect.left() + rect.width() * cutoff_t;
    painter.line_segment(
        [
            Pos2::new(cutoff_x, rect.top()),
            Pos2::new(cutoff_x, rect.bottom()),
        ],
        Stroke::new(1.0, palette::FILTER.gamma_multiply(0.6)),
    );

    painter.text(
        Pos2::new(rect.right() - 4.0, rect.top() + 3.0),
        Align2::RIGHT_TOP,
        format!("{} {}", mode.name(), if slope == Slope::Db24 { "24dB" } else { "12dB" }),
        FontId::monospace(9.0),
        palette::TEXT_DIM,
    );

    response
}

/// Magnitude of the analogue SVF prototype at a normalised frequency.
///
/// `omega` is frequency divided by cutoff, `k` the damping (1/Q).
fn svf_magnitude(omega: f32, k: f32, mode: SvfMode) -> f32 {
    let w2 = omega * omega;
    let denominator = ((1.0 - w2).powi(2) + (k * omega).powi(2)).sqrt().max(1e-9);
    match mode {
        SvfMode::Lowpass => 1.0 / denominator,
        SvfMode::Highpass => w2 / denominator,
        SvfMode::Bandpass => (k * omega) / denominator,
        SvfMode::Notch => (1.0 - w2).abs() / denominator,
        SvfMode::Peak => (1.0 + w2) / denominator,
        SvfMode::Bypass => 1.0,
    }
}

/// A horizontal output level meter with a slow-falling peak hold.
///
/// The hold matters: audio peaks last a millisecond or two, and a meter that
/// tracks them exactly flickers too fast to read. Falling slowly leaves the
/// peak visible long enough to see.
pub fn level_meter(ui: &mut Ui, size: Vec2, level: f32, hold: &mut f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);

    *hold = hold.max(level) - 0.008;
    *hold = hold.clamp(0.0, 1.0);

    painter.rect_filled(rect, 2.0, palette::PANEL);

    // Decibel scale, because a linear meter spends most of its length on the
    // top 6 dB and shows nothing about quiet passages.
    let to_x = |value: f32| {
        let db = 20.0 * value.max(1e-4).log10();
        let t = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
        rect.left() + rect.width() * t
    };

    let filled = Rect::from_min_max(rect.min, Pos2::new(to_x(level), rect.max.y));
    let colour = if level > 0.98 {
        palette::DANGER
    } else if level > 0.7 {
        palette::ACCENT
    } else {
        palette::ENV
    };
    painter.rect_filled(filled, 2.0, colour);

    if *hold > 0.001 {
        let x = to_x(*hold);
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.5, palette::TEXT),
        );
    }

    response
}

/// The sequencer's pattern as a grid of steps.
///
/// Returns the index of a step that was clicked, for the caller to toggle.
/// Downbeats are marked, the playing step is highlighted, and each active step
/// shows its note name — the generator writes the pattern, and being able to
/// read what it wrote is most of what makes it trustworthy.
pub fn step_grid(
    ui: &mut Ui,
    steps: &[synth_core::sequencer::Step],
    current: usize,
    playing: bool,
) -> Option<usize> {
    let mut clicked = None;

    // Sixteen to a row: the bar length everything else here assumes, so
    // wrapping on it keeps downbeats aligned vertically.
    const PER_ROW: usize = 16;

    ui.vertical(|ui| {
        for row in steps.chunks(PER_ROW).enumerate().map(|(i, c)| (i, c)) {
            let (row_index, row_steps) = row;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for (column, step) in row_steps.iter().enumerate() {
                    let index = row_index * PER_ROW + column;
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::new(30.0, 34.0), Sense::click());

                    if response.clicked() {
                        clicked = Some(index);
                    }

                    let is_current = playing && index == current;
                    let painter = ui.painter_at(rect);

                    let background = if is_current {
                        palette::SEQ
                    } else if step.active {
                        palette::SEQ.gamma_multiply(0.45)
                    } else if index % 4 == 0 {
                        // Downbeats stay visible even when empty, so the bar
                        // structure is readable in a sparse pattern.
                        palette::TRACK
                    } else {
                        palette::PANEL
                    };
                    painter.rect_filled(rect, 3.0, background);

                    if response.hovered() {
                        painter.rect_stroke(
                            rect,
                            3.0,
                            Stroke::new(1.0, palette::TEXT),
                            egui::StrokeKind::Inside,
                        );
                    }

                    if step.active {
                        let note = synth_core::Note::new(step.note, step.velocity);
                        let (name, octave) = note.name();
                        let text_colour = if is_current {
                            palette::PANEL
                        } else {
                            palette::TEXT
                        };
                        painter.text(
                            rect.center() - Vec2::new(0.0, 5.0),
                            Align2::CENTER_CENTER,
                            format!("{name}{octave}"),
                            FontId::monospace(9.5),
                            text_colour,
                        );
                        // Velocity as a small bar: dynamics are half of what
                        // makes a generated line sound played.
                        let width = rect.width() * 0.7 * step.velocity;
                        let bar = Rect::from_min_size(
                            Pos2::new(rect.center().x - width * 0.5, rect.bottom() - 8.0),
                            Vec2::new(width, 3.0),
                        );
                        painter.rect_filled(bar, 1.0, text_colour.gamma_multiply(0.7));
                    }
                }
            });
            ui.add_space(3.0);
        }
    });

    clicked
}

/// Which of a drum row's three targets the pointer hit.
///
/// The widget reports rather than acts: it has no `&Synth`, and the caller
/// owns the difference between toggling a cell and cycling its velocity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrumHit {
    /// A cell. `shift` means cycle the velocity instead of toggling.
    Cell { step: usize, pad: usize, shift: bool },
    /// The mute dot at the head of a row.
    Mute(usize),
    /// The pad's name, which selects it for the knobs below.
    Select(usize),
}

/// The drum rack's pattern: eight labelled rows by `grid.len()` columns.
///
/// Unlike `step_grid` this does not wrap at sixteen. A row is a pad, and a pad
/// broken across two rows stops being readable as one instrument — so a long
/// pattern gets wide instead, and the panel's scroll area carries it.
pub fn drum_grid(
    ui: &mut Ui,
    grid: &DrumPattern,
    current: usize,
    playing: bool,
    muted: &[bool; PAD_COUNT],
    selected: usize,
) -> Option<DrumHit> {
    let mut hit = None;

    const CELL: Vec2 = Vec2::new(22.0, 20.0);
    const NAME_WIDTH: f32 = 58.0;

    // Read once, outside the loop: the modifier belongs to the click, and
    // asking egui per cell would be the same answer sixty-four times a row.
    let shift = ui.input(|i| i.modifiers.shift);

    ui.vertical(|ui| {
        for (pad, &pad_muted) in muted.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;

                // Mute and select are two targets, not one. A single label
                // doing both would mean every attempt to look at a pad's decay
                // silenced it.
                let (dot, dot_response) =
                    ui.allocate_exact_size(Vec2::splat(12.0), Sense::click());
                if dot_response.clicked() {
                    hit = Some(DrumHit::Mute(pad));
                }
                let colour = if pad_muted {
                    palette::TRACK
                } else {
                    palette::DRUM
                };
                ui.painter_at(dot).circle_filled(dot.center(), 4.0, colour);
                dot_response.on_hover_text("mute this pad");

                let (name, name_response) =
                    ui.allocate_exact_size(Vec2::new(NAME_WIDTH, CELL.y), Sense::click());
                if name_response.clicked() {
                    hit = Some(DrumHit::Select(pad));
                }
                let painter = ui.painter_at(name);
                if pad == selected {
                    painter.rect_filled(name, 3.0, palette::TRACK);
                }
                painter.text(
                    Pos2::new(name.left() + 5.0, name.center().y),
                    Align2::LEFT_CENTER,
                    Pad::from_u32(pad as u32).name(),
                    FontId::monospace(9.5),
                    if pad_muted {
                        palette::TEXT_DIM
                    } else {
                        palette::TEXT
                    },
                );

                for step in 0..grid.len() {
                    let cell = grid.get(step, pad);
                    let (rect, response) = ui.allocate_exact_size(CELL, Sense::click());
                    if response.clicked() {
                        hit = Some(DrumHit::Cell { step, pad, shift });
                    }

                    let painter = ui.painter_at(rect);
                    let background = if cell.active {
                        // Velocity as brightness. The three levels shift-click
                        // cycles through have to be tellable apart at a glance,
                        // and a number in a 22-pixel box is not readable.
                        let lit = palette::DRUM.gamma_multiply(0.3 + 0.7 * cell.velocity);
                        if pad_muted {
                            lit.gamma_multiply(0.3)
                        } else {
                            lit
                        }
                    } else if step % 4 == 0 {
                        // Downbeats stay visible when empty, so the bar
                        // structure is readable in a sparse pattern.
                        palette::TRACK
                    } else {
                        palette::PANEL
                    };
                    painter.rect_filled(rect, 3.0, background);

                    // The playhead is an outline rather than a fill, so it
                    // stays legible over a lit cell instead of replacing it —
                    // which would hide the velocity exactly when playing.
                    if playing && step == current {
                        painter.rect_stroke(
                            rect,
                            3.0,
                            Stroke::new(1.5, palette::ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    } else if response.hovered() {
                        painter.rect_stroke(
                            rect,
                            3.0,
                            Stroke::new(1.0, palette::TEXT),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            });
            ui.add_space(2.0);
        }
    });

    hit
}

/// A clickable piano keyboard.
///
/// Returns the note under the pointer while a mouse button is held, and `None`
/// otherwise. The caller compares that against the note it last saw to decide
/// when to send note-on and note-off — which is also what makes dragging across
/// the keys glissando rather than retrigger the same note.
pub fn piano(
    ui: &mut Ui,
    size: Vec2,
    base_note: u8,
    octaves: u32,
    sounding: &[u8],
) -> (Option<u8>, Response) {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    // Seven white keys to an octave; the black keys sit between them rather
    // than taking width of their own.
    let white_count = (octaves * 7).max(1);
    let white_width = rect.width() / white_count as f32;
    let black_width = white_width * 0.62;
    let black_height = rect.height() * 0.6;

    /// Semitone offset of each white key within an octave.
    const WHITE_OFFSETS: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
    /// Semitone offset of each black key, and which white key it follows.
    const BLACK_OFFSETS: [(u8, usize); 5] = [(1, 0), (3, 1), (6, 3), (8, 4), (10, 5)];

    let pointer = if response.is_pointer_button_down_on() {
        response.interact_pointer_pos()
    } else {
        None
    };
    let mut hit = None;

    // White keys first, so black keys paint on top.
    for i in 0..white_count {
        let octave = i / 7;
        let index = (i % 7) as usize;
        let note = base_note as u32 + octave * 12 + WHITE_OFFSETS[index] as u32;
        if note > 127 {
            continue;
        }
        let note = note as u8;

        let key = Rect::from_min_size(
            Pos2::new(rect.left() + white_width * i as f32, rect.top()),
            Vec2::new(white_width - 1.0, rect.height()),
        );

        let is_sounding = sounding.contains(&note);
        let colour = if is_sounding {
            palette::ACCENT
        } else {
            Color32::from_rgb(232, 234, 240)
        };
        painter.rect_filled(key, 2.0, colour);

        if let Some(position) = pointer {
            if key.contains(position) {
                hit = Some(note);
            }
        }

        // Label every C, so the octave is findable without counting.
        if index == 0 {
            painter.text(
                Pos2::new(key.center().x, key.bottom() - 9.0),
                Align2::CENTER_CENTER,
                format!("C{}", (note as i32 / 12) - 1),
                FontId::monospace(9.0),
                Color32::from_rgb(120, 124, 132),
            );
        }
    }

    for i in 0..white_count {
        let octave = i / 7;
        let index = (i % 7) as usize;
        let Some(&(semitone, _)) = BLACK_OFFSETS.iter().find(|(_, after)| *after == index) else {
            continue;
        };
        let note = base_note as u32 + octave * 12 + semitone as u32;
        if note > 127 {
            continue;
        }
        let note = note as u8;

        let key = Rect::from_min_size(
            Pos2::new(
                rect.left() + white_width * (i as f32 + 1.0) - black_width * 0.5,
                rect.top(),
            ),
            Vec2::new(black_width, black_height),
        );

        let is_sounding = sounding.contains(&note);
        let colour = if is_sounding {
            palette::ACCENT.gamma_multiply(0.8)
        } else {
            Color32::from_rgb(28, 30, 36)
        };
        painter.rect_filled(key, 2.0, colour);

        // Black keys are checked after the white ones and overwrite the hit,
        // because they are drawn on top and the pointer is over both.
        if let Some(position) = pointer {
            if key.contains(position) {
                hit = Some(note);
            }
        }
    }

    (hit, response)
}

/// A labelled section frame, so the panel groups into recognisable blocks.
pub fn section<R>(
    ui: &mut Ui,
    title: &str,
    colour: Color32,
    contents: impl FnOnce(&mut Ui) -> R,
) -> R {
    egui::Frame::new()
        .fill(palette::SECTION)
        .corner_radius(5.0)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // A colour tab per section: the eye finds "the orange block" far
                // faster than it reads the word "Filter".
                let (tab, _) = ui.allocate_exact_size(Vec2::new(3.0, 12.0), Sense::hover());
                ui.painter().rect_filled(tab, 1.5, colour);
                ui.label(
                    egui::RichText::new(title)
                        .color(palette::TEXT)
                        .size(11.0)
                        .strong(),
                );
            });
            ui.add_space(4.0);
            contents(ui)
        })
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> KnobSpec<'static> {
        KnobSpec::new("test", 20.0..=20000.0).log()
    }

    #[test]
    fn normalisation_round_trips() {
        let spec = spec();
        for value in [20.0f32, 100.0, 440.0, 5000.0, 20000.0] {
            let back = from_normalised(to_normalised(value, &spec), &spec);
            assert!(
                (back - value).abs() < value * 0.001,
                "{value} came back as {back}"
            );
        }
    }

    /// The reason for logarithmic scaling: on a linear knob, 1 kHz sits at 5%
    /// of the travel and the whole usable bass range is unreachable.
    #[test]
    fn logarithmic_puts_the_midpoint_where_the_ear_expects() {
        let spec = spec();
        let midpoint = from_normalised(0.5, &spec);
        // Geometric mean of 20 and 20000 is ~632 Hz.
        assert!(
            (500.0..800.0).contains(&midpoint),
            "midpoint was {midpoint} Hz"
        );

        let linear = KnobSpec::new("test", 20.0..=20000.0);
        assert!(from_normalised(0.5, &linear) > 9000.0);
    }

    #[test]
    fn normalisation_clamps_out_of_range_values() {
        let spec = spec();
        assert_eq!(to_normalised(-100.0, &spec), 0.0);
        assert_eq!(to_normalised(1e9, &spec), 1.0);
    }

    /// A zero lower bound would make `log2` return infinity and poison every
    /// subsequent calculation with NaN.
    #[test]
    fn logarithmic_survives_a_zero_lower_bound() {
        let spec = KnobSpec::new("time", 0.0..=20.0).log();
        for t in [0.0f32, 0.5, 1.0] {
            assert!(from_normalised(t, &spec).is_finite());
        }
        assert!(to_normalised(0.0, &spec).is_finite());
    }

    #[test]
    fn filter_response_matches_the_expected_shapes() {
        // Well below cutoff, a lowpass passes and a highpass does not.
        assert!(svf_magnitude(0.01, 1.0, SvfMode::Lowpass) > 0.99);
        assert!(svf_magnitude(0.01, 1.0, SvfMode::Highpass) < 0.01);
        // Well above, the reverse.
        assert!(svf_magnitude(100.0, 1.0, SvfMode::Lowpass) < 0.01);
        assert!(svf_magnitude(100.0, 1.0, SvfMode::Highpass) > 0.99);
        // Bandpass peaks at the corner; notch nulls there.
        assert!(svf_magnitude(1.0, 1.0, SvfMode::Bandpass) > 0.99);
        assert!(svf_magnitude(1.0, 1.0, SvfMode::Notch) < 0.01);
    }

    #[test]
    fn resonance_raises_the_displayed_peak() {
        let damped = svf_magnitude(1.0, 2.0, SvfMode::Lowpass);
        let resonant = svf_magnitude(1.0, 0.1, SvfMode::Lowpass);
        assert!(resonant > damped * 5.0);
    }

    #[test]
    fn every_response_value_is_finite() {
        for mode in SvfMode::ALL {
            for k in [0.025f32, 0.5, 1.0, 2.0] {
                for omega in [0.0f32, 0.001, 1.0, 1000.0] {
                    let g = svf_magnitude(omega, k, mode);
                    assert!(g.is_finite(), "{mode:?} k={k} w={omega} gave {g}");
                }
            }
        }
    }

    #[test]
    fn values_format_without_absurd_precision() {
        assert_eq!(format_value(8532.7, "Hz"), "8533 Hz");
        assert_eq!(format_value(0.25, ""), "0.250");
        assert_eq!(format_value(12.5, "st"), "12.5 st");
    }
}
