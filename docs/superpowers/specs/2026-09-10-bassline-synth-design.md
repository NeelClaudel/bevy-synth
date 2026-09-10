# A 303-style bassline instrument

Date: 2026-09-10
Status: approved design, not yet implemented

## The request

> "next we will add a new instrument that is a bassline like synth, we will
> use the same sequencer and fx we already have for this one"

Refined through brainstorming into three decisions:

1. **Sequencer** — its own independent pattern. A second `Sequencer`
   instance: same generative engine and code, its own density, range,
   octave and seed. Bass and lead are two independent lines locked to the
   one `Clock`.
2. **Sound** — a 303-style acid bassline. Monophonic, one voice, per-step
   slide and accent, one oscillator into a resonant lowpass swept by a fast
   decay envelope.
3. **Routing** — a full third bus. `bass_gain`, `bass_send` and its own
   `comp_bass` insert, symmetric with what the synth and drums already
   have, with the compressor able to sidechain off the existing kick tap.

## What exists today

Three buses meet at the master compressor:

```
voices -> drive -> DC block -> synth_gain -> comp_synth --+-- send --+
                                                          |          |
drums -> drum_gain -------------------------------------- + -- send -+
                                                          |          |
                                                          |   [FX return]
                                                          |   delay+reverb
                                                          |          |
                                                          +<---------+
                                                          v
                                                     comp_master -> out
```

The pieces the bass needs mostly exist already:

- `Oscillator` with `Waveform::{Sine, Triangle, Saw, Pulse, Noise}`.
- `Filter` — a mode plus a `Slope`, where `Slope::Db24` is two SVFs in
  series, "closer to a ladder filter". Exactly the 303 target.
- `Adsr` with `AdsrSettings { attack, decay, sustain, release }`. A
  decay-only envelope is `sustain: 0.0`.
- `Sequencer` with `new(seed)`, `regenerate(&GenerativeSettings)`,
  `advance`, `on_midi_tick`, `release_all`, and a `Pattern` of `Step`.
- `Compressor` with `SidechainSource::{Off, DrumBus, Kick}` and a live kick
  tap from the drum rack.
- `SharedCompressor` — the established pattern for mirroring a grouped
  parameter struct into atomics.

What does *not* exist: any notion of slide, any audible meaning for accent,
and any way for a voice to have parameters of its own.

## Why a dedicated voice rather than reusing `Voice`

`Voice::process_block(&mut self, out, p: &Params, lfo, mod_depth)` reads
**23 fields straight off the flat `Params` struct** — `p.cutoff`,
`p.osc1_wave`, `p.amp_env`, `p.filter_slope`, and so on. So "give the bass
its own sound" is really a question about that coupling: the bass cannot
have its own cutoff while `Voice` reaches into the one global `Params`.

Two alternatives were considered and rejected:

- **Extract a `VoiceParams` struct** that `Params` holds twice, once per
  instrument. Gives the bass the whole engine — two oscillators, sub,
  noise, every filter mode, LFO targets. Rejected because it rewrites every
  lead knob in `params.rs` and across the whole UI as `p.synth.cutoff`, and
  accent and slide still have to be bolted on afterwards.
- **Mirror the 23 fields as `bass_*`** and assemble a synthetic `Params`
  per block. Smallest diff, worst to live with: 23 fields duplicated by
  hand and kept in sync forever.

The chosen approach is a dedicated `BassVoice`. The deciding argument is
not risk, it is fit. A 303 is not a general subtractive voice with bass
settings:

- Its filter envelope is decay-only. There is no sustain to configure.
- Accent is one control with three destinations at once — level, cutoff
  and envelope depth.
- Slide is a fixed-time portamento gated by a per-step tie, not a global
  glide time.

Bolting those onto `Voice::note_on(note, velocity, glide_from, reset, age)`
fights the abstraction. A purpose-built voice composed from the existing
DSP blocks is smaller, and `voice.rs`, the lead's parameters and the lead's
UI are untouched by construction.

## Section 1 — The bass voice

New file `crates/synth_core/src/bass.rs`. It composes existing DSP; the
only new DSP-adjacent logic is accent, slide and the cutoff formula.

