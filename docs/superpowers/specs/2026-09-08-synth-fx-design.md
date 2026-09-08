# Reverb and delay for bevy_synth

Date: 2026-09-08
Status: approved, ready for implementation planning

## Goal

Add a stereo effects stage — a tempo-syncable stereo delay and a plate reverb —
to the end of the synth's signal chain. Effects are off by default, so every
existing patch, test and render is unchanged until a mix knob is turned up.

## Decisions

Four choices were settled before design, and everything below follows from them.

**The effects stage is stereo; the voices stay mono.** Reverb and delay are
where a mono synth earns a stereo image. Making the voices themselves stereo
(per-voice pan) would rewrite the voice mixer and every test that calls
`process` with a mono buffer, for a payoff that is not reverb or delay. So the
mono voice sum feeds a stereo effects stage, and stereo leaves the engine from
there.

**Delay time is switchable between free milliseconds and a note division.**
Synced-only is wrong when the sequencer is stopped and you are playing the
keyboard; free-only turns tempo matching into arithmetic you redo on every BPM
change. Both modes, one toggle.

**The reverb is a Dattorro plate.** Its stereo output comes from tapping one
shared tank at fixed points, so the two channels are genuinely decorrelated
rather than an offset copy of each other — which is exactly what the stereo
decision above asks for. Freeverb is simpler but its stereo is a 23-sample
offset hack and its tail rings on percussive input. An FDN sounds better still,
but its stability and tail density are tuning work with no reference
implementation to check against.

**Defaults are dry.** Every mix parameter defaults to 0.0. This is what makes
the change additive rather than a behavioural break.

## Signal path

The output stage in `Engine::process` is today:

    voices -> drive -> soft clip -> DC block -> master gain -> NaN guard

It becomes:

    voices -> drive -> soft clip -> DC block
           -> delay -> reverb            (stereo from here)
           -> master gain -> NaN guard

Two placement choices, both deliberate:

*Effects sit after the DC blocker and before master gain.* The feedback loops
therefore see a pre-fader signal, so master gain stays a pure output trim —
pulling the fader down does not change how long the reverb tail is. And the soft
clipper's harmonics get reverberated, rather than the reverb tail getting
clipped.

*Delay runs before reverb.* The reverb then smears the repeats, which is the
conventional order. Reversed, the delay would multiply the entire tail and turn
it to mud.

## Module layout

`engine.rs` is already 902 lines and the effects are self-contained, so they get
their own module: `crates/synth_core/src/fx/`.

| File | Contents |
|---|---|
| `line.rs` | `DelayLine` — power-of-two ring buffer, integer and fractional reads |
| `delay.rs` | `StereoDelay`, `NoteDivision` |
| `reverb.rs` | `PlateReverb` |
| `mod.rs` | `FxChain`, owning both, exposing one `process_block` |

`FxChain::process_block(&mut self, left: &mut [f32], right: &mut [f32], params:
&Params, tempo_bpm: f32)` is the only entry point `engine.rs` calls.

### Real-time safety

Every buffer is allocated in `FxChain::new` and reused. The tank and delay lines
are sized from the sample rate, so `set_sample_rate` reallocates them — that
method is already documented as not real-time safe and is called from the setup
path, so this is consistent with the existing contract rather than a new
exception to it. Nothing under `process` allocates, locks or panics.

Delay buffers are sized for the 2.0 s maximum. The Dattorro tank's delay lengths
are specified against the paper's 29761 Hz sample rate and scale linearly with
the actual rate; at 48 kHz the tank plus pre-delay comes to roughly 130 KB.

## Public interface

One internal `render_stereo(&mut self, left: &mut [f32], right: &mut [f32])`
does the whole job. Both public methods become thin wrappers over it:

- `process(out)` renders stereo and writes `(l + r) * 0.5`. With the mixes at
  their 0.0 defaults both channels are bit-identical to today's mono signal, so
  this is exactly the current output.
- `process_stereo_interleaved(out)` renders stereo and interleaves. It stops
  being a duplication hack and becomes the real path.

Neither signature changes.

### Host

`synth_audio/src/host.rs` currently calls mono `process` and copies the result
to every channel. Left alone, it would make the stereo effects inaudible in the
actual app, so it changes: channel 0 takes L, channel 1 takes R, and any channel
beyond the second takes the mono downmix. A genuinely mono device keeps the
existing `process` path. `MAX_SCRATCH` doubles to 16384, because the scratch
buffer now holds interleaved frames rather than mono samples.

## Parameters

Twelve new parameters, each added at the five existing touchpoints in
`params.rs`: the `Params` field, `Default`, the `SharedParams` field, the
constructor, `snapshot` (with its clamp), and `store`.

### Delay

| Parameter | Type | Range | Purpose |
|---|---|---|---|
| `delay_mix` | f32 | 0.0..=1.0 | Wet/dry. 0.0 by default |
| `delay_sync` | bool | — | Note division when true, free time when false |
| `delay_time` | f32 | 0.001..=2.0 s | Delay time in free mode |
| `delay_division` | enum | `NoteDivision` | Delay time in sync mode |
| `delay_feedback` | f32 | 0.0..=0.95 | Repeat count |
| `delay_damping` | f32 | 0.0..=1.0 | One-pole lowpass in the feedback loop |
| `delay_ping_pong` | bool | — | Cross-feed repeats between channels |

`delay_feedback` is clamped strictly below 1.0 in `snapshot`, so no UI bug or
preset typo can produce a runaway loop.

`delay_damping` is not optional polish. Without a lowpass in the feedback path
every repeat carries the full high end and a long tail turns into hiss.

