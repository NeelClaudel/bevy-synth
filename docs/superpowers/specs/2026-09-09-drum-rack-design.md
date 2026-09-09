# A drum rack for bevy_synth

Date: 2026-09-09
Status: approved, ready for implementation planning

## Goal

Add eight synthesized drum voices and a step grid that plays them, locked to the
same clock as the melodic sequencer. The grid is programmed by hand; a
generative groove writer is deliberately left for later, and the types here are
shaped so it can be added without rework.

Drums are silent by default, so every existing patch, test and render is
unchanged until a cell is switched on.

## Decisions

Five choices were settled before design, and everything below follows from them.

**The clock moves to the engine.** It currently belongs to the melodic
sequencer, and `engine.rs` reaches through that sequencer to reach it. Two
sequencers cannot both own it, and two clocks fed the same tempo would drift —
`Clock` keeps its position in `f64` precisely because accumulated `f32` error
drops steps, and running two accumulators would reintroduce the error the
existing design documents removing. One clock, two subscribers.

**The drums get their own sequencer, not another track in the existing one.** A
drum cell is a hit with a velocity; a melodic step is a note with a gate, glide
and accent. Forcing both into one `Step` would widen it for fields that are
meaningless half the time. Two types, one clock.

**The drums are synthesized, not sampled.** Kick, snare, hats and the rest fall
out of oscillators, noise, envelopes and filters already in `synth_core`.
Samples would put WAV decoding, asset loading and file I/O into a crate whose
defining property is having no dependencies and no I/O at all. It also means a
game embedding this ships drums with no assets.

**Drum patterns have their own length.** Sixteen drum steps against a twelve
step melody phase against each other and come back around, which is free once
each sequencer keeps its own position counter. Swing and steps-per-beat stay
shared: those describe the feel of the piece, not of one track.

**Drums are dry by default and bypass the synth's drive and soft clip.** They
are finished sounds, not raw voices. See the signal path for the one toggle that
sends them to the effects.

## Clock ownership

`Clock` moves from `Sequencer` into `Engine`. `Sequencer::advance` today takes a
sample count and advances the clock itself; it becomes:

    fn advance(&mut self, adv: Advance, phase: f32, p: &Params) -> SeqOutput

`Advance` is already `{ steps: u32 }` and `Copy`, so handing the same value to
two sequencers copies four bytes. `Clock::phase()` is passed alongside it
because both sequencers need it for gate length and swing.

`Engine::process_block` advances the clock once, then calls each sequencer with
the result. This is the whole reason for the refactor: with the clock inside one
sequencer, a second call would advance it twice and the two tracks would run at
double speed relative to each other.

The call sites in `engine.rs` that change, all mechanically:

| Line | Today | Becomes |
|---|---|---|
| 119 | `sequencer.clock.set_tempo` | `clock.set_tempo` |
| 276 | `sequencer.clock.tempo_bpm` | `clock.tempo_bpm` |
| 352 | `sequencer.on_midi_tick` | `clock.on_midi_tick`, result fed to both |
| 362-373 | `sequencer.clock.start/stop/resume` | `clock.start/stop/resume` |
| 417-421 | `sequencer.clock.is_running` | `clock.is_running` |

The delay's tempo-sync read at line 276 becomes more honest rather than less:
the delay was never following the melody, it was following the clock.

`set_sample_rate` forwards to the clock and both sequencers, as it forwards to
the one sequencer today.

## Transport and muting

`seq_playing` keeps its meaning: it runs and stops the clock, and therefore both
tracks. Two new booleans gate the tracks individually — `melody_enabled`
(default true) and `drum_enabled` (default false). Those defaults reproduce
exactly today's behaviour.

A mute stops a sequencer producing events; it does not stop the clock or reset
the position. Muting a track and unmuting it four bars later comes back in time
rather than at the top, which is what a mute is for. Muting the melody releases
any sounding note through the existing `release_all`.

## Signal path

Today's output stage:

    voices -> drive -> soft clip -> DC block -> delay -> reverb -> gain -> NaN guard

It becomes:

    voices -> drive -> soft clip --+--> DC block -> delay -> reverb --+--> gain -> guard
                                   |                                 |
    drum bus --(drum_to_fx)--------+        (not drum_to_fx) --------+

`drum_to_fx` is a boolean, not a send amount, and that is a change from what was
first sketched. A continuous send does not work against this effects chain:
`FxChain::process_block` mixes dry and wet internally, so a signal fed in at
`send` arrives at the output partly through the chain's own dry path, and the
drum bus's dry level would then move as the send knob turned. A true aux send
needs `FxChain` to grow a wet-only output, which is a change to the effects
chain rather than to the drums. A toggle is exact, costs one bit, and delivers
the case that motivated the send: a dry kick under a reverbed pad.

