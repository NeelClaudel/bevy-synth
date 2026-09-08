# bevy_synth

A subtractive synthesizer for Bevy, in Rust, with no C dependencies beyond the
platform audio and MIDI stacks.

Polyphonic or monophonic. Two band-limited oscillators, a sub and noise, into a
state-variable filter with every response and real resonance. Two envelopes, an
LFO, glide, drive. A tempo-syncable stereo delay into a plate reverb, stereo
out. MIDI keyboard and MIDI clock. A step sequencer with a generative pattern
writer that stays in key.

## The one thing to understand

There are two clocks, and mixing them up is the mistake that makes hand-rolled
game audio sound broken.

| | rate | jitter | what runs there |
|---|---|---|---|
| Game thread | ~60 Hz | a whole frame, worse on a spike | knobs, note triggers, UI |
| Audio thread | 44.1/48 kHz | none permitted | every sample, the sequencer, the clock |

The control side never generates a sample. It writes atomics and pushes events
onto a lock-free queue. The audio side reads them at a block boundary and
smooths them. Nothing below `Engine::process` allocates, locks, or blocks.

That is why the sequencer lives on the audio thread rather than in a Bevy
system: a note placed from a system inherits the frame's jitter, and 16 ms of
timing error is audible as sloppy playing.

## Quick start

```rust
use bevy_app::prelude::*;
use bevy_synth::{Synth, SynthPlugin};

App::new()
    .add_plugins(SynthPlugin::default())
    .add_systems(Startup, |synth: Res<Synth>| {
        synth.params.cutoff.set(1200.0);
        synth.note_on(60, 0.9);
    })
    .run();
```

Hear it before wiring anything up:

```
cargo run                                          # the full panel, in a window
cargo run -p bevy_synth --example generative_jam   # jams with itself, headless
cargo run -p synth_audio --bin render-demo -- ./out # five patches to WAV, no sound card
```

`cargo run` builds the workspace root, which is the app in `src/main.rs`. Add
`--release` if the audio crackles. The library crates are tested with
`cargo test --workspace`.

## The panel

```rust
App::new()
    .add_plugins(DefaultPlugins)
    .add_plugins(EguiPlugin::default())
    .add_plugins(SynthPlugin::default())
    .add_plugins(SynthUiPlugin::open())
    .run();
```

Knobs rather than sliders, because forty horizontal sliders is a wall nobody
can navigate; a knob's pointer angle is readable at a glance, which is what you
want when you are listening rather than looking. Frequency and time knobs are
logarithmic — on a linear cutoff knob the entire usable bass range lives in the
first 5% of the travel. Drag to turn, shift-drag for fine control,
double-click to reset.

Three things are drawn rather than numbered, because the number tells you
nothing about what you will hear:

- **The envelopes**, as the shape they will produce, with segment widths
  proportional to their real times and a marker at note-off.
- **The filter response**, computed from the analogue SVF prototype — exact,
  a few dozen multiplies, and it updates the instant a knob moves rather than
  after an analysis window. Watch what resonance does to the corner.
- **The pattern**, as a grid showing each step's note name and velocity, with
  the playing step highlighted. Click a step to toggle it; toggling keeps the
  note, so a step switched off and back on returns with what the generator
  chose.

Plus an on-screen keyboard (drag across it for a glissando — it releases the old
note and starts the new one rather than retriggering), a peak meter with a
slow-falling hold, and eight presets. Presets set the *patch* only and leave
tempo, key and pattern alone: those belong to the piece, not the sound.

The delay and reverb parameters, with their ranges and defaults:

| Parameter | Range | Default | What it does |
| --- | --- | --- | --- |
| Delay mix | 0.0–1.0 | 0.0 | Dry/wet. 0.0 bypasses the delay entirely. |
| Delay sync | on/off | off | Take the time from the sequencer clock instead of the Time knob. |
| Delay time | 0.001–2.0 s | 0.375 s | Free-running repeat interval. |
| Delay division | 1/1–1/16T | 1/8 | Repeat interval in musical time, used when sync is on. |
| Delay feedback | 0.0–0.95 | 0.35 | How much of each repeat feeds the next. |
| Delay damping | 0.0–1.0 | 0.3 | High-frequency loss per repeat. Bright digital to dark tape. |
| Ping pong | on/off | off | Repeats alternate between the speakers. |
| Reverb mix | 0.0–1.0 | 0.0 | Dry/wet. 0.0 bypasses the reverb entirely. |
| Reverb size | 0.0–1.0 | 0.5 | Tail length. |
| Reverb damping | 0.0–1.0 | 0.5 | High-frequency absorption in the tank. |
| Pre-delay | 0.0–0.25 s | 0.02 s | Gap before the tail starts. This is what reads as room size. |
| Reverb width | 0.0–1.0 | 1.0 | 0.0 collapses the tail to mono, 1.0 is the full spread. |

### How the UI sees the pattern without a lock

The pattern lives inside the sequencer on the audio thread. Rather than lock it,
the engine publishes a packed copy into an array of atomics whenever it changes
— one `u32` per step, so no step is ever read half-written. The UI reads that
with no synchronisation at all, and the audio thread never waits for a repaint.

Everything else in the panel is the same idea: every control writes an atomic
and returns. A UI that stalls drops a frame. A UI that made the audio thread
wait would drop the audio, which is worse and far more audible.

## Layout

```
synth_core     pure DSP. No Bevy, no cpal, no MIDI, no I/O at all.
synth_audio    cpal output stream, midir input, offline WAV rendering.
bevy_synth     the Bevy plugin: one resource, one system.
bevy_synth_ui  the egui control panel. Optional — the synth does not need it.
```