### Reverb

| Parameter | Type | Range | Purpose |
|---|---|---|---|
| `reverb_mix` | f32 | 0.0..=1.0 | Wet/dry. 0.0 by default |
| `reverb_size` | f32 | 0.0..=1.0 | Tank decay coefficient |
| `reverb_damping` | f32 | 0.0..=1.0 | High-frequency absorption in the tank |
| `reverb_predelay` | f32 | 0.0..=0.25 s | Gap before the tail starts |
| `reverb_width` | f32 | 0.0..=1.0 | Stereo spread of the tail |

`reverb_width` blends between the two tank taps summed to mono at 0.0 and the
taps sent hard to their own channels at 1.0. It affects the wet signal only; the
dry path is untouched by it.

### `NoteDivision`

A new enum in `fx/delay.rs`, following the `AtomicEnum` + `from_u32`/`name`
convention every other enum in the crate uses — which means the UI's generic
`UiEnum` dropdown handles it with no new machinery. Nine variants: 1/1, 1/2,
1/4d, 1/4, 1/8d, 1/8, 1/8t, 1/16, 1/16t. Each maps to a length in beats; seconds
follow from the tempo.

### Tempo source

In sync mode the delay reads the tempo from
`sequencer.clock.tempo_bpm(steps_per_beat)`, not from `params.tempo`. The clock
is the running tempo. When it is slaved to external MIDI clock, reading it means
the delay follows the DAW with no extra work; reading `params.tempo` would leave
the delay locked to a number nobody is using.

### Smoothing

Delay time is smoothed through the existing `Smoothed` one-pole, which moves the
fractional read position gradually and produces a tape-style pitch glide when
the time changes. A hard jump would click. Mix, feedback and the reverb
coefficients are smoothed the same way, for the reason the module already
documents: a per-block step is a broadband click.

## User interface

Two sections, `delay_section` and `reverb_section` in
`bevy_synth_ui/src/lib.rs`, built from the existing `widgets::section`,
`KnobSpec`, `dropdown` and `integer` helpers, laid out the way `lfo_section` is.
They occupy a new fourth column in the panel's `horizontal_top` row.

One new palette constant, `FX`, in the teal/cyan range — no existing palette
entry occupies it, so the effects read as their own group at a glance.

The window's `default_size` widens from 880 to 1120 to fit the column.

`delay_time` and `delay_division` occupy the same slot in the layout: whichever
one the `delay_sync` toggle has active is the one shown. Two live controls for
one quantity, only one of which does anything, is a UI that invites the wrong
knob.

## Presets

Preset `apply` functions set the entire patch, so all twelve new parameters are
set in all eight presets. `init` zeroes both mixes; the others use them where
they earn it — Warm Pad and Glass Bell get real reverb, Pluck a synced 1/8
delay, Acid Bass and Sub Bass stay dry because a resonant bassline in a hall is
mud.

The existing preset test that asserts every preset writes every patch parameter
needs the new fields added to it, or it will pass while saying nothing about
them.

Presets continue to leave the sequencer alone. `delay_division` is a patch
property, not a sequencer one — it describes the sound, not the piece — so it is
set by presets like everything else.

## Testing

Unit tests in-module under `#[cfg(test)]`, matching the rest of `synth_core`.
Written before the code they cover.

**`line.rs`** — a read at an integer offset returns exactly the sample written
there; a fractional read at 1.5 returns the midpoint of its two neighbours;
reads that wrap the ring buffer are correct; a read older than the buffer is
clamped rather than aliasing.

**`delay.rs`**

- Impulse in, feedback 0, mix 1: exactly one non-zero output sample, at the
  sample offset the configured time implies. This is the test that catches an
  off-by-one in the ring buffer, which is the most likely bug in the file and
  the hardest to hear.
- Feedback 0.5: successive repeats at half amplitude, spaced by the delay time.
- Ping-pong on: an impulse into L puts the first repeat in R and the second back
  in L.
- `NoteDivision` to seconds: at 120 BPM, 1/8 is 0.25 s and 1/4d is 0.75 s.
- Mix 0: output bit-identical to input. This guards the promise that defaults
  change nothing.

**`reverb.rs`** — a reverb has no exact expected output, so these are property
tests.

- Impulse in: output still non-zero 200 ms later, and the RMS of successive
  50 ms windows decreases. A tail that exists and decays.
- Higher `size` produces a measurably longer tail than lower `size`.
- A mono input produces L and R that differ. The decorrelation is the entire
  reason this topology was chosen, so it is asserted rather than assumed.
- Ten seconds of full-scale noise at `size` 1.0 and `damping` 0.0 — the worst
  case for stability — leaves every output sample finite and within ±4.0. This
  is what catches a tank that self-oscillates, and it is the single most
  valuable test in the file.

**`engine.rs`**

- With every mix at its 0.0 default, `process` output is bit-identical to a
  golden sample vector. The vector is captured from the current code *before*
  the effects chain is written and committed alongside the test, so it records
  what the synth sounds like today rather than what it sounds like after the
  change. A reference "rendered without the effects chain" would not work: once
  the chain exists there is no such path left to compare against.
- With reverb up, `process_stereo_interleaved` produces L != R.

**Manual verification.** `cargo run`, load Warm Pad, confirm it sounds like a
pad in a room, and drag the delay time knob to confirm it glides rather than
clicks. Automated tests can show the tail decays; they cannot show it sounds
good.

## Out of scope

Chorus, phaser, distortion as a separate stage, per-voice panning, sidechain
ducking, and effect ordering as a user-facing control. Each is a reasonable
future addition and none is needed to make reverb and delay work.