The drum bus is mono. When it joins post-effects it is added equally to both
channels. Per-pad panning arrives with the per-voice panning already on the
README's list; doing them together is less work than doing them twice.

**Hard bypass.** When no cell is active and no pad is still ringing, the drum
render and the summing are skipped entirely, exactly as delay and reverb
hard-bypass at zero mix. This is what keeps the dry path bit-identical.

## Module layout

`engine.rs` is 1226 lines already, so the drums get their own module,
`crates/synth_core/src/drums/`.

| File | Contents |
|---|---|
| `voice.rs` | `DrumVoice` and the eight synthesis routines |
| `pattern.rs` | `Pad`, `Cell`, `DrumPattern`, `DrumGenSettings` |
| `sequencer.rs` | `DrumSequencer` |
| `mod.rs` | `DrumRack`, owning the eight voices and the sequencer |

One entry point is all `engine.rs` calls:

    fn render_block(&mut self, frames: usize, adv: Advance, phase: f32,
                    p: &Params) -> &[f32]

It fills the rack's own mono scratch buffer and hands back `frames` samples of
it. The engine sums that slice into `left` and `right` at whichever point
`drum_to_fx` selects, so the routing decision stays in `render_chunk` where the
rest of the signal path already lives, rather than being split across two
crates' worth of buffer plumbing.

The scratch buffer is `[f32; BLOCK]`, allocated in `DrumRack::new` like every
other buffer the rack owns; `render_chunk` never asks for more than `BLOCK`
frames. Nothing under `render_block` allocates, locks or panics.

## The drum voices

Eight pads, as a `Pad` enum following the `AtomicEnum` and `from_u32`/`name`
convention every other enum in the crate uses, so the UI's dropdown helper and
the existing publishing helpers need no new machinery.

| Pad | Synthesis | Base |
|---|---|---|
| Kick | Sine with an exponential pitch drop from 4x base to base over 50 ms, plus a 2 ms noise click | 50 Hz |
| Snare | Two detuned triangles for the body, plus noise through a bandpass, on separate decays | 180 Hz |
| Closed hat | Noise through a highpass, short decay | 8 kHz |
| Open hat | The same, with a long decay | 8 kHz |
| Clap | Noise through a bandpass, envelope retriggered three times at 10 ms, then a tail | 1.2 kHz |
| Low tom | Sine with a gentler pitch drop than the kick, longer decay | 100 Hz |
| High tom | The same, tuned up | 180 Hz |
| Rim | A single short bandpassed burst, under 30 ms | 800 Hz |

The clap's triple retrigger is the whole difference between a clap and a short
snare, and it is three lines.

Each pad is monophonic and retriggers by restarting its envelope rather than
allocating, so there is no voice stealing and no fade to manage. A drum machine
that could not retrigger a hi-hat inside its own decay would be useless.

**One choke group:** closed hat silences open hat. This is what makes a hi-hat
part sound like one instrument rather than two overlapping ones.

Envelopes are a local attack-decay, not the existing `Adsr`. A one-shot has no
sustain stage, and reusing `Adsr` would mean holding a note off that never
comes.

## The grid

`DrumPattern` is 64 columns of eight cells, reusing the existing
`MAX_STEPS = 64`. A `Cell` is an active flag and a velocity. It is returned by
copy, for the same reason `Pattern` is: the real one lives on the audio thread
and lending a reference would need a lock.

`DrumGenSettings` sits beside `DrumPattern`, unused, in the shape
`GenerativeSettings` sits beside `Pattern` today, so the groove writer is a new
function rather than a new file layout.

## Publishing the grid without a lock

The melodic pattern publishes one `u32` per step into an array of atomics. The
grid extends that exactly: one `u32` per step holds a whole **column** — eight
pads at four bits each, which is 32 bits precisely.

Within a pad's nibble, bit 3 is the active flag and bits 0-2 are the velocity,
giving eight levels which map to gain as `(v + 1) / 8`, so 0.125 through 1.0.
Ghost, normal and accent need three of those; eight is generous and costs
nothing.

A column is therefore never read half-written, the UI reads it with no
synchronisation, and this is the mechanism the README already explains rather
than a second one to document. Published only when the grid changes, as
`publish_pattern` is today. `drum_position` is published per block alongside the
melodic position.

## Parameters

Added at the five existing touchpoints in `params.rs`: the `Params` field,
`Default`, the `SharedParams` field, the constructor, `snapshot` with its clamp,
and `store`.

| Parameter | Type | Range | Default | Purpose |
|---|---|---|---|---|
| `drum_enabled` | bool | - | false | Gates the drum track |
| `melody_enabled` | bool | - | true | Gates the melodic track |
| `drum_length` | usize | 1..=64 | 16 | Grid length, independent of `seq_length` |
| `drum_level` | f32 | 0.0..=1.0 | 0.8 | Drum bus level |
| `drum_to_fx` | bool | - | false | Route the bus through delay and reverb |