```rust
pub struct BassVoice {
    osc: Oscillator,
    filter: Filter,        // mode: Lowpass, slope: Db24, fixed
    amp_env: Adsr,
    filter_env: Adsr,
    note: u8,
    gate: bool,
    accented: bool,
    pitch: f32,            // current, in semitones
    target_pitch: f32,
    glide_coef: f32,       // 0.0 = jump straight there
}
```

**Envelopes.** The filter envelope is decay-only:

```rust
AdsrSettings { attack: 0.003, decay: <Decay knob>, sustain: 0.0, release: <Decay knob> }
```

That single choice is most of the acid character. Every note is a sweep
that falls to nothing regardless of how long the gate is held, which is why
a 303 line breathes even when every step is the same length. The amp
envelope is a fast gate — `attack: 0.003, decay: 0.0, sustain: 1.0,
release: 0.008` — short enough never to lag the beat, long enough never to
click.

**Cutoff.** Summed in octaves and applied exponentially, matching the
convention `voice.rs` already documents (pitch and brightness are both
logarithmic, so "+1 octave" means the same thing at 200 Hz as at 4 kHz):

```
octaves = env_mod * filter_env.level() + accent_octaves
cutoff_hz = (base_cutoff * octaves.exp2()) bounded to 20.0 ..= 20000.0
```

**Accent** is one knob feeding three destinations, which is what makes a
real accent read as emphasis rather than as a volume change:

| Destination | Value on an accented step |
|---|---|
| Level | `1.0 + accent * 0.5` multiplying the note amplitude |
| Cutoff | `accent_octaves = accent * 1.5`, added to the exponent above |
| Envelope depth | `env_mod * (1.0 + accent)` for that note only |

On an unaccented step all three collapse to their neutral values —
amplitude ×1.0, no cutoff offset, `env_mod` as set — so the `accent` knob
sets how much louder and brighter an accented step is than its neighbours,
not an absolute level.

Accent comes from the pattern step, not from note-on velocity. It is
latched at note-on into `accented` and held for the life of the note, so a
pattern edit mid-note cannot change a note already sounding.

**Slide** is the tie. When the incoming step is marked `slide`, the voice
does **not** retrigger either envelope. It moves `target_pitch` and lets a
one-pole glide walk there over `slide_time`. Not retriggering is the whole
point: retriggered notes cannot produce the legato squelch. `Voice` already
proves the one-pole-in-semitone-space approach, and `BassVoice` mirrors it
rather than inventing a second convention.

A slide on step 0, or after a rest, has nothing to slide *from*. In both
cases the voice falls back to a normal retrigger.

**`BassParams`** is a plain struct in `params.rs` beside `CompressorParams`,
with a `SharedBass` atomic mirror following `SharedCompressor` exactly —
`new`, `snapshot` (with `sane()` and range clamps on every field) and
`apply`:

| Field | Range | Default |
|---|---|---|
| `wave` | `Saw` \| `Pulse` | `Saw` |
| `tune` | -12.0 ..= 12.0 semitones | 0.0 |
| `cutoff` | 20.0 ..= 20000.0 Hz | 300.0 |
| `resonance` | 0.0 ..= 1.0 | 0.7 |
| `env_mod` | 0.0 ..= 6.0 octaves | 3.0 |
| `decay` | 0.02 ..= 2.0 s | 0.3 |
| `accent` | 0.0 ..= 1.0 | 0.5 |
| `slide_time` | 0.01 ..= 0.5 s | 0.06 |

Eight fields, not nine: there is no `level` here. The voice renders at
unity and the bus stage owns its level, as `bass_gain` on the mixer strip.
Duplicating a level control in both places is how the two drift apart.

Monophonic, a single voice, no allocation, nothing on the `process` path
that is not real-time safe.

## Section 2 — The sequencer

A second `Sequencer` instance on `Engine`, seeded independently of the
lead's. Both read the same `advance` from the one `Clock`, so the two lines
are always locked together and share swing, tempo and transport.

### `Step` gains a `slide` flag

```rust
pub struct Step {
    pub active: bool,
    pub note: u8,
    pub velocity: f32,
    pub accent: bool,
    pub slide: bool,   // new
}
```

Two properties make this cheap:

- **`Step` stays 8 bytes.** The layout is one `f32` plus a `u8` and now
  three `bool`s, which still fits the 4-byte tail. `Pattern`'s doc comment
  ("At eight bytes a step this is 512 bytes on the stack") remains true,
  and `Event`, whose slot size is set by its largest variant, does not grow.