`synth_core` has no dependencies whatsoever, which is what makes it testable:
126 tests run the DSP offline, in under a second, with no audio device.

## Where each feature lives

```
voices -> drive -> soft clip -> DC block -> delay -> reverb -> master gain -> NaN guard
```

The chain is mono up to the delay and stereo after it — the reverb's stereo
image comes from tapping a single shared tank at different points, rather
than from running two reverbs side by side.

| What you asked for | Where | Notes |
|---|---|---|
| Mono / poly | `engine.rs` | Mono is last-note priority with fallback and legato — what makes basslines playable. Poly steals releasing voices before held ones, quietest first. |
| Low / high / mid / notch | `filter.rs` | One state-variable filter computes all taps simultaneously. Picking a mode is picking a tap. |
| Resonance | `filter.rs` | `k = 2 - 2·res`, clamped just above zero. 1.0 approaches self-oscillation. |
| 12 / 24 dB slope | `filter.rs` | Two SVFs in series; the second gets half the resonance, or the peaks multiply into a howl. |
| Envelopes | `env.rs` | Exponential ADSR, one for amplitude and one for the filter. Times in seconds, so patches survive a sample-rate change. |
| LFO | `lfo.rs` | Six shapes including sample-and-hold and smoothed random. Routes to cutoff, pitch, amplitude or pulse width. |
| Delay and reverb | `fx/` | A tempo-syncable stereo delay into a Dattorro plate reverb, in that order so the reverb hears the repeats. Both hard-bypass at zero mix, so a dry patch comes out bit-identical. |
| MIDI keyboard | `synth_audio/midi.rs` | Notes, velocity, pitch bend, mod wheel, all-notes-off. Bypasses the ECS — hardware input should not wait for a frame. |
| MIDI clock | `clock.rs` | 24 PPQN, with Start/Stop/Continue. Estimates the incoming tempo from tick spacing and ignores implausible gaps. |
| Sequencer | `sequencer.rs` | Sample-accurate to within one 32-sample block (~0.7 ms), with per-step gate length and swing. |
| Random notes in key | `scale.rs`, `sequencer.rs` | See below. |

## Making random notes sound good

Uniform random MIDI numbers sound like a fault. Four constraints, stacked, fix
it — this is the part worth reading:

1. **Stay in the scale.** A scale is a table of semitone offsets; a random
   *degree* through that table is always consonant with everything else in the
   key. Fourteen scales, from major to hirajoshi. This removes the wrong notes.
2. **Move by step, not by leap.** Real melodies mostly walk. Candidate degrees
   are weighted by proximity, so the line walks and only occasionally jumps.
   Without this you get a scatter plot that happens to be in key.
3. **Chord tones on downbeats.** Root, third and fifth are weighted up at the
   start of each bar and flatten out in between, so the notes off the beat
   sound like passing tones rather than mistakes. This is most of what makes it
   sound composed rather than merely correct.
4. **Rests.** Gaps make phrases, and phrases are what a listener remembers.

The generator writes a whole pattern and loops it, rather than picking each note
as it plays. Repetition is what lets the ear recognise a figure; without it even
well-chosen notes sound aimless. Seeded, so the same seed always gives the same
melody — `synth.regenerate_with_seed(level_id)` gives each level a theme that is
reproducible but that you never had to write.

## Choices worth knowing about

**PolyBLEP oscillators.** A naive saw or square aliases: harmonics above Nyquist
fold back at frequencies unrelated to the note, giving the metallic shimmer that
marks out an amateur synth. PolyBLEP costs a handful of multiplies and removes
the worst of it. A wavetable with per-octave mipmaps is cleaner still, if you
ever want it.

**TPT state-variable filter, not a biquad.** A direct-form biquad bakes in its
coefficients; sweep its cutoff at high resonance and you get zipper noise and
bursts of instability, because the leftover state is wrong for the new
coefficients. The topology-preserving form keeps its state in physical units, so
you can modulate it at audio rate and it stays musical. There is a test that
sweeps it across the whole range at 0.98 resonance and checks it does not blow
up.

**Voice stealing fades.** Cutting a sounding voice to start a new one is a step
discontinuity — a click — and in a busy passage voices get stolen constantly, so
the clicks become a rattle. Two milliseconds of fade removes it at no
perceptible cost in latency.

**Parameter smoothing.** Every continuous parameter is one-poled toward its
target at control rate. Without it, dragging a knob steps the value once per
block and each step is a broadband click.

**`f64` for the clock and LFO phase.** An `f32` phase advanced 48000 times a
second accumulates visible error within seconds — enough to drop a step at
exactly the wrong moment.

**Failing to open audio is not fatal.** No sound card means a logged warning and
a silent app, not a crash. Games run on machines with no audio, and in CI.

## Extending it

The obvious next moves, roughly in order of value per line of code:

- **Chorus and phaser.** Both fall out of the delay line primitive that is
  already there.
- **A distortion stage.** Separate from the drive control, for more character
  than drive alone gives.
- **Per-voice panning.** So the stereo field starts before the effects rather
  than at them.
- **User-reorderable effects.** Worth having once there are more than two.
- **Patch save/load.** `Params` is plain data; derive `Serialize` on it and you
  have patch files.
- **Unison.** Several detuned voices per note. The voice allocator already has
  the structure; it needs a voices-per-note multiplier.
- **Wavetables.** If PolyBLEP's residual aliasing bothers you up high.

## Licence

MIT OR Apache-2.0.