Per pad, eight of each, as `[..; 8]` arrays of atomics:

| Parameter | Range | Default | Purpose |
|---|---|---|---|
| `pad_level` | 0.0..=1.0 | 0.8 | Mix level |
| `pad_tune` | -12.0..=12.0 | 0.0 | Semitones from the pad's base frequency |
| `pad_decay` | 0.25..=4.0 | 1.0 | Multiplier on the pad's natural decay |
| `pad_mute` | bool | false | Silences the pad without disturbing its level |

Tune is in semitones and decay is a multiplier rather than absolute seconds,
because the pads' natural decays differ by an order of magnitude and one
absolute range would be unusable at both ends of it.

Drums read `seq_swing` and `steps_per_beat` rather than owning copies.

## User interface

Two additions to `bevy_synth_ui`.

**The grid**, a new widget: eight labelled rows by `drum_length` columns. Click
toggles a cell, and the playing column is highlighted from `drum_position` the
way the melodic pattern's playing step already is. Shift-click cycles the
velocity through ghost, normal and accent rather than exposing eight levels no
one can aim at.

Each row begins with a mute dot and then the pad's name. The dot toggles
`pad_mute`; the name selects the pad for the knobs below. Two jobs, two targets
— one label doing both would mean every attempt to look at a pad's decay
silenced it.

**A pad section** showing level, tune and decay for the selected pad, using the
existing `widgets::section` and `KnobSpec` helpers. One pad's controls at a
time: twenty-four knobs on screen at once would be the wall of sliders the
panel's own design notes argue against.

One new palette constant for the drums, in the amber range, which no existing
entry occupies.

The four control columns reflow to whatever width the window has, so a fifth
would be the wrong shape for the grid anyway: it is wide rather than tall. It
goes in a full-width row beneath the columns, next to the melodic sequencer that
already sits there, and the window's default height grows from 760 to fit it.

## Presets

Presets do not touch any drum parameter. They set the patch, and by the
reasoning already recorded for the sequencer, a kit and a groove belong to the
piece rather than to the sound — auditioning a pad patch should not silently
retune your kick.

This means the existing preset test, which asserts every preset writes every
patch parameter, must exclude the drum parameters explicitly. Left implicit it
would either fail immediately or quietly stop meaning anything.

## Testing

Unit tests in-module under `#[cfg(test)]`, matching the rest of `synth_core`,
written before the code they cover.

**The clock hoist** — the regression that matters most, because it touches
working code. Both sequencers cross a step boundary on the same sample, across
an internal tempo change and across an external MIDI start. Existing sequencer
and clock tests keep passing unmodified except for the changed `advance`
signature.

**`voice.rs`** — each pad renders non-silent output that decays to near zero
within its expected time. The kick's fundamental falls over its first 50 ms. A
closed hat triggered during an open hat's tail silences it. Retriggering a pad
mid-decay restarts it without a discontinuity larger than one sample step.

**`pattern.rs`** — a column packs and unpacks to the same eight cells; velocity
survives the three-bit round trip at every level; a pattern shorter than 64
loops at its own length.

**`sequencer.rs`** — a 16-step drum grid and a 12-step melody driven from one
clock realign after 48 steps, which is the polyrhythm claim made concrete. Swing
displaces odd steps in the drum grid by the same amount it displaces them in the
melodic one.

**`engine.rs`** — with `drum_enabled` false and an empty grid, `process` output
is bit-identical to the existing golden vector at `engine.rs:1210`. This test is
not re-baselined for this change. If it fails, the drum bus is leaking into the
dry path, and the fix is in the routing.

A second engine test asserts that with `drum_to_fx` false and reverb at full
mix, the drum transient arrives in the output unreverberated.

**Manual verification.** `cargo run`, program a four-on-the-floor kick with hats
on the offbeats, confirm it locks to the melody through a tempo change and that
the hi-hat choke sounds like one instrument. Tests can show a pad decays; they
cannot show a groove sounds good.

## Order of work

Each step is a commit that leaves the synth working and the tests green.

1. Hoist the clock into the engine. No new behaviour, all existing tests pass.
2. The drum voices, tested offline with no sequencer and no engine wiring.
3. The grid, the drum sequencer, and the publishing.
4. Engine routing, including the hard bypass and the golden-vector check.
5. The UI: grid widget, then pad section.

## Out of scope

The generative groove writer, which is the intended next project and is why
`DrumGenSettings` exists here unused. Per-pad panning, which waits for per-voice
panning. Per-source effect sends, which need `FxChain` to grow a wet-only
output. Sample playback. Pattern chaining and song mode. Saving any of this to
disk, which is the project after the groove writer and is better done once,
against the multi-track shape this change creates.