- **The mirror has room.** `pack_step` uses bit 31 for `active`, bit 30 for
  `accent`, bits 8–15 for `note` and bits 0–7 for `velocity`. Bits 16–29
  are free; `slide` takes bit 29.

The lead ignores `slide` entirely. The existing `accent` keeps its current
meaning for the lead — a downbeat marker for visuals — and additionally
becomes audible on the bass channel.

### Generation

`GenerativeSettings` gains two fields:

```rust
/// Chance a step is marked for slide.
pub slide_chance: f32,
/// Chance a step is accented on top of the downbeats it already gets.
pub accent_chance: f32,
```

**Both draws must be conditional.** This is a hard constraint, not a
stylistic one:

```rust
let mut accent = strong;
if gen.accent_chance > 0.0 && !accent {
    accent = self.rng.chance(gen.accent_chance);
}
let slide = gen.slide_chance > 0.0 && self.rng.chance(gen.slide_chance);
```

The lead passes `0.0` for both, the short-circuit skips the draw, and the
lead's RNG sequence is therefore byte-identical to today's. If either draw
ran unconditionally, every lead pattern would change and `GOLDEN_HASH`
would break. The golden-vector test renders through the sequencer, so this
is load-bearing.

`GenerativeSettings::from_params` stays as it is, for the lead. A new
`GenerativeSettings::for_bass(p: &Params)` reads the bass's own knobs.

### Shared key, independent line

The bass shares `gen_root` and `gen_scale` with the lead and takes
everything else from its own parameters:

| From the lead | The bass's own |
|---|---|
| `gen_root` | `bass_gen_octave` |
| `gen_scale` | `bass_gen_range` |
| | `bass_gen_density` |
| | `bass_gen_max_jump` |
| | `bass_gen_chord_bias` |
| | `bass_seq_length` |
| | `bass_slide_chance` |
| | `bass_accent_chance` |

Sharing root and scale is deliberate: a bass in a different key from the
lead is a bug, not a feature, and splitting them later is a one-line change
if it is ever wanted. Everything that distinguishes a bass line from a lead
line — register, range, how busy it is, how far it leaps — is independent.

### Mirror and events

The bass pattern needs its own mirror alongside the lead's, and its own
events:

```rust
Event::SetBassStep { index: u8, step: Step },
Event::RegenerateBass,
```

Both are the same size as `SetStep`, so the queue slot does not grow. New
variants rather than a `channel: u8` field on the existing ones: additive,
and no existing call site changes.

`SharedParams` gains `bass_pattern`, `bass_pattern_len` and `bass_step`
telemetry, with `publish_bass_step` / `publish_bass_len` /
`read_bass_pattern` mirroring the existing methods. `Engine` gains
`publish_bass_pattern`, called on the same terms as `publish_pattern` —
only when the pattern changes, never every block.

`ClockStop`, `AllNotesOff` and `Panic` must reach the bass sequencer and
voice as well as the lead's.

## Section 3 — The bus and the FX

The bass gets a full third bus, symmetric with the synth's:

```
bass -> bass_gain -> comp_bass --+-- bass_send --+
                                 |               |
synth -> drive -> comp_synth ----+-- synth_send -+
                                 |               |
drums -> drum_gain --------------+-- drum_send --+
                                 |               |
                                 |         [FX return]
                                 |         delay + reverb
                                 |               |
                                 +<--------------+
                                 v
                            comp_master -> out
```

Placement follows the rules the existing buses already established:

- **Not through `drive`.** The soft clipper belongs to the synth; the
  comment in `engine.rs` about the kick — "a kick through the soft clipper
  at drive 3.0 is a different instrument, and not a better one" — applies
  equally here. The 303's dirt comes from resonance and envelope depth.
- **Compressor before the send tap**, matching `comp_synth`, so the return
  hears the compressed signal and a pumped bass pumps in the reverb too.
- **Mono, duplicated to the pair.** `BassVoice` renders one mono block,
  which is copied into `bass_l` / `bass_r` before the compressor. A bass is
  centred; there is no pan control and no reason for one.

`comp_bass` is a `Compressor` with `CompressorParams`, unchanged and
reused. Setting its `sidechain` to `SidechainSource::Kick` uses the tap the
drum rack already publishes, which makes bass-ducking-under-kick fall out
with no new machinery. Its gain reduction is reported as `comp_bass_gr`,
following `comp_synth_gr` and `comp_master_gr` — a telemetry atomic living
in `SharedParams` and `from_params` only, never in the plain `Params`
snapshot.

New parameters on this section: `bass_enabled`, `bass_gain`, `bass_send`,
`comp_bass`.

### Defaults keep the golden vector intact

`bass_enabled` defaults to **false**. The existing golden render therefore
produces bit-identical output, `GOLDEN_HASH` is unchanged, and the hash is
never re-baselined. Combined with the conditional RNG draws in Section 2,
the bass is invisible to every existing test until something switches it
on.

## Section 4 — Parameters, events and UI

**`params.rs`.** `BassParams` and `SharedBass` beside the compressor pair;
the bus fields above; the generative fields; the pattern mirror; the
`comp_bass_gr` telemetry. `snapshot()` clamps every field through `sane()`
exactly as the existing code does, so a NaN arriving from the control side
can never reach the audio thread.

**UI.** Two additions, both fitting structures that already exist:

- A fourth **BASS** tab beside Synth, Sequencer and Drums, holding the eight
  voice knobs, the bass step grid with per-step slide and accent toggles,
  and the generative controls.
- A fourth **bass** strip in `sections/mixer.rs`, built with the existing
  `strip()` helper: Level, Send, the compressor controls and the GR
  readout. `mixer.rs` is 130 lines and its doc comment says "three strips
  and a meter"; that comment gets updated with the strip.

## Testing

Every test below must fail if the behaviour it names is removed. The bar is
the one the previous wave used: apply a mutation, watch that one test fail
alone, revert, watch it pass.

| Area | Test |
|---|---|
| Slide | A tied step does not retrigger the amp envelope — no new attack transient at the step boundary |
| Slide | Pitch moves continuously across a tie and discontinuously without one |
| Slide | Slide on step 0, and slide after a rest, both retrigger normally |
| Accent | An accented step is louder *and* brighter than the same step unaccented |
| Accent | Accent is latched at note-on: editing the step mid-note does not change the sounding note |
| Envelope | The filter envelope reaches zero while the gate is still held |
| Routing | `bass_send` at 0.0 puts no bass in the reverb tail; at 1.0 it does |
| Routing | `comp_bass` sits before the send tap — the return hears the compressed signal |
| Sidechain | `comp_bass` with `SidechainSource::Kick` ducks on the kick and not on other pads |
| Telemetry | `comp_bass_gr` reports the reduction actually applied |
| Sequencer | Bass and lead advance from the same clock and stay locked |
| Sequencer | The bass pattern is independent of the lead's — regenerating one leaves the other alone |
| Regression | With `bass_enabled: false`, `GOLDEN_HASH` is unchanged |
| Regression | With `slide_chance` and `accent_chance` at 0.0, lead patterns are identical to today's |

## Constraints

- **Never re-baseline `GOLDEN_HASH`.** The failing test prints a
  replacement constant; pasting it defeats the test. If the hash moves,
  something in this design leaked into the lead's path and that is the bug
  to fix.
- **Real-time safety on `process`.** No allocation, no lock, no `panic!`,
  `unwrap`, `expect`, out-of-bounds indexing or syscall. No `Vec`, `Box` or
  `String`. Allocation inside `#[cfg(test)]` is fine.
- **NaN.** `f32::clamp` propagates NaN; `f32::max`/`f32::min` discard a NaN
  `self`. Chain `max`/`min`, never `clamp`, anywhere a NaN could arrive.
- **Clippy baseline is 39** with `--all-targets`. The delta must be zero.
- **Never run `cargo fmt --all`** — the repo has never been rustfmt-clean.
- Repo files are CRLF; multiline `perl -0777` edits need `\r?\n`.

## Out of scope

Deliberately excluded, and easy to add later if wanted:

- A distortion or overdrive stage on the bass bus. The 303's famous dirt is
  usually an external pedal, and `drive` already exists to be generalised
  if that day comes.
- Per-step gate length or ratcheting.
- Bass pan. A centred bass is the correct default and a pan control on it
  is a mixing mistake waiting to happen.
- Splitting `gen_root` / `gen_scale` per instrument.
- Polyphony. The instrument is monophonic by definition.
