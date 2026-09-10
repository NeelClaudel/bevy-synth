# Bassline Synth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a monophonic 303-style bassline instrument — its own voice, its own generative sequencer line, and its own mixer bus — to bevy_synth, without changing a single sample of the existing lead or drum output.

**Architecture:** A new `BassVoice` in `crates/synth_core/src/bass.rs` (one oscillator, one 24 dB lowpass, a decay-only filter envelope, accent and slide) is driven by a *second* `Sequencer` instance sharing the one `Clock` with the lead. It renders mono, is duplicated to a stereo pair, and runs through its own `comp_bass` insert before tapping `bass_send` into the existing FX return. `Voice`, `Params`'s melodic fields and the lead UI are untouched. `bass_enabled` defaults to `false`, so every existing render is bit-identical until something switches the bass on.

**Tech Stack:** Rust workspace — `bevy_synth_app` (root binary) plus `crates/synth_core`, `crates/synth_audio`, `crates/bevy_synth`, `crates/bevy_synth_ui`. Bevy 0.19, bevy_egui 0.42, egui 0.36.1. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-10-bassline-synth-design.md`

## Global Constraints

- **Never re-baseline `GOLDEN_HASH`** (`crates/synth_core/src/engine.rs:1549`, currently `0x1c357a5698234b39`). The failing test prints a replacement constant; pasting it defeats the test. If the hash moves, something in this work leaked into the lead's path and *that* is the bug to fix.
- **Real-time safety on everything reachable from `Engine::process`.** No allocation, no lock, no `panic!`, `unwrap`, `expect`, out-of-bounds indexing or syscall. No `Vec`, `Box` or `String`. Allocation inside `#[cfg(test)]` is fine.
- **NaN.** `f32::clamp` propagates NaN; `f32::max`/`f32::min` discard a NaN `self`. Chain `max`/`min`, never `clamp`, anywhere a NaN can arrive from the control side. `params.rs` already provides `fn sane(value, fallback)` and `fn clamp01(value)` for exactly this — use them.
- **Clippy baseline is 39** warnings with `cargo clippy --workspace --all-targets`. The delta must be zero. Count with `grep -E "^warning: .* generated"`; never pipe clippy through `tail`, and never use plain `--workspace` (it shows 4 of them).
- **Never run `cargo fmt --all`.** The repo has never been rustfmt-clean; running it would produce a diff of thousands of unrelated lines.
- **Repo source files are CRLF.** A multiline `perl -0777` edit must use `\r?\n` in the pattern and `\r\n` in the replacement. Prefer the `Edit` tool, which handles this for you.
- **In tests, never write `let mut p = Params::default(); p.x = ...;`** — that trips the `field_reassign_with_default` clippy lint and moves the baseline. Use struct-update syntax: `Params { x: ..., ..Params::default() }`.
- Telemetry atomics live in `SharedParams` and its `from_params` only. They never appear in the plain `Params` snapshot.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/synth_core/src/bass.rs` | The `BassVoice`: oscillator, filter, two envelopes, accent, slide. One monophonic instrument, no bus logic. | **Create** |
| `crates/synth_core/src/sequencer.rs` | Gains `Step::slide`, `SeqSettings`, accent/slide on `SeqOutput` and `Pending`, and the two conditional generative draws. | Modify |
| `crates/synth_core/src/params.rs` | `BassParams` + `SharedBass` beside the compressor pair; the bus, generative and mirror fields; `comp_bass_gr`. | Modify |
| `crates/synth_core/src/event.rs` | `Event::SetBassStep` and `Event::RegenerateBass`. | Modify |
| `crates/synth_core/src/engine.rs` | Owns the second `Sequencer`, the `BassVoice` and `comp_bass`; renders and routes the bass bus. | Modify |
| `crates/synth_core/src/lib.rs` | Re-exports `bass::BassVoice` and `params::BassParams`. | Modify |
| `crates/bevy_synth/src/lib.rs` | `Synth` facade methods for the bass pattern; `bass_step` and `comp_bass_gr` on `SynthTelemetry`. | Modify |
| `crates/bevy_synth_ui/src/sections/bass.rs` | The BASS tab: eight voice knobs, the bass step grid, the generative controls. | **Create** |
| `crates/bevy_synth_ui/src/sections/mod.rs` | Registers and re-exports the new section. | Modify |
| `crates/bevy_synth_ui/src/sections/mixer.rs` | A fourth `strip()` for the bass bus. | Modify |
| `crates/bevy_synth_ui/src/widgets.rs` | `bass_step_grid`: the step grid with slide and accent affordances. | Modify |
| `crates/bevy_synth_ui/src/lib.rs` | `Tab::Bass` and its dispatch arm. | Modify |

`bass.rs` is a peer of `voice.rs`, not a variant of it. The two instruments share `Oscillator`, `Filter` and `Adsr` and nothing else; keeping them apart is what lets `Voice` stay polyphonic and parameter-heavy while `BassVoice` stays eight knobs and one note.

---

## Task 1: Sequencer plumbing — `Step::slide`, `SeqSettings`, accent and slide on the output

The engine is about to run two sequencers from one clock. Today `Sequencer` reaches into the global `Params` for three fields (`seq_length`, `seq_swing`, `seq_gate`), which would silently give the bass the lead's length; and `SeqOutput` drops the step's `accent` and `slide` on the floor, which the bass voice needs per note. This task fixes both, plus adds the `slide` flag itself. It is pure plumbing: every value the lead sees is identical, so `GOLDEN_HASH` must not move.

**Files:**
- Modify: `crates/synth_core/src/sequencer.rs`
- Modify: `crates/synth_core/src/params.rs` (`pack_step` / `unpack_step`)
- Modify: `crates/synth_core/src/engine.rs:294` and `crates/synth_core/src/engine.rs:487`
- Test: `crates/synth_core/src/sequencer.rs` (inline `mod tests`), `crates/synth_core/src/params.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `Params` (`seq_length: usize`, `seq_swing: f32`, `seq_gate: f32`), `ClockView { running: bool, samples_per_step: f32 }`, `Advance { steps: usize, .. }`.
- Produces:
  - `pub struct Step { pub active: bool, pub note: u8, pub velocity: f32, pub accent: bool, pub slide: bool }`
  - `pub struct SeqSettings { pub length: usize, pub swing: f32, pub gate: f32 }` with `pub fn from_params(p: &Params) -> Self`
  - `pub struct SeqOutput { pub note_on: Option<(u8, f32)>, pub note_off: Option<u8>, pub stepped: bool, pub accent: bool, pub slide: bool }`
  - `pub fn Sequencer::advance(&mut self, samples: usize, adv: Advance, clock: ClockView, s: &SeqSettings) -> SeqOutput`
  - `pub fn Sequencer::on_midi_tick(&mut self, ticked: bool, clock: ClockView, s: &SeqSettings) -> SeqOutput`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/synth_core/src/sequencer.rs`:

```rust
    #[test]
    fn output_carries_accent_and_slide_from_the_step() {
        let mut s = Sequencer::new(0x51DE_0001);
        s.set_step(
            1,
            Step { active: true, note: 40, velocity: 0.9, accent: true, slide: true },
        );
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params { seq_length: 4, seq_swing: 0.0, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        // Run until the sequencer lands on step 1 and fires it.
        let mut fired = None;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if out.note_on == Some((40, 0.9)) {
                fired = Some(out);
                break;
            }
        }
        let out = fired.expect("step 1 should have triggered");
        assert!(out.accent, "an accented step must report accent");
        assert!(out.slide, "a tied step must report slide");
    }

    #[test]
    fn swung_notes_keep_their_accent_and_slide() {
        // Swing routes odd steps through `Pending`, which is exactly where the
        // step's flags used to be dropped.
        let mut s = Sequencer::new(0x51DE_0002);
        s.set_step(
            1,
            Step { active: true, note: 41, velocity: 0.9, accent: true, slide: true },
        );
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params { seq_length: 4, seq_swing: 0.4, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        let mut fired = None;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if out.note_on == Some((41, 0.9)) {
                fired = Some(out);
                break;
            }
        }
        let out = fired.expect("swung step 1 should have triggered");
        assert!(out.accent, "swing must not lose the accent flag");
        assert!(out.slide, "swing must not lose the slide flag");
    }

    #[test]
    fn settings_come_from_the_argument_not_the_global_params() {
        // Two sequencers, one clock, different lengths: the whole point of
        // lifting these three fields out of `Params`.
        let mut lead = Sequencer::new(0x51DE_0003);
        let mut bass = Sequencer::new(0x51DE_0003);
        for i in 0..16 {
            let step = Step { active: true, note: 40 + i as u8, velocity: 0.8, accent: false, slide: false };
            lead.set_step(i, step);
            bass.set_step(i, step);
        }
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params::default();
        let long = SeqSettings { length: 16, ..SeqSettings::from_params(&p) };
        let short = SeqSettings { length: 2, ..SeqSettings::from_params(&p) };

        let mut lead_max = 0usize;
        let mut bass_max = 0usize;
        for _ in 0..20_000 {
            let adv = c.advance(BLOCK, p.clock_source);
            let view = c.view();
            lead.advance(BLOCK, adv, view, &long);
            bass.advance(BLOCK, adv, view, &short);
            lead_max = lead_max.max(lead.position());
            bass_max = bass_max.max(bass.position());
        }
        assert!(lead_max > 8, "the long sequencer should reach the back half");
        assert_eq!(bass_max, 1, "the short sequencer must loop within two steps");
    }
```

Note: both sequencers are driven from a *single* `advance`/`view` pair. Calling `c.advance` twice per block would run the clock at double speed and the test would prove nothing.

And add to the `mod tests` block in `crates/synth_core/src/params.rs`:

```rust
    #[test]
    fn packed_step_round_trips_slide() {
        let step = crate::sequencer::Step {
            active: true,
            note: 37,
            velocity: 0.75,
            accent: true,
            slide: true,
        };
        let back = unpack_step(pack_step(&step));
        assert!(back.slide, "slide must survive the mirror");
        assert!(back.accent);
        assert_eq!(back.note, 37);

        let plain = crate::sequencer::Step { slide: false, ..step };
        assert!(!unpack_step(pack_step(&plain)).slide);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p synth_core --lib
```

Expected: compile errors — `Step` has no field `slide`, `SeqSettings` is not defined, `SeqOutput` has no `accent`.

- [ ] **Step 3: Add `slide` to `Step`**

In `crates/synth_core/src/sequencer.rs`, extend the struct and its `Default`:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// False means a rest.
    pub active: bool,
    pub note: u8,
    pub velocity: f32,
    /// Downbeat marker for the lead's visuals; audible on the bass channel,
    /// where it drives cutoff and level.
    pub accent: bool,
    /// Ties this step to the one before it: the bass glides into it without
    /// retriggering. The lead ignores it.
    pub slide: bool,
}
```

Add `slide: false` to the existing `impl Default for Step`, and `slide: false` to the `Step { .. }` literal at the tail of `regenerate`. Leave `Pattern`'s doc comment alone — the struct is still one `f32` plus a `u8` and three `bool`s at align 4, so it is still eight bytes.

- [ ] **Step 4: Pack `slide` into the mirror**

In `crates/synth_core/src/params.rs`, bits 16–29 are free. Take bit 29:

```rust
fn pack_step(step: &crate::sequencer::Step) -> u32 {
    ((step.active as u32) << 31)
        | ((step.accent as u32) << 30)
        | ((step.slide as u32) << 29)
        | ((step.note as u32) << 8)
        | ((step.velocity.clamp(0.0, 1.0) * 255.0) as u32)
}

fn unpack_step(packed: u32) -> crate::sequencer::Step {
    crate::sequencer::Step {
        active: packed & (1 << 31) != 0,
        accent: packed & (1 << 30) != 0,
        slide: packed & (1 << 29) != 0,
        note: ((packed >> 8) & 0xFF) as u8,
        velocity: (packed & 0xFF) as f32 / 255.0,
    }
}
```

- [ ] **Step 5: Add `SeqSettings` and widen `SeqOutput` and `Pending`**

In `crates/synth_core/src/sequencer.rs`, above `SeqOutput`:

```rust
/// The three per-line settings the sequencer used to read straight out of the
/// global parameter block.
///
/// The engine runs two sequencers — the lead and the bass — off one clock.
/// Reading `Params` inside `advance` would hand both the same length, swing
/// and gate, which is precisely the coupling a second line exists to avoid.
/// Passing a small block instead lets each caller choose.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeqSettings {
    /// Loop length in steps. Clamped to `1..=MAX_STEPS` by the reader.
    pub length: usize,
    /// Delay on every second step, as a fraction of a step.
    pub swing: f32,
    /// Note length, as a fraction of a step.
    pub gate: f32,
}

impl SeqSettings {
    /// The lead's settings.
    pub fn from_params(p: &Params) -> Self {
        Self {
            length: p.seq_length,
            swing: p.seq_swing,
            gate: p.seq_gate,
        }
    }
}
```

Then:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SeqOutput {
    pub note_on: Option<(u8, f32)>,
    pub note_off: Option<u8>,
    pub stepped: bool,
    /// Whether the note in `note_on` came from an accented step.
    pub accent: bool,
    /// Whether the note in `note_on` is tied to the note before it.
    pub slide: bool,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    note: u8,
    velocity: f32,
    accent: bool,
    slide: bool,
    samples_remaining: f32,
}
```

`Default` still derives cleanly: both new flags default to `false`, which is exactly what a block with no note-on should report.

- [ ] **Step 6: Thread `SeqSettings` through the four methods**

In `crates/synth_core/src/sequencer.rs`, change the signatures and the three field reads. `advance`:

```rust
    pub fn advance(
        &mut self,
        samples: usize,
        adv: Advance,
        clock: ClockView,
        s: &SeqSettings,
    ) -> SeqOutput {
        let mut out = SeqOutput::default();

        let requested_length = s.length.clamp(1, MAX_STEPS);
```

…leaving the rest of that block untouched, then:

```rust
        if let Some(pending) = &mut self.pending {
            pending.samples_remaining -= samples_f;
            if pending.samples_remaining <= 0.0 {
                let pending = self.pending.take().expect("just checked");
                self.trigger(pending.note, pending.velocity, pending.accent, pending.slide, s, clock, &mut out);
            }
        }

        for _ in 0..adv.steps {
            self.step(s, clock, &mut out);
        }

        out
    }

    pub fn on_midi_tick(&mut self, ticked: bool, clock: ClockView, s: &SeqSettings) -> SeqOutput {
        let mut out = SeqOutput::default();
        if ticked {
            self.step(s, clock, &mut out);
        }
        out
    }
```

`step` and `trigger`:

```rust
    fn step(&mut self, s: &SeqSettings, clock: ClockView, out: &mut SeqOutput) {
        // ... the existing wrap, queued-pattern swap, `self.position = next`
        // and `out.stepped = true` are unchanged ...

        let step = self.pattern[self.position];
        if !step.active {
            return;
        }

        let swing_delay = if self.position % 2 == 1 && s.swing > 0.0 {
            s.swing * clock.samples_per_step
        } else {
            0.0
        };

        if swing_delay > 0.0 {
            self.pending = Some(Pending {
                note: step.note,
                velocity: step.velocity,
                accent: step.accent,
                slide: step.slide,
                samples_remaining: swing_delay,
            });
        } else {
            self.trigger(step.note, step.velocity, step.accent, step.slide, s, clock, out);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn trigger(
        &mut self,
        note: u8,
        velocity: f32,
        accent: bool,
        slide: bool,
        s: &SeqSettings,
        clock: ClockView,
        out: &mut SeqOutput,
    ) {
        if let Some(previous) = self.sounding.take() {
            out.note_off = Some(previous);
        }

        out.note_on = Some((note, velocity));
        out.accent = accent;
        out.slide = slide;
        self.sounding = Some(note);
        self.samples_until_off = s.gate * clock.samples_per_step;
    }
```

`trigger` now takes seven arguments, one over clippy's default threshold of seven-inclusive; the `#[allow]` is there so the baseline of 39 does not move. If clippy does not in fact complain, delete the attribute — an unnecessary `allow` is itself a lint.

- [ ] **Step 7: Update the two engine call sites and the test helper**

`crates/synth_core/src/engine.rs`, in `render_chunk` (around line 294):

```rust
        let seq = self
            .sequencer
            .advance(count, adv, view, &SeqSettings::from_params(params));
```

and in the `Event::ClockTick` arm (around line 487):

```rust
                    let seq = self.sequencer.on_midi_tick(
                        ticked,
                        self.clock.view(),
                        &SeqSettings::from_params(params),
                    );
```

Add `SeqSettings` to the `use crate::sequencer::{...}` import at the top of `engine.rs`.

In `crates/synth_core/src/sequencer.rs`'s `mod tests`, the helper takes settings instead of params:

```rust
    /// Drives a sequencer for one block from a clock the test owns.
    fn block(s: &mut Sequencer, c: &mut Clock, settings: &SeqSettings) -> SeqOutput {
        let p = Params::default();
        if p.clock_source == ClockSource::Internal {
            c.set_tempo(p.tempo, p.steps_per_beat);
        }
        let adv = c.advance(BLOCK, p.clock_source);
        s.advance(BLOCK, adv, c.view(), settings)
    }
```

Then update every existing call: a test that had `let p = Params { .. }` and called `block(&mut s, &mut c, &p)` now builds `let settings = SeqSettings::from_params(&p);` once and calls `block(&mut s, &mut c, &settings)`. Tests that vary `tempo` or `clock_source` need the helper's local `Params::default()` replaced by their own — in that case give the helper a fourth argument rather than duplicating it, or drive the clock inline as `settings_come_from_the_argument_not_the_global_params` does.

- [ ] **Step 8: Run the tests**

```bash
cargo test -p synth_core --lib
```

Expected: PASS, including `golden_vector_is_stable`. If `GOLDEN_HASH` fails here, a value the lead sees changed — find it; do not touch the constant.

- [ ] **Step 9: Run the full suite and clippy**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: tests green; the clippy line reports the same 39 as before.

- [ ] **Step 10: Commit**

```bash
git add crates/synth_core/src/sequencer.rs crates/synth_core/src/params.rs crates/synth_core/src/engine.rs
git commit -m "$(cat <<'EOF'
Give the sequencer its own settings and a slide flag

Two lines are about to run off one clock, so the three per-line
settings stop being read out of the global parameter block, and the
step's accent and slide reach the output instead of being dropped on
the swing path.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Generation — `slide_chance`, `accent_chance`, and the tie that holds its gate

Two new generative fields, drawn **conditionally** so the lead's RNG stream is byte-identical; plus the one behaviour a tie needs from the sequencer, which is that the note before it is still sounding when it lands.

**Files:**
- Modify: `crates/synth_core/src/sequencer.rs`
- Test: `crates/synth_core/src/sequencer.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `SeqSettings`, `Step { slide }`, `SeqOutput { accent, slide }` from Task 1; `Rng::chance(p: f32) -> bool`, `Rng::next_f32() -> f32`.
- Produces:
  - `GenerativeSettings` gains `pub slide_chance: f32` and `pub accent_chance: f32`. `GenerativeSettings::from_params` sets both to `0.0`.
  - `Sequencer::step` extends the gate to two full steps when the *next* step is a tie.

**Design note, extending the spec.** The spec defines slide as "does not retrigger either envelope", but says nothing about the gate. With the default gate of 0.6 the previous note is released 40% of a step before the tie arrives, so the amp envelope is already in release and there is nothing to glide from — the tie would sound retriggered anyway. The sequencer therefore looks one step ahead: when the next step is an active tie, the current note holds for two full steps instead of `gate`. Two steps rather than one covers the maximum swing delay of 0.6 and still guarantees release if the tie is edited away mid-flight. This only ever fires when `slide` is set, which the lead never sets, so the lead's timing is untouched.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/synth_core/src/sequencer.rs`:

```rust
    /// Generative settings for the tests below, with the two new chances
    /// spelled out so their effect is never accidental.
    fn gen_with(slide_chance: f32, accent_chance: f32) -> GenerativeSettings {
        GenerativeSettings {
            root: 0,
            scale: Scale::NaturalMinor,
            octave: 3,
            range: 2,
            density: 0.9,
            max_jump: 3.0,
            chord_bias: 0.6,
            length: 16,
            slide_chance,
            accent_chance,
        }
    }

    #[test]
    fn zero_chances_produce_no_slides_and_only_downbeat_accents() {
        let mut s = Sequencer::new(0x6EA5_0001);
        s.regenerate(&gen_with(0.0, 0.0));
        for (i, step) in s.pattern().iter().enumerate() {
            assert!(!step.slide, "step {i} was marked for slide at chance 0.0");
            assert_eq!(
                step.accent,
                i % 4 == 0,
                "step {i} accent should still be the downbeat marker alone"
            );
        }
    }

    #[test]
    fn nonzero_chances_draw_from_the_rng() {
        // The short-circuit is the whole reason the lead's stream is
        // unchanged. Prove the draw really is skipped at 0.0 by showing it
        // is *not* skipped above it: the extra draws shift the stream, so
        // the same seed yields different notes.
        let mut zero = Sequencer::new(0x6EA5_0002);
        zero.regenerate(&gen_with(0.0, 0.0));
        let zero_notes: Vec<u8> = zero.pattern().iter().map(|s| s.note).collect();

        let mut accented = Sequencer::new(0x6EA5_0002);
        accented.regenerate(&gen_with(0.0, 0.5));
        let accented_notes: Vec<u8> = accented.pattern().iter().map(|s| s.note).collect();

        assert_ne!(
            zero_notes, accented_notes,
            "a nonzero accent chance must consume RNG, shifting the melody"
        );

        let mut slid = Sequencer::new(0x6EA5_0002);
        slid.regenerate(&gen_with(1.0, 0.0));
        assert!(
            slid.pattern().iter().filter(|s| s.active).all(|s| s.slide),
            "at chance 1.0 every sounding step should be tied"
        );
    }

    #[test]
    fn a_tie_holds_the_note_before_it_past_the_gate() {
        let mut s = Sequencer::new(0x6EA5_0003);
        for i in 0..4 {
            s.set_step(
                i,
                Step { active: true, note: 40, velocity: 0.8, accent: false, slide: i == 1 },
            );
        }
        let mut c = Clock::new(48_000.0);
        c.start();
        // A short gate: without the lookahead, step 0 releases well before
        // step 1 arrives.
        let p = Params { seq_length: 4, seq_swing: 0.0, seq_gate: 0.2, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        let mut seen_first_on = false;
        let mut off_before_tie = false;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if out.note_on.is_some() {
                if seen_first_on {
                    // This is the tie. Stop here.
                    assert!(out.slide, "step 1 is the tie");
                    break;
                }
                seen_first_on = true;
                continue;
            }
            if seen_first_on && out.note_off.is_some() {
                off_before_tie = true;
            }
        }
        assert!(
            !off_before_tie,
            "the gate must not expire between a note and the tie that follows it"
        );
    }

    #[test]
    fn an_untied_step_still_honours_the_gate() {
        let mut s = Sequencer::new(0x6EA5_0004);
        for i in 0..4 {
            s.set_step(
                i,
                Step { active: true, note: 40, velocity: 0.8, accent: false, slide: false },
            );
        }
        let mut c = Clock::new(48_000.0);
        c.start();
        let p = Params { seq_length: 4, seq_swing: 0.0, seq_gate: 0.2, ..Params::default() };
        let settings = SeqSettings::from_params(&p);

        let mut seen_first_on = false;
        let mut off_before_next = false;
        for _ in 0..4_000 {
            let out = block(&mut s, &mut c, &settings);
            if seen_first_on && out.note_off.is_some() {
                off_before_next = true;
            }
            if out.note_on.is_some() {
                if seen_first_on {
                    break;
                }
                seen_first_on = true;
            }
        }
        assert!(
            off_before_next,
            "with no tie, a 0.2 gate must release long before the next step"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p synth_core --lib sequencer
```

Expected: compile errors — `GenerativeSettings` has no field `slide_chance`.

- [ ] **Step 3: Extend `GenerativeSettings`**

In `crates/synth_core/src/sequencer.rs`:

```rust
pub struct GenerativeSettings {
    pub root: u8,
    pub scale: crate::scale::Scale,
    pub octave: i32,
    pub range: u32,
    pub density: f32,
    pub max_jump: f32,
    pub chord_bias: f32,
    pub length: usize,
    /// Chance a step is tied to the one before it. `0.0` skips the draw
    /// entirely, which is what keeps the lead's RNG stream unchanged.
    pub slide_chance: f32,
    /// Chance a step is accented on top of the downbeats it already gets.
    /// `0.0` skips the draw, as above.
    pub accent_chance: f32,
}
```

and in `from_params`, which stays the lead's constructor:

```rust
    pub fn from_params(p: &Params) -> Self {
        Self {
            root: p.gen_root,
            scale: p.gen_scale,
            octave: p.gen_octave,
            range: p.gen_range,
            density: p.gen_density,
            max_jump: p.gen_max_jump,
            chord_bias: p.gen_chord_bias,
            length: p.seq_length,
            // The lead has neither control. Zero here is load-bearing: it is
            // what makes the two draws below short-circuit, leaving the lead's
            // RNG stream byte-identical and `GOLDEN_HASH` intact.
            slide_chance: 0.0,
            accent_chance: 0.0,
        }
    }
```

- [ ] **Step 4: Draw the two flags, conditionally**

At the tail of `regenerate`, replace the `Step` literal with:

```rust
            let velocity = if strong {
                0.85 + self.rng.next_f32() * 0.15
            } else {
                0.5 + self.rng.next_f32() * 0.25
            };

            // Both draws short-circuit at 0.0. This is not a micro-
            // optimisation: an unconditional draw would advance the RNG on
            // every step of every lead pattern and change every melody the
            // synth has ever generated.
            let mut accent = strong;
            if gen.accent_chance > 0.0 && !accent {
                accent = self.rng.chance(gen.accent_chance);
            }
            let slide = gen.slide_chance > 0.0 && self.rng.chance(gen.slide_chance);

            self.pattern[i] = Step { active: true, note, velocity, accent, slide };
```

- [ ] **Step 5: Hold the gate through a tie**

In `Sequencer::step`, after `let step = self.pattern[self.position];` and the `if !step.active { return; }` guard, work out whether the next step is a tie, and pass it to `trigger`:

```rust
        // A tie needs the note before it still sounding when it lands, or the
        // glide has nothing to glide from. Look one step ahead: hold this note
        // through the whole step rather than releasing it at the gate. Two
        // steps rather than one, so the maximum swing delay of 0.6 is covered
        // and the note still releases if the tie is edited away mid-flight.
        let following = self.pattern[(self.position + 1) % self.length];
        let hold = following.active && following.slide;
```

and thread `hold` into both `trigger` call sites and into `Pending`:

```rust
        if swing_delay > 0.0 {
            self.pending = Some(Pending {
                note: step.note,
                velocity: step.velocity,
                accent: step.accent,
                slide: step.slide,
                hold,
                samples_remaining: swing_delay,
            });
        } else {
            self.trigger(step.note, step.velocity, step.accent, step.slide, hold, s, clock, out);
        }
```

`Pending` gains the field:

```rust
#[derive(Debug, Clone, Copy)]
struct Pending {
    note: u8,
    velocity: f32,
    accent: bool,
    slide: bool,
    /// The step after this one is a tie, so ignore the gate and hold.
    hold: bool,
    samples_remaining: f32,
}
```

and `advance`'s pending branch passes it:

```rust
                self.trigger(
                    pending.note,
                    pending.velocity,
                    pending.accent,
                    pending.slide,
                    pending.hold,
                    s,
                    clock,
                    &mut out,
                );
```

`trigger` gains the parameter and the branch:

```rust
    #[allow(clippy::too_many_arguments)]
    fn trigger(
        &mut self,
        note: u8,
        velocity: f32,
        accent: bool,
        slide: bool,
        hold: bool,
        s: &SeqSettings,
        clock: ClockView,
        out: &mut SeqOutput,
    ) {
        if let Some(previous) = self.sounding.take() {
            out.note_off = Some(previous);
        }

        out.note_on = Some((note, velocity));
        out.accent = accent;
        out.slide = slide;
        self.sounding = Some(note);
        self.samples_until_off = if hold {
            2.0 * clock.samples_per_step
        } else {
            s.gate * clock.samples_per_step
        };
    }
```

- [ ] **Step 6: Run the tests**

```bash
cargo test -p synth_core --lib
```

Expected: PASS, `golden_vector_is_stable` included.

- [ ] **Step 7: Run the full suite and clippy**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: green; 39 warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/synth_core/src/sequencer.rs
git commit -m "$(cat <<'EOF'
Generate slides and accents, and let a tie hold its gate

Both draws short-circuit at zero so the lead's random stream, and
every melody it has ever written, are unchanged. A tie also extends
the note before it: without that the gate expires first and there is
nothing left to glide from.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `BassParams` and its atomic mirror

Eight fields, one plain struct and one atomic mirror, following `CompressorParams` / `SharedCompressor` exactly. No engine wiring yet — this task ends with a struct that round-trips and refuses NaN.

**Files:**
- Modify: `crates/synth_core/src/params.rs`
- Modify: `crates/synth_core/src/lib.rs`
- Test: `crates/synth_core/src/params.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `AtomicF32`, `AtomicEnum`, `fn sane(f32, f32) -> f32`, `crate::osc::Waveform` (which already has `from_u32`, clamping unknown values to `Saw`).
- Produces:
  - `pub struct BassParams { pub wave: Waveform, pub tune: f32, pub cutoff: f32, pub resonance: f32, pub env_mod: f32, pub decay: f32, pub accent: f32, pub slide_time: f32 }` with `Default`.
  - `pub struct SharedBass` with `fn new(&BassParams) -> Self`, `fn snapshot(&self) -> BassParams`, `fn apply(&self, &BassParams)`.

There is deliberately **no `level`** field: the bus owns the bass's level as `bass_gain` on the mixer strip, and two level controls for one signal is how the two drift apart. `wave` reuses `Waveform` rather than introducing a two-variant enum; the UI offers only Saw and Pulse, which is what the spec's range means in practice.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/synth_core/src/params.rs`:

```rust
    #[test]
    fn shared_bass_round_trips_and_rejects_nonsense() {
        let p = BassParams {
            wave: crate::osc::Waveform::Pulse,
            tune: -5.0,
            cutoff: 820.0,
            resonance: 0.9,
            env_mod: 4.5,
            decay: 0.12,
            accent: 0.8,
            slide_time: 0.2,
        };
        let shared = SharedBass::new(&p);
        assert_eq!(shared.snapshot(), p);

        // Everything the control side can write goes through `sane` and a
        // range clamp, so nothing non-finite or out of range reaches the
        // audio thread.
        shared.cutoff.set(f32::NAN);
        shared.resonance.set(40.0);
        shared.decay.set(-3.0);
        shared.slide_time.set(f32::INFINITY);
        let out = shared.snapshot();
        assert_eq!(out.cutoff, 300.0, "NaN falls back to the default");
        assert_eq!(out.resonance, 1.0);
        assert_eq!(out.decay, 0.02);
        assert_eq!(out.slide_time, 0.06);

        // `apply` is the inverse of `new`.
        let other = BassParams::default();
        shared.apply(&other);
        assert_eq!(shared.snapshot(), other);
    }
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p synth_core --lib shared_bass_round_trips
```

Expected: FAIL — `cannot find struct BassParams`.

- [ ] **Step 3: Add `BassParams`**

In `crates/synth_core/src/params.rs`, immediately after the `CompressorParams` block:

```rust
/// The bassline voice, as eight knobs.
///
/// A 303 is a small instrument on purpose: one oscillator, one lowpass, one
/// envelope aimed at the cutoff, and two per-step gestures — accent and slide
/// — that do the expressive work. There is no `level` here; the bus owns the
/// bass's level as `bass_gain`, and duplicating it would only let the two
/// drift apart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BassParams {
    /// Saw or pulse. The two the original offered, and the two that suit it.
    pub wave: crate::osc::Waveform,
    /// Transpose, in semitones.
    pub tune: f32,
    /// Filter cutoff before the envelope is added, in Hz.
    pub cutoff: f32,
    /// Filter resonance. High values are the point of the instrument.
    pub resonance: f32,
    /// How far the filter envelope opens the cutoff, in octaves.
    pub env_mod: f32,
    /// Filter envelope decay, in seconds. The single most important knob.
    pub decay: f32,
    /// How much an accented step adds: level, cutoff and envelope depth all
    /// come off this one control.
    pub accent: f32,
    /// How long a tied note takes to glide to its new pitch, in seconds.
    pub slide_time: f32,
}

impl Default for BassParams {
    fn default() -> Self {
        Self {
            wave: crate::osc::Waveform::Saw,
            tune: 0.0,
            cutoff: 300.0,
            resonance: 0.7,
            env_mod: 3.0,
            decay: 0.3,
            accent: 0.5,
            slide_time: 0.06,
        }
    }
}

/// The atomic mirror of [`BassParams`].
#[derive(Debug)]
pub struct SharedBass {
    pub wave: AtomicEnum,
    pub tune: AtomicF32,
    pub cutoff: AtomicF32,
    pub resonance: AtomicF32,
    pub env_mod: AtomicF32,
    pub decay: AtomicF32,
    pub accent: AtomicF32,
    pub slide_time: AtomicF32,
}

impl SharedBass {
    fn new(p: &BassParams) -> Self {
        Self {
            wave: AtomicEnum::new(p.wave as u32),
            tune: AtomicF32::new(p.tune),
            cutoff: AtomicF32::new(p.cutoff),
            resonance: AtomicF32::new(p.resonance),
            env_mod: AtomicF32::new(p.env_mod),
            decay: AtomicF32::new(p.decay),
            accent: AtomicF32::new(p.accent),
            slide_time: AtomicF32::new(p.slide_time),
        }
    }

    fn snapshot(&self) -> BassParams {
        BassParams {
            wave: crate::osc::Waveform::from_u32(self.wave.get()),
            tune: sane(self.tune.get(), 0.0).clamp(-12.0, 12.0),
            cutoff: sane(self.cutoff.get(), 300.0).clamp(20.0, 20_000.0),
            resonance: sane(self.resonance.get(), 0.7).clamp(0.0, 1.0),
            env_mod: sane(self.env_mod.get(), 3.0).clamp(0.0, 6.0),
            decay: sane(self.decay.get(), 0.3).clamp(0.02, 2.0),
            accent: sane(self.accent.get(), 0.5).clamp(0.0, 1.0),
            slide_time: sane(self.slide_time.get(), 0.06).clamp(0.01, 0.5),
        }
    }

    fn apply(&self, p: &BassParams) {
        self.wave.set(p.wave as u32);
        self.tune.set(p.tune);
        self.cutoff.set(p.cutoff);
        self.resonance.set(p.resonance);
        self.env_mod.set(p.env_mod);
        self.decay.set(p.decay);
        self.accent.set(p.accent);
        self.slide_time.set(p.slide_time);
    }
}
```

`sane` first, then `clamp`: `sane` removes the NaN, so the `clamp` that follows can never propagate one. This is the same order `SharedCompressor::snapshot` uses.

The test calls `SharedBass::new` and `snapshot` from inside the module, so the private visibility that `SharedCompressor` uses is fine.

- [ ] **Step 4: Re-export from the crate root**

In `crates/synth_core/src/lib.rs`, extend the params re-export:

```rust
pub use params::{
    BassParams, CompressorParams, Params, SharedParams, SidechainSource, Smoothed, VoiceMode,
};
```

- [ ] **Step 5: Run the test**

```bash
cargo test -p synth_core --lib shared_bass_round_trips
```

Expected: PASS.

- [ ] **Step 6: Run the full suite and clippy**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: green; 39 warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/synth_core/src/params.rs crates/synth_core/src/lib.rs
git commit -m "$(cat <<'EOF'
Describe the bassline voice as eight parameters

Level is deliberately absent: the bus owns it as bass_gain, and one
signal with two level controls is one too many.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `BassVoice` — the instrument, without accent or slide

One oscillator, one 24 dB lowpass, a decay-only filter envelope aimed at the cutoff, a near-instant amp envelope. Monophonic. This task builds the sound; Task 5 adds the two gestures.

**Files:**
- Create: `crates/synth_core/src/bass.rs`
- Modify: `crates/synth_core/src/lib.rs`
- Test: `crates/synth_core/src/bass.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `BassParams` (Task 3); `Oscillator::{new(sample_rate: f32, seed: u64), set_sample_rate, set_freq, set_pulse_width, next(Waveform) -> f32}`; `Filter::{new(sample_rate: f32), set_sample_rate, reset, set_params(cutoff_hz: f32, resonance: f32), process(f32) -> f32}` with public fields `mode: SvfMode` and `slope: Slope`; `Adsr::{new(sample_rate: f32), set_sample_rate, set_settings(AdsrSettings), gate_on(reset: bool), gate_off, level() -> f32, is_active() -> bool, next() -> f32}`; `crate::note::midi_to_hz(note: f32) -> f32`.
- Produces:
  - `pub struct BassVoice` with `pub fn new(sample_rate: f32, seed: u64) -> Self`, `pub fn set_sample_rate(&mut self, sample_rate: f32)`, `pub fn note_on(&mut self, note: u8, accent: bool, slide: bool)`, `pub fn note_off(&mut self)`, `pub fn silence(&mut self)`, `pub fn is_active(&self) -> bool`, `pub fn process_block(&mut self, out: &mut [f32], p: &BassParams)`.

`process_block` **adds nothing** — it writes, so the caller does not have to clear a buffer first. `note_on` takes `accent` and `slide` from this task on, so no signature changes in Task 5; both are simply latched and, for now, only `accent` is stored.

- [ ] **Step 1: Write the failing tests**

Create `crates/synth_core/src/bass.rs` with only the test module and the imports, so the tests compile against a type that does not exist yet:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Renders `seconds` of audio and hands back the peak absolute sample.
    fn peak(v: &mut BassVoice, p: &BassParams, seconds: f32) -> f32 {
        let mut block = [0.0f32; 64];
        let blocks = (seconds * SR / 64.0) as usize;
        let mut worst = 0.0f32;
        for _ in 0..blocks {
            v.process_block(&mut block, p);
            for s in block {
                assert!(s.is_finite(), "the bass produced a non-finite sample");
                worst = worst.max(s.abs());
            }
        }
        worst
    }

    #[test]
    fn a_gated_note_makes_sound_and_silence_stops_it() {
        let mut v = BassVoice::new(SR, 0xB455_0001);
        let p = BassParams::default();
        assert!(!v.is_active(), "a fresh voice is silent");

        v.note_on(40, false, false);
        assert!(v.is_active());
        assert!(peak(&mut v, &p, 0.05) > 0.01, "a gated note should sound");

        v.silence();
        assert!(!v.is_active());
        assert!(peak(&mut v, &p, 0.05) < 1e-6, "silence must be silent");
    }

    #[test]
    fn the_filter_envelope_decays_to_zero_while_the_gate_is_held() {
        // Sustain is zero by design: the cutoff sweep is the instrument, and
        // it has to finish even on a long note.
        let mut v = BassVoice::new(SR, 0xB455_0002);
        let p = BassParams { decay: 0.05, ..BassParams::default() };
        v.note_on(40, false, false);
        let mut block = [0.0f32; 64];
        for _ in 0..(0.4 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        assert!(
            v.filter_env_level() < 0.01,
            "the filter envelope should be spent while the gate is still held"
        );
        assert!(v.is_active(), "the amp envelope is still holding the note");
    }

    #[test]
    fn env_mod_opens_the_filter() {
        // More envelope depth means more high-frequency energy early in the
        // note, which shows up as a bigger peak through a resonant lowpass.
        let mut closed = BassVoice::new(SR, 0xB455_0003);
        let mut open = BassVoice::new(SR, 0xB455_0003);
        let shut = BassParams { env_mod: 0.0, cutoff: 120.0, ..BassParams::default() };
        let wide = BassParams { env_mod: 5.0, cutoff: 120.0, ..BassParams::default() };
        closed.note_on(40, false, false);
        open.note_on(40, false, false);
        let a = peak(&mut closed, &shut, 0.1);
        let b = peak(&mut open, &wide, 0.1);
        assert!(b > a * 1.5, "env_mod 5.0 ({b}) should be far brighter than 0.0 ({a})");
    }

    #[test]
    fn tune_transposes_the_note() {
        let mut v = BassVoice::new(SR, 0xB455_0004);
        v.note_on(45, false, false);
        let plain = BassParams { tune: 0.0, ..BassParams::default() };
        let up = BassParams { tune: 12.0, ..BassParams::default() };
        let mut block = [0.0f32; 64];
        v.process_block(&mut block, &plain);
        let low = v.current_hz();
        v.process_block(&mut block, &up);
        let high = v.current_hz();
        assert!(
            (high / low - 2.0).abs() < 0.01,
            "twelve semitones should double the frequency: {low} -> {high}"
        );
    }

    #[test]
    fn nonsense_parameters_never_produce_nonsense_audio() {
        // `BassParams` reaching `process_block` has already been through
        // `SharedBass::snapshot`, but the voice is a public type and must not
        // rely on that.
        let mut v = BassVoice::new(SR, 0xB455_0005);
        let p = BassParams {
            cutoff: f32::NAN,
            resonance: f32::NAN,
            env_mod: f32::NAN,
            tune: f32::NAN,
            ..BassParams::default()
        };
        v.note_on(40, false, false);
        let mut block = [0.0f32; 64];
        for _ in 0..200 {
            v.process_block(&mut block, &p);
            for s in block {
                assert!(s.is_finite(), "NaN parameters leaked into the audio");
            }
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p synth_core --lib bass
```

Expected: the module is not declared yet, so nothing runs. Add `pub mod bass;` to `crates/synth_core/src/lib.rs` beside `pub mod voice;` and re-run: FAIL with `cannot find type BassVoice`.

- [ ] **Step 3: Write `BassVoice`**

Above the test module in `crates/synth_core/src/bass.rs`:

```rust
//! The bassline voice: one oscillator, one lowpass, and an envelope aimed at
//! the cutoff.
//!
//! This is a peer of [`crate::voice::Voice`], not a variant of it. `Voice` is
//! polyphonic, has two oscillators, an LFO destination and a dozen parameters;
//! a 303 is one voice, one filter and eight knobs, and the two gestures that
//! make it an instrument — accent and slide — have no analogue in the
//! polyphonic voice at all. Sharing the code would mean growing `Voice` with
//! flags that are always false for every note it will ever play.

use crate::env::{Adsr, AdsrSettings};
use crate::filter::{Filter, Slope, SvfMode};
use crate::note::midi_to_hz;
use crate::osc::Oscillator;
use crate::params::BassParams;

/// Both envelopes attack in three milliseconds: fast enough to click like the
/// original, slow enough not to alias.
const ATTACK_S: f32 = 0.003;

/// The amp envelope's release. Short, because the sequencer's gate is what
/// actually decides note length here.
const AMP_RELEASE_S: f32 = 0.008;

/// One monophonic bassline voice.
#[derive(Debug)]
pub struct BassVoice {
    sample_rate: f32,
    osc: Oscillator,
    filter: Filter,
    amp_env: Adsr,
    filter_env: Adsr,
    /// The note the voice is playing, as MIDI.
    note: u8,
    gate: bool,
    /// Latched at note-on: editing the step mid-note must not change the note
    /// already sounding.
    accented: bool,
    /// Current pitch in semitones, which may sit between notes during a slide.
    pitch: f32,
    /// Where a slide is heading.
    target_pitch: f32,
    /// Per-sample one-pole coefficient for the slide. `0.0` means jump.
    glide_coef: f32,
    /// The frequency the oscillator was last set to, for tests and for the
    /// pitch update to compare against.
    current_hz: f32,
}

impl BassVoice {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        let mut filter = Filter::new(sample_rate);
        // Fixed, both of them. A 303's filter is a 24 dB lowpass and nothing
        // else; making either selectable would be a different instrument.
        filter.mode = SvfMode::Lowpass;
        filter.slope = Slope::Db24;

        let mut amp_env = Adsr::new(sample_rate);
        amp_env.set_settings(AdsrSettings {
            attack: ATTACK_S,
            decay: 0.0,
            sustain: 1.0,
            release: AMP_RELEASE_S,
        });

        let filter_env = Adsr::new(sample_rate);

        Self {
            sample_rate,
            osc: Oscillator::new(sample_rate, seed),
            filter,
            amp_env,
            filter_env,
            note: 40,
            gate: false,
            accented: false,
            pitch: 40.0,
            target_pitch: 40.0,
            glide_coef: 0.0,
            current_hz: midi_to_hz(40.0),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.osc.set_sample_rate(sample_rate);
        self.filter.set_sample_rate(sample_rate);
        self.amp_env.set_sample_rate(sample_rate);
        self.filter_env.set_sample_rate(sample_rate);
    }

    /// Starts a note.
    ///
    /// `accent` is latched here rather than read per sample, so an edit to the
    /// step mid-note cannot change what is already sounding. `slide` is the
    /// tie; Task 5 gives it its behaviour.
    pub fn note_on(&mut self, note: u8, accent: bool, _slide: bool) {
        self.note = note;
        self.gate = true;
        self.accented = accent;
        self.target_pitch = note as f32;
        self.pitch = self.target_pitch;
        self.amp_env.gate_on(true);
        self.filter_env.gate_on(true);
    }

    pub fn note_off(&mut self) {
        self.gate = false;
        self.amp_env.gate_off();
        self.filter_env.gate_off();
    }

    /// Cuts the voice dead, envelopes and filter state together. For the
    /// transport stopping and for panic.
    pub fn silence(&mut self) {
        self.gate = false;
        self.amp_env.hard_reset();
        self.filter_env.hard_reset();
        self.filter.reset();
    }

    pub fn is_active(&self) -> bool {
        self.amp_env.is_active()
    }

    /// Renders one block, overwriting `out`.
    pub fn process_block(&mut self, out: &mut [f32], p: &BassParams) {
        // Everything that arrives from the control side is bounded here as
        // well as in `SharedBass::snapshot`: `BassVoice` is public, so it
        // cannot assume it was called through the mirror. `max`/`min` rather
        // than `clamp`, because `clamp` propagates NaN and these two do not.
        let tune = if p.tune.is_finite() { p.tune } else { 0.0 };
        let base_cutoff = sane_or(p.cutoff, 300.0).max(20.0).min(20_000.0);
        let resonance = sane_or(p.resonance, 0.7).max(0.0).min(1.0);
        let env_mod = sane_or(p.env_mod, 3.0).max(0.0).min(6.0);
        let decay = sane_or(p.decay, 0.3).max(0.02).min(2.0);

        // Decay-only: sustain is zero, so the sweep finishes even under a held
        // gate. Release matches decay, so letting go mid-sweep sounds like the
        // sweep continuing rather than a second gesture.
        self.filter_env.set_settings(AdsrSettings {
            attack: ATTACK_S,
            decay,
            sustain: 0.0,
            release: decay,
        });

        self.set_glide(p.slide_time);

        for sample in out.iter_mut() {
            // Pitch first: a slide moves it every sample.
            if self.glide_coef > 0.0 {
                self.pitch = self.target_pitch + (self.pitch - self.target_pitch) * self.glide_coef;
            } else {
                self.pitch = self.target_pitch;
            }
            self.current_hz = midi_to_hz(self.pitch + tune);
            self.osc.set_freq(self.current_hz);

            let env = self.filter_env.next();
            let amp = self.amp_env.next();

            // Octaves rather than Hz: an envelope that adds 3 octaves sweeps
            // the same musical distance from 80 Hz as it does from 800.
            let octaves = env_mod * env;
            let cutoff = (base_cutoff * octaves.exp2()).max(20.0).min(20_000.0);
            self.filter.set_params(cutoff, resonance);

            let raw = self.osc.next(p.wave);
            let filtered = self.filter.process(raw);
            let value = filtered * amp;
            *sample = if value.is_finite() { value } else { 0.0 };
        }
    }

    /// The filter envelope's current level. Test-facing: the sweep finishing
    /// under a held gate is the behaviour that makes this a 303 and not a
    /// generic mono synth, so it is worth being able to assert on.
    pub fn filter_env_level(&self) -> f32 {
        self.filter_env.level()
    }

    /// The frequency the oscillator is currently running at.
    pub fn current_hz(&self) -> f32 {
        self.current_hz
    }

    fn set_glide(&mut self, seconds: f32) {
        // Same one-pole-in-semitone-space form `Voice::set_glide` uses, so the
        // two instruments slide with the same curve.
        self.glide_coef = if !seconds.is_finite() || seconds <= 0.0001 {
            0.0
        } else {
            (-1.0 / (seconds * self.sample_rate)).exp()
        };
    }
}

/// `params::sane` is private to that module; this is the same idea, local.
fn sane_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}
```

Note that `set_glide` is called every block but `glide_coef` is only *used* when a slide is in flight — Task 4 always sets `pitch == target_pitch` at note-on, so the glide is a no-op until Task 5 stops doing that.

- [ ] **Step 4: Declare and export the module**

`crates/synth_core/src/lib.rs`:

```rust
pub mod bass;
```

beside `pub mod voice;`, and:

```rust
pub use bass::BassVoice;
```

beside the other re-exports.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p synth_core --lib bass
```

Expected: PASS, all five.

- [ ] **Step 6: Run the full suite and clippy**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: green; 39 warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/synth_core/src/bass.rs crates/synth_core/src/lib.rs
git commit -m "$(cat <<'EOF'
Add a monophonic bassline voice

One oscillator, a fixed 24 dB lowpass and a decay-only envelope aimed
at the cutoff. Sustain is zero on purpose: the sweep is the
instrument, so it finishes even under a held gate.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Accent and slide

The two gestures. Accent adds level, cutoff and envelope depth from one knob and is latched at note-on. Slide suppresses the envelope retrigger and glides.

**Files:**
- Modify: `crates/synth_core/src/bass.rs`
- Test: `crates/synth_core/src/bass.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `BassVoice` from Task 4, `BassParams::{accent, slide_time}`.
- Produces: no signature changes. `note_on(&mut self, note: u8, accent: bool, slide: bool)` gains behaviour for its third argument.

The accent table, exactly:

| What accent does | Formula |
|---|---|
| Level | `1.0 + p.accent * 0.5` |
| Cutoff | `accent_octaves = p.accent * 1.5`, added to the envelope's octaves |
| Envelope depth | `env_mod * (1.0 + p.accent)` |

All three are neutral on an unaccented step: multiply by `1.0`, add `0.0`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/synth_core/src/bass.rs`:

```rust
    #[test]
    fn an_accented_note_is_louder_and_brighter() {
        let p = BassParams { accent: 1.0, cutoff: 150.0, ..BassParams::default() };
        let mut plain = BassVoice::new(SR, 0xB455_0011);
        let mut loud = BassVoice::new(SR, 0xB455_0011);
        plain.note_on(40, false, false);
        loud.note_on(40, true, false);
        let a = peak(&mut plain, &p, 0.08);
        let b = peak(&mut loud, &p, 0.08);
        assert!(b > a * 1.2, "an accented note ({b}) should top an unaccented one ({a})");
    }

    #[test]
    fn accent_is_neutral_when_the_knob_is_at_zero() {
        let p = BassParams { accent: 0.0, ..BassParams::default() };
        let mut plain = BassVoice::new(SR, 0xB455_0012);
        let mut marked = BassVoice::new(SR, 0xB455_0012);
        plain.note_on(40, false, false);
        marked.note_on(40, true, false);
        let a = peak(&mut plain, &p, 0.08);
        let b = peak(&mut marked, &p, 0.08);
        assert!((a - b).abs() < 1e-6, "accent 0.0 should change nothing: {a} vs {b}");
    }

    #[test]
    fn accent_is_latched_at_note_on() {
        // The accent belongs to the note, not to the knob: turning the knob
        // mid-note changes how loud the *next* accented note is, not this one.
        let mut v = BassVoice::new(SR, 0xB455_0013);
        v.note_on(40, true, false);
        assert!(v.is_accented());
        v.note_on(40, false, false);
        assert!(!v.is_accented());
    }

    #[test]
    fn a_tie_glides_and_does_not_retrigger() {
        let p = BassParams { slide_time: 0.1, ..BassParams::default() };
        let mut v = BassVoice::new(SR, 0xB455_0014);
        let mut block = [0.0f32; 64];

        v.note_on(40, false, false);
        for _ in 0..(0.2 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        let settled = v.filter_env_level();

        // The tie: same voice, new note, no retrigger.
        v.note_on(52, false, true);
        v.process_block(&mut block, &p);
        assert!(
            v.filter_env_level() <= settled,
            "a tie must not restart the filter envelope"
        );
        let start_hz = v.current_hz();

        // Partway through the glide the pitch is between the two notes.
        for _ in 0..(0.03 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        let mid_hz = v.current_hz();
        assert!(
            mid_hz > start_hz && mid_hz < midi_to_hz(52.0),
            "pitch should be mid-glide: {start_hz} -> {mid_hz} -> {}",
            midi_to_hz(52.0)
        );

        // And it gets there.
        for _ in 0..(0.6 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        assert!((v.current_hz() / midi_to_hz(52.0) - 1.0).abs() < 0.01);
    }

    #[test]
    fn an_untied_note_jumps_and_retriggers() {
        let p = BassParams { slide_time: 0.1, ..BassParams::default() };
        let mut v = BassVoice::new(SR, 0xB455_0015);
        let mut block = [0.0f32; 64];

        v.note_on(40, false, false);
        for _ in 0..(0.5 * SR / 64.0) as usize {
            v.process_block(&mut block, &p);
        }
        assert!(v.filter_env_level() < 0.01, "the envelope should be spent");

        v.note_on(52, false, false);
        v.process_block(&mut block, &p);
        assert!(v.filter_env_level() > 0.01, "an untied note restarts the envelope");
        assert!(
            (v.current_hz() / midi_to_hz(52.0) - 1.0).abs() < 0.01,
            "an untied note jumps straight to pitch"
        );
    }

    #[test]
    fn a_tie_from_silence_retriggers_instead() {
        // Nothing to slide from. Falling back to a normal note-on is the only
        // sensible reading, and it is what step 0 of a pattern needs.
        let p = BassParams::default();
        let mut v = BassVoice::new(SR, 0xB455_0016);
        v.note_on(40, false, true);
        assert!(v.is_active(), "a tie with nothing sounding must still start a note");
        assert!(v.filter_env_level() > 0.0, "and must trigger its envelope");
        assert!((v.current_hz() / midi_to_hz(40.0) - 1.0).abs() < 0.01);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p synth_core --lib bass
```

Expected: FAIL — no method `is_accented`; `a_tie_glides_and_does_not_retrigger` fails because `note_on` currently always retriggers.

- [ ] **Step 3: Give `slide` its behaviour**

Replace `note_on` in `crates/synth_core/src/bass.rs`:

```rust
    /// Starts a note.
    ///
    /// `accent` is latched here rather than read per sample, so editing the
    /// step mid-note cannot change what is already sounding.
    ///
    /// `slide` is the tie. A tied note keeps both envelopes exactly where they
    /// are and lets the glide walk the pitch to its new home — not
    /// retriggering is the whole point, because a retriggered note cannot
    /// produce the legato squelch the gesture exists for. A tie with nothing
    /// sounding has nothing to slide from, so it falls back to a normal
    /// note-on; that covers step 0 of a pattern and a tie after a rest.
    pub fn note_on(&mut self, note: u8, accent: bool, slide: bool) {
        self.note = note;
        self.gate = true;
        self.accented = accent;
        self.target_pitch = note as f32;

        if slide && self.is_active() {
            return;
        }

        self.pitch = self.target_pitch;
        self.amp_env.gate_on(true);
        self.filter_env.gate_on(true);
    }

    /// Whether the sounding note was started as an accent.
    pub fn is_accented(&self) -> bool {
        self.accented
    }
```

- [ ] **Step 4: Give `accent` its three effects**

In `process_block`, after the existing bounds work, add the accent terms and use them:

```rust
        let accent_amount = sane_or(p.accent, 0.5).max(0.0).min(1.0);
        // One knob, three destinations. All three are neutral on an
        // unaccented step: nothing is added and nothing is scaled.
        let (accent_level, accent_octaves, accent_depth) = if self.accented {
            (1.0 + accent_amount * 0.5, accent_amount * 1.5, 1.0 + accent_amount)
        } else {
            (1.0, 0.0, 1.0)
        };
        let env_mod = env_mod * accent_depth;
```

and inside the per-sample loop, replace the cutoff and the output lines:

```rust
            let octaves = env_mod * env + accent_octaves;
            let cutoff = (base_cutoff * octaves.exp2()).max(20.0).min(20_000.0);
            self.filter.set_params(cutoff, resonance);

            let raw = self.osc.next(p.wave);
            let filtered = self.filter.process(raw);
            let value = filtered * amp * accent_level;
            *sample = if value.is_finite() { value } else { 0.0 };
```

`let env_mod = env_mod * accent_depth;` shadows the bounded value rather than introducing a second name; the result is still bounded, because `accent_depth` is at most 2.0 and the cutoff is clamped after the exponent anyway.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p synth_core --lib bass
```

Expected: PASS, all eleven.

- [ ] **Step 6: Run the full suite and clippy**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: green; 39 warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/synth_core/src/bass.rs
git commit -m "$(cat <<'EOF'
Give the bassline accent and slide

Accent is one knob reaching level, cutoff and envelope depth, latched
at note-on so an edit cannot change a note already sounding. Slide
suppresses the retrigger and glides, falling back to a normal note-on
when there is nothing to slide from.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: The parameters — bus, generator, mirror and telemetry

Everything the engine and the UI need to address the bass, in `params.rs`. No behaviour yet: this task ends with parameters that snapshot, apply and mirror correctly.

**Files:**
- Modify: `crates/synth_core/src/params.rs`
- Modify: `crates/synth_core/src/sequencer.rs` (`GenerativeSettings::for_bass`, `SeqSettings::for_bass`)
- Test: `crates/synth_core/src/params.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `BassParams` / `SharedBass` (Task 3), `SeqSettings` / `GenerativeSettings` (Tasks 1–2), `pack_step` / `unpack_step` with slide (Task 1).
- Produces, on `Params`:
  - `pub bass_enabled: bool` (default **`false`**), `pub bass_gain: f32` (1.0), `pub bass_send: f32` (0.0), `pub comp_bass: CompressorParams`, `pub bass: BassParams`
  - `pub bass_seq_length: usize` (16), `pub bass_gen_octave: i32` (2), `pub bass_gen_range: u32` (1), `pub bass_gen_density: f32` (0.85), `pub bass_gen_max_jump: f32` (5.0), `pub bass_gen_chord_bias: f32` (0.8), `pub bass_slide_chance: f32` (0.25), `pub bass_accent_chance: f32` (0.3)
- Produces, on `SharedParams`: the atomic mirror of each of the above, plus `pub bass_pattern: [AtomicU32; MAX_STEPS]`, `pub bass_pattern_len: AtomicU32`, `pub bass_gen_seed: AtomicU64`, `pub bass_regenerate: AtomicU32`, and the telemetry pair `pub bass_step: AtomicU32`, `pub comp_bass_gr: AtomicF32`.
- Produces, methods on `SharedParams`: `publish_bass_step(&self, index: usize, step: &Step)`, `publish_bass_len(&self, len: usize)`, `read_bass_step(&self, index: usize) -> Step`, `read_bass_pattern(&self) -> Pattern`, `regenerate_bass(&self)`.
- Produces: `GenerativeSettings::for_bass(p: &Params) -> Self` and `SeqSettings::for_bass(p: &Params) -> Self`.

`bass_enabled` defaults to `false` so the existing golden render stays bit-identical. `bass_send` defaults to `0.0` for the same reason a new send always should: silence until asked. The bass takes `seq_swing` and `seq_gate` from the lead — the spec gives the bass its own *generator* settings and its own length, not its own groove, and a bass swinging differently from the lead is a bug rather than a feature.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/synth_core/src/params.rs`:

```rust
    #[test]
    fn the_bass_is_off_by_default_and_round_trips() {
        let p = Params::default();
        assert!(!p.bass_enabled, "the bass must be silent until switched on");
        assert_eq!(p.bass_send, 0.0, "a new send starts closed");

        let shared = SharedParams::from_params(&p);
        let back = shared.snapshot();
        assert!(!back.bass_enabled);
        assert_eq!(back.bass_gain, p.bass_gain);
        assert_eq!(back.bass_seq_length, p.bass_seq_length);
        assert_eq!(back.bass, p.bass);

        let changed = Params {
            bass_enabled: true,
            bass_gain: 0.6,
            bass_send: 0.4,
            bass_seq_length: 8,
            bass_gen_octave: 1,
            bass_slide_chance: 0.75,
            ..Params::default()
        };
        shared.apply(&changed);
        let out = shared.snapshot();
        assert!(out.bass_enabled);
        assert_eq!(out.bass_gain, 0.6);
        assert_eq!(out.bass_send, 0.4);
        assert_eq!(out.bass_seq_length, 8);
        assert_eq!(out.bass_gen_octave, 1);
        assert_eq!(out.bass_slide_chance, 0.75);
    }

    #[test]
    fn the_bass_pattern_mirror_is_separate_from_the_lead_s() {
        let shared = SharedParams::from_params(&Params::default());
        let lead = crate::sequencer::Step {
            active: true, note: 72, velocity: 1.0, accent: false, slide: false,
        };
        let bass = crate::sequencer::Step {
            active: true, note: 36, velocity: 0.6, accent: true, slide: true,
        };
        shared.publish_step(3, &lead);
        shared.publish_bass_step(3, &bass);
        shared.publish_bass_len(8);

        assert_eq!(shared.read_step(3).note, 72);
        assert_eq!(shared.read_bass_step(3).note, 36);
        assert!(shared.read_bass_step(3).slide);
        assert_eq!(shared.read_bass_pattern().len(), 8);
        assert_eq!(shared.read_pattern().len(), Params::default().seq_length);
    }

    #[test]
    fn the_bass_generator_reads_its_own_knobs_and_the_lead_s_key() {
        use crate::sequencer::{GenerativeSettings, SeqSettings};
        let p = Params {
            gen_root: 7,
            gen_octave: 5,
            gen_density: 0.2,
            seq_length: 32,
            bass_gen_octave: 1,
            bass_gen_density: 0.95,
            bass_seq_length: 8,
            bass_slide_chance: 0.5,
            bass_accent_chance: 0.4,
            ..Params::default()
        };
        let g = GenerativeSettings::for_bass(&p);
        assert_eq!(g.root, 7, "key is shared with the lead");
        assert_eq!(g.scale, p.gen_scale, "so is the scale");
        assert_eq!(g.octave, 1, "register is the bass's own");
        assert_eq!(g.density, 0.95);
        assert_eq!(g.length, 8);
        assert_eq!(g.slide_chance, 0.5);
        assert_eq!(g.accent_chance, 0.4);

        let s = SeqSettings::for_bass(&p);
        assert_eq!(s.length, 8, "the bass owns its loop length");
        assert_eq!(s.swing, p.seq_swing, "and shares the groove");
        assert_eq!(s.gate, p.seq_gate);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p synth_core --lib params
```

Expected: FAIL — `Params` has no field `bass_enabled`.

- [ ] **Step 3: Add the fields to `Params`**

In `crates/synth_core/src/params.rs`, in the `Params` struct beside `synth_gain` / `synth_send` / `comp_synth`:

```rust
    /// Whether the bassline is audible. Off by default: adding an instrument
    /// should never change what an existing patch sounds like.
    pub bass_enabled: bool,
    /// Level of the bass bus alone, under the master.
    pub bass_gain: f32,
    /// How much of the bass bus is sent to the effects return.
    pub bass_send: f32,
    /// The compressor inserted on the bass bus, before the send tap.
    pub comp_bass: CompressorParams,
    /// The bassline voice itself.
    pub bass: BassParams,
```

and beside the `seq_*` / `gen_*` block:

```rust
    /// Loop length of the bass line, independent of `seq_length`.
    pub bass_seq_length: usize,
    /// The bass generator's register. Low, by default: that is the job.
    pub bass_gen_octave: i32,
    /// How many octaves the bass line may span.
    pub bass_gen_range: u32,
    /// How often a bass step has a note rather than a rest.
    pub bass_gen_density: f32,
    /// How far the bass line may leap, in scale degrees.
    pub bass_gen_max_jump: f32,
    /// How strongly bass downbeats land on root, third and fifth.
    pub bass_gen_chord_bias: f32,
    /// Chance a generated bass step is tied to the one before it.
    pub bass_slide_chance: f32,
    /// Chance a generated bass step is accented off the downbeat.
    pub bass_accent_chance: f32,
```

and the matching defaults in `impl Default for Params`:

```rust
            bass_enabled: false,
            bass_gain: 1.0,
            bass_send: 0.0,
            comp_bass: CompressorParams::default(),
            bass: BassParams::default(),
            bass_seq_length: 16,
            bass_gen_octave: 2,
            bass_gen_range: 1,
            bass_gen_density: 0.85,
            bass_gen_max_jump: 5.0,
            bass_gen_chord_bias: 0.8,
            bass_slide_chance: 0.25,
            bass_accent_chance: 0.3,
```

- [ ] **Step 4: Mirror them on `SharedParams`**

Fields, beside the corresponding synth ones:

```rust
    pub bass_enabled: AtomicBool32,
    pub bass_gain: AtomicF32,
    pub bass_send: AtomicF32,
    pub comp_bass: SharedCompressor,
    pub bass: SharedBass,

    pub bass_seq_length: AtomicEnum,
    pub bass_gen_octave: AtomicEnum,
    pub bass_gen_range: AtomicEnum,
    pub bass_gen_density: AtomicF32,
    pub bass_gen_max_jump: AtomicF32,
    pub bass_gen_chord_bias: AtomicF32,
    pub bass_slide_chance: AtomicF32,
    pub bass_accent_chance: AtomicF32,

    /// The bass pattern, mirrored for the control side exactly as the lead's
    /// is. Its own length, because a bass line is not the same length as the
    /// melody unless somebody says so.
    pub bass_pattern: [AtomicU32; crate::sequencer::MAX_STEPS],
    pub bass_pattern_len: AtomicU32,
    /// Seed for the bass generator, independent of the lead's.
    pub bass_gen_seed: AtomicU64,
    /// Bumped to ask the audio thread for a new bass line.
    pub bass_regenerate: AtomicU32,

    /// Bass sequencer step currently playing. Telemetry.
    pub bass_step: AtomicU32,
    /// Gain reduction the bass-bus compressor is applying, in positive dB.
    /// Telemetry.
    pub comp_bass_gr: AtomicF32,
```

`from_params`:

```rust
            bass_enabled: AtomicBool32::new(p.bass_enabled),
            bass_gain: AtomicF32::new(p.bass_gain),
            bass_send: AtomicF32::new(p.bass_send),
            comp_bass: SharedCompressor::new(&p.comp_bass),
            bass: SharedBass::new(&p.bass),

            bass_seq_length: AtomicEnum::new(p.bass_seq_length as u32),
            bass_gen_octave: AtomicEnum::new(p.bass_gen_octave as u32),
            bass_gen_range: AtomicEnum::new(p.bass_gen_range),
            bass_gen_density: AtomicF32::new(p.bass_gen_density),
            bass_gen_max_jump: AtomicF32::new(p.bass_gen_max_jump),
            bass_gen_chord_bias: AtomicF32::new(p.bass_gen_chord_bias),
            bass_slide_chance: AtomicF32::new(p.bass_slide_chance),
            bass_accent_chance: AtomicF32::new(p.bass_accent_chance),

            bass_pattern: core::array::from_fn(|_| AtomicU32::new(0)),
            bass_pattern_len: AtomicU32::new(p.bass_seq_length as u32),
            bass_gen_seed: AtomicU64::new(0x5EED_1234_ABCD_0B45),
            bass_regenerate: AtomicU32::new(0),

            bass_step: AtomicU32::new(0),
            comp_bass_gr: AtomicF32::new(0.0),
```

If the existing `pattern` field is built some other way than `core::array::from_fn` — check line 796 — use whatever form is already there, so the two read the same.

`snapshot`:

```rust
            bass_enabled: self.bass_enabled.get(),
            bass_gain: sane(self.bass_gain.get(), 1.0).clamp(0.0, 2.0),
            bass_send: clamp01(self.bass_send.get()),
            comp_bass: self.comp_bass.snapshot(),
            bass: self.bass.snapshot(),

            bass_seq_length: (self.bass_seq_length.get() as usize)
                .clamp(1, crate::sequencer::MAX_STEPS),
            bass_gen_octave: (self.bass_gen_octave.get() as i32).clamp(0, 7),
            bass_gen_range: self.bass_gen_range.get().clamp(1, 4),
            bass_gen_density: clamp01(self.bass_gen_density.get()),
            bass_gen_max_jump: sane(self.bass_gen_max_jump.get(), 5.0).clamp(1.0, 12.0),
            bass_gen_chord_bias: clamp01(self.bass_gen_chord_bias.get()),
            bass_slide_chance: clamp01(self.bass_slide_chance.get()),
            bass_accent_chance: clamp01(self.bass_accent_chance.get()),
```

`apply`:

```rust
        self.bass_enabled.set(p.bass_enabled);
        self.bass_gain.set(p.bass_gain);
        self.bass_send.set(p.bass_send);
        self.comp_bass.apply(&p.comp_bass);
        self.bass.apply(&p.bass);

        self.bass_seq_length.set(p.bass_seq_length as u32);
        self.bass_gen_octave.set(p.bass_gen_octave as u32);
        self.bass_gen_range.set(p.bass_gen_range);
        self.bass_gen_density.set(p.bass_gen_density);
        self.bass_gen_max_jump.set(p.bass_gen_max_jump);
        self.bass_gen_chord_bias.set(p.bass_gen_chord_bias);
        self.bass_slide_chance.set(p.bass_slide_chance);
        self.bass_accent_chance.set(p.bass_accent_chance);
```

The telemetry atomics — `bass_step`, `comp_bass_gr` — appear in `from_params` only, never in `snapshot` or `apply`. That is the existing rule for `comp_synth_gr` and it holds here.

- [ ] **Step 5: Add the mirror methods**

Beside `publish_step` / `publish_len` / `read_step` / `read_pattern`:

```rust
    /// Copies one bass step into the mirror. Same contract as
    /// [`SharedParams::publish_step`].
    pub fn publish_bass_step(&self, index: usize, step: &crate::sequencer::Step) {
        if let Some(slot) = self.bass_pattern.get(index) {
            slot.store(pack_step(step), REL);
        }
    }

    pub fn publish_bass_len(&self, len: usize) {
        self.bass_pattern_len.store(len as u32, REL);
    }

    pub fn read_bass_step(&self, index: usize) -> crate::sequencer::Step {
        match self.bass_pattern.get(index) {
            Some(slot) => unpack_step(slot.load(REL)),
            None => crate::sequencer::Step::default(),
        }
    }

    /// The bass pattern as the audio thread last published it.
    pub fn read_bass_pattern(&self) -> crate::sequencer::Pattern {
        let mut steps = [crate::sequencer::Step::default(); crate::sequencer::MAX_STEPS];
        for (index, step) in steps.iter_mut().enumerate() {
            *step = self.read_bass_step(index);
        }
        let len = (self.bass_pattern_len.load(REL) as usize)
            .clamp(1, crate::sequencer::MAX_STEPS);
        crate::sequencer::Pattern::new(steps, len)
    }

    /// Asks the audio thread for a new bass line, without needing a queue slot.
    pub fn regenerate_bass(&self) {
        self.bass_regenerate.fetch_add(1, REL);
    }
```

Match the exact bodies of the existing `publish_step` / `read_step` / `read_pattern` where they differ from the above — they are the model, and the two pairs must behave identically.

- [ ] **Step 6: Add the two `for_bass` constructors**

In `crates/synth_core/src/sequencer.rs`:

```rust
impl GenerativeSettings {
    // ... `from_params` unchanged ...

    /// The bass's generator.
    ///
    /// Key and scale come from the lead: a bass in a different key from the
    /// melody is a bug, not a feature, and splitting them later is a one-line
    /// change if it is ever wanted. Everything that actually distinguishes a
    /// bass line — register, range, how busy it is, how far it leaps — is the
    /// bass's own.
    pub fn for_bass(p: &Params) -> Self {
        Self {
            root: p.gen_root,
            scale: p.gen_scale,
            octave: p.bass_gen_octave,
            range: p.bass_gen_range,
            density: p.bass_gen_density,
            max_jump: p.bass_gen_max_jump,
            chord_bias: p.bass_gen_chord_bias,
            length: p.bass_seq_length,
            slide_chance: p.bass_slide_chance,
            accent_chance: p.bass_accent_chance,
        }
    }
}

impl SeqSettings {
    // ... `from_params` unchanged ...

    /// The bass's sequencer settings.
    ///
    /// Its own length, the lead's groove: two lines that swing differently
    /// against one clock is not an arrangement, it is a mistake.
    pub fn for_bass(p: &Params) -> Self {
        Self {
            length: p.bass_seq_length,
            swing: p.seq_swing,
            gate: p.seq_gate,
        }
    }
}
```

- [ ] **Step 7: Run the tests**

```bash
cargo test -p synth_core --lib
```

Expected: PASS, `golden_vector_is_stable` included — nothing here changes a value the existing render reads.

- [ ] **Step 8: Run the full suite and clippy**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: green; 39 warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/synth_core/src/params.rs crates/synth_core/src/sequencer.rs
git commit -m "$(cat <<'EOF'
Give the bassline its parameters, mirror and telemetry

Off by default, so the render an existing patch produces is unchanged
to the bit. The generator shares the lead's key and owns everything
else.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: The engine — second sequencer, the voice, the bus

The bass becomes audible. A second `Sequencer`, the `BassVoice`, a `comp_bass` insert and a `bass_send` tap, all off the one clock.

**Files:**
- Modify: `crates/synth_core/src/engine.rs`
- Test: `crates/synth_core/src/engine.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: `BassVoice` (Tasks 4–5), `BassParams`, the `Params` / `SharedParams` fields (Task 6), `SeqSettings::for_bass`, `GenerativeSettings::for_bass`, `Compressor::process(&mut [f32], &mut [f32], key: Option<&[f32]>, &CompressorParams)`, `fn sidechain(source: SidechainSource, bus: &DrumBuses, scratch: &mut [f32; BLOCK], count: usize) -> Option<&[f32]>` as `comp_synth` already uses it, `Smoothed::new(initial, time_ms, control_rate)`.
- Produces: `Engine` fields `bass_seq: Sequencer`, `bass: BassVoice`, `comp_bass: Compressor`, `bass_gain: Smoothed`, `bass_block: Vec<f32>`, `last_bass_regenerate: u32`; and `fn publish_bass_pattern(&self)`.

Routing, from the spec, in order: render mono → `bass_gain` → duplicate to `bass_l` / `bass_r` → `comp_bass` (sidechain-capable) → tap `bass_send` into the shared send pair → sum into `left` / `right`. **Not** through `drive`, the soft clipper or the DC blocker: those belong to the synth, and the same argument the code already makes about the kick applies here.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/synth_core/src/engine.rs`:

```rust
    /// Renders `blocks` blocks through the *stereo* path and reports the peak
    /// absolute sample.
    ///
    /// `render_stereo` and `peak` are already in this `mod tests`; the stereo
    /// entry point is the one that matters here, because the bass bus is a
    /// pair and the mono `process` collapses it before anything can be seen.
    fn engine_peak(engine: &mut Engine, blocks: usize) -> f32 {
        let out = render_stereo(engine, blocks * BLOCK);
        assert!(out.iter().all(|s| s.is_finite()), "the bus went non-finite");
        peak(&out)
    }

    #[test]
    fn the_bass_is_silent_until_it_is_enabled() {
        let quiet = Params {
            bass_enabled: false,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&quiet));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        assert!(engine_peak(&mut engine, 400) < 1e-6, "the bass must stay off");

        shared.bass_enabled.set(true);
        assert!(engine_peak(&mut engine, 400) > 0.001, "and sound once switched on");
    }

    #[test]
    fn bass_and_lead_step_from_the_same_clock() {
        let p = Params { bass_enabled: true, seq_playing: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        // Same length on both lines: locked to one clock, they must agree on
        // which step they are on, every block, forever.
        shared.bass_seq_length.set(shared.seq_length.get());
        for _ in 0..2_000 {
            render_stereo(&mut engine, BLOCK);
            assert_eq!(
                shared.bass_step.load(core::sync::atomic::Ordering::Relaxed),
                shared.current_step.load(core::sync::atomic::Ordering::Relaxed),
                "two lines, one clock"
            );
        }
    }

    #[test]
    fn bass_send_feeds_the_return_and_zero_does_not() {
        // Reverb fully wet, everything else muted: whatever comes out is the
        // send.
        let base = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            reverb_mix: 1.0,
            ..Params::default()
        };
        let closed = Params { bass_send: 0.0, ..base };
        let shared = std::sync::Arc::new(SharedParams::from_params(&closed));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        let dry = engine_peak(&mut engine, 800);

        shared.bass_send.set(1.0);
        let wet = engine_peak(&mut engine, 800);
        assert!(wet > dry, "an open send should add a tail: {dry} -> {wet}");
    }

    #[test]
    fn comp_bass_reports_the_reduction_it_applies() {
        let p = Params {
            bass_enabled: true,
            seq_playing: true,
            comp_bass: CompressorParams {
                on: true,
                threshold_db: -50.0,
                ratio: 20.0,
                ..CompressorParams::default()
            },
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        engine_peak(&mut engine, 600);
        assert!(
            shared.comp_bass_gr.get() > 0.5,
            "a -50 dB threshold at 20:1 should show real gain reduction"
        );
    }

    #[test]
    fn the_bass_bus_skips_the_drive_stage() {
        // Drive belongs to the synth. Turning it up must not change the bass,
        // exactly as it does not change the drums.
        let p = Params {
            bass_enabled: true,
            melody_enabled: false,
            drum_enabled: false,
            seq_playing: true,
            drive: 1.0,
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (_tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        let clean = engine_peak(&mut engine, 600);

        shared.drive.set(8.0);
        let driven = engine_peak(&mut engine, 600);
        assert!(
            (clean - driven).abs() < clean * 0.05,
            "drive must not touch the bass: {clean} vs {driven}"
        );
    }
```

`Engine::new(sample_rate, Arc<SharedParams>, Consumer)` is the constructor these tests use; `with_sources` takes a `Vec<Consumer>` and is only for the multi-queue case. There is no `reverb_on` — the reverb is always live and `reverb_mix` alone decides how much of the return is heard.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p synth_core --lib engine
```

Expected: FAIL — `SharedParams` has the fields, but nothing renders the bass, so `the_bass_is_silent_until_it_is_enabled` fails on the second assert.

- [ ] **Step 3: Add the engine's fields**

In `crates/synth_core/src/engine.rs`, on `struct Engine`:

```rust
    /// The bass line. A second sequencer rather than a second channel on the
    /// first: two independent patterns are the point, and they cost one
    /// struct.
    bass_seq: Sequencer,
    bass: BassVoice,
    comp_bass: Compressor,
    bass_gain: Smoothed,
    /// Scratch for the bass's mono render, before it is duplicated to a pair.
    bass_block: Vec<f32>,
    last_bass_regenerate: u32,
```

and in `with_sources`, beside the equivalent lead lines:

```rust
        let mut bass_seq = Sequencer::new(
            params.bass_gen_seed.load(core::sync::atomic::Ordering::Relaxed),
        );
        bass_seq.regenerate(&GenerativeSettings::for_bass(&snapshot));
```

then in the struct literal:

```rust
            bass_seq,
            bass: BassVoice::new(sample_rate, 0xB455_5EED),
            comp_bass: Compressor::new(sample_rate),
            bass_gain: Smoothed::new(snapshot.bass_gain, 15.0, control_rate),
            bass_block: vec![0.0; BLOCK],
            last_bass_regenerate: 0,
```

`Smoothed::new(initial, time_ms, control_rate)`: 15.0 ms, the same time constant `synth_gain` and `master_gain` use on the lines above. `bass_block` is allocated here, in the constructor, and never resized afterwards; that is the same contract `block` already has.

After the constructor's `engine.publish_pattern();`, add:

```rust
        engine.publish_bass_pattern();
```

and add the publisher beside `publish_pattern`:

```rust
    /// Copies the bass pattern into its mirror. Same contract as
    /// [`Engine::publish_pattern`]: only when it changes, never every block.
    fn publish_bass_pattern(&self) {
        for (index, step) in self.bass_seq.pattern().iter().enumerate() {
            self.params.publish_bass_step(index, step);
        }
        self.params.publish_bass_len(self.bass_seq.pattern().len());
    }
```

Add `BassVoice` and `SeqSettings` to the imports at the top of the file.

- [ ] **Step 4: Step the bass sequencer and drive the voice**

In `render_chunk`, immediately after the lead's `advance` / `pattern_changed` / `melody_enabled` block and before the drums:

```rust
        // The same `adv` and `view` the lead just used. One clock advance
        // drives all three sequencers, so nothing can drift from anything
        // else by construction.
        let bass_seq = self
            .bass_seq
            .advance(count, adv, view, &SeqSettings::for_bass(params));
        if self.bass_seq.take_pattern_changed() {
            self.params.bass_seq_length.set(self.bass_seq.pattern().len() as u32);
            self.publish_bass_pattern();
        }
        if params.bass_enabled {
            // Note-off first, then note-on: a tie relies on the voice still
            // being active when the new note arrives, and `BassVoice::note_on`
            // re-gates anyway.
            if bass_seq.note_off.is_some() && !bass_seq.slide {
                self.bass.note_off();
            }
            if let Some((note, _velocity)) = bass_seq.note_on {
                self.bass.note_on(note, bass_seq.accent, bass_seq.slide);
            }
        }
```

The velocity is deliberately discarded: on this instrument, dynamics come from accent, and a velocity-scaled 303 is a different and worse instrument. The `!bass_seq.slide` guard is what makes the tie legato — `Sequencer::trigger` emits the previous note's note-off in the same output as the tie's note-on, and honouring it would release the voice a sample before the glide starts.

- [ ] **Step 5: Render and route the bass bus**

In `render_chunk`, after the synth's `comp_synth` insert and the `send_l` / `send_r` tap, and before the drums are summed:

```rust
        // The bass bus. Not through `drive`, the soft clipper or the DC
        // blocker: those are the synth's, and the same argument the kick makes
        // above applies here — the 303's dirt comes from resonance and
        // envelope depth, not from saturation.
        if params.bass_enabled {
            let bass_gain = self.bass_gain.next();
            let block = &mut self.bass_block[..count];
            self.bass.process_block(block, &params.bass);

            // Mono, duplicated. A bass is centred; there is no pan control and
            // no reason for one.
            let mut bass_l = [0.0f32; BLOCK];
            let mut bass_r = [0.0f32; BLOCK];
            for i in 0..count {
                let value = self.bass_block[i] * bass_gain;
                let value = if value.is_finite() { value } else { 0.0 };
                bass_l[i] = value;
                bass_r[i] = value;
            }

            // Insert, before the send tap, matching `comp_synth`: the return
            // hears the compressed signal, so a bass ducking under the kick
            // ducks in the reverb too.
            let mut detector = [0.0f32; BLOCK];
            let source = params.comp_bass.sidechain;
            let key = sidechain(source, self.drums.buses(), &mut detector, count);
            self.comp_bass.process(
                &mut bass_l[..count],
                &mut bass_r[..count],
                key,
                &params.comp_bass,
            );

            for i in 0..count {
                left[i] += bass_l[i];
                right[i] += bass_r[i];
                send_l[i] += bass_l[i] * params.bass_send;
                send_r[i] += bass_r[i] * params.bass_send;
            }
        } else {
            // Keep the smoother tracking while the bus is muted, so switching
            // the bass on does not start it with a stale ramp.
            self.bass_gain.next();
        }
```

`self.bass_gain`'s target is set alongside the other gains — find where `master_gain`, `synth_gain` and `drum_gain` have `set_target` (or equivalent) called with the snapshot values, and add `self.bass_gain.set_target(params.bass_gain);` there in the same form.

`bass_l` / `bass_r` are `[f32; BLOCK]` on the stack, exactly as `send_l` / `send_r` and `detector` already are; nothing here allocates.

- [ ] **Step 6: Publish the bass telemetry**

In `publish_telemetry`:

```rust
        self.params
            .bass_step
            .store(self.bass_seq.position() as u32, Relaxed);
```

and wherever `comp_synth_gr` and `comp_master_gr` are stored, add:

```rust
        self.params.comp_bass_gr.set(self.comp_bass.gain_reduction_db());
```

using whatever accessor `comp_synth` uses on the line above — copy it exactly.

- [ ] **Step 7: Run the tests**

```bash
cargo test -p synth_core --lib
```

Expected: PASS. `golden_vector_is_stable` must still pass — `bass_enabled` is `false` in the golden render, so the bass branch never runs and the only added work is the `else` arm's smoother tick, which touches nothing the render reads.

- [ ] **Step 8: Run the full suite and clippy**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: green; 39 warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/synth_core/src/engine.rs
git commit -m "$(cat <<'EOF'
Put the bassline on its own bus

A second sequencer off the one clock, a monophonic voice, and a full
third bus with its own compressor and send. It skips the drive stage
for the same reason the drums do.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Events, transport and the `Synth` facade

Editing a bass step, regenerating the bass line, and making the transport reach the new voice.

**Files:**
- Modify: `crates/synth_core/src/event.rs`
- Modify: `crates/synth_core/src/engine.rs`
- Modify: `crates/bevy_synth/src/lib.rs`
- Test: `crates/synth_core/src/engine.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: everything from Tasks 1–7.
- Produces:
  - `Event::SetBassStep { index: u8, step: Step }` and `Event::RegenerateBass`
  - `Synth::bass_pattern(&self) -> synth_core::Pattern`, `Synth::set_bass_step(&self, index: usize, step: synth_core::Step) -> bool`, `Synth::toggle_bass_step(&self, index: usize) -> bool`, `Synth::toggle_bass_slide(&self, index: usize) -> bool`, `Synth::toggle_bass_accent(&self, index: usize) -> bool`, `Synth::regenerate_bass(&self)`, `Synth::regenerate_bass_with_seed(&self, seed: u64)`

Both new variants are the same size as `SetStep`, which is already the largest variant, so the queue slot does not grow. New variants rather than a `channel: u8` field on the existing ones: additive, and no existing call site changes.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/synth_core/src/engine.rs`:

```rust
    #[test]
    fn editing_a_bass_step_leaves_the_lead_alone() {
        let p = Params { bass_enabled: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        render_stereo(&mut engine, BLOCK);

        let lead_before = shared.read_step(2);
        assert!(tx.push(Event::SetBassStep {
            index: 2,
            step: crate::sequencer::Step {
                active: true, note: 31, velocity: 0.7, accent: true, slide: true,
            },
        }));
        render_stereo(&mut engine, BLOCK);

        assert_eq!(shared.read_bass_step(2).note, 31);
        assert!(shared.read_bass_step(2).slide);
        assert_eq!(shared.read_step(2), lead_before, "the lead must be untouched");
    }

    #[test]
    fn regenerating_one_line_leaves_the_other_alone() {
        let p = Params { bass_enabled: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        render_stereo(&mut engine, BLOCK);

        let lead: Vec<u8> = shared.read_pattern().iter().map(|s| s.note).collect();
        let bass: Vec<u8> = shared.read_bass_pattern().iter().map(|s| s.note).collect();

        shared.bass_gen_seed.store(0xFACE_0001, core::sync::atomic::Ordering::Relaxed);
        assert!(tx.push(Event::RegenerateBass));
        render_stereo(&mut engine, BLOCK);

        let lead_after: Vec<u8> = shared.read_pattern().iter().map(|s| s.note).collect();
        let bass_after: Vec<u8> = shared.read_bass_pattern().iter().map(|s| s.note).collect();
        assert_eq!(lead, lead_after, "regenerating the bass must not touch the lead");
        assert_ne!(bass, bass_after, "and must actually write a new bass line");
    }

    #[test]
    fn stopping_the_transport_silences_the_bass() {
        let p = Params { bass_enabled: true, seq_playing: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        assert!(engine_peak(&mut engine, 600) > 0.001, "playing");

        assert!(tx.push(Event::ClockStop));
        render_stereo(&mut engine, BLOCK);
        // Past the amp release of 8 ms.
        assert!(engine_peak(&mut engine, 200) < 1e-6, "stopped means silent");
    }

    #[test]
    fn panic_silences_the_bass() {
        let p = Params { bass_enabled: true, seq_playing: true, ..Params::default() };
        let shared = std::sync::Arc::new(SharedParams::from_params(&p));
        let (tx, rx) = crate::event::channel(64);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);
        engine_peak(&mut engine, 600);
        shared.seq_playing.set(false);
        assert!(tx.push(Event::Panic));
        render_stereo(&mut engine, BLOCK);
        assert!(engine_peak(&mut engine, 100) < 1e-6, "panic cuts everything");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p synth_core --lib engine
```

Expected: FAIL — no variant `SetBassStep`.

- [ ] **Step 3: Add the two events**

In `crates/synth_core/src/event.rs`, in the `Event` enum, immediately after `Regenerate`:

```rust
    /// Overwrites one step of the bass pattern.
    ///
    /// A separate variant rather than a channel field on `SetStep`: additive,
    /// no existing call site changes, and it is the same size as the variant
    /// that already sets the queue's slot size, so the queue does not grow.
    SetBassStep { index: u8, step: Step },
    /// Writes a fresh bass line from the current `bass_gen_*` parameters.
    RegenerateBass,
```

- [ ] **Step 4: Handle them, and reach the bass from the transport**

In `crates/synth_core/src/engine.rs`, in the event loop beside the existing arms:

```rust
                Event::SetBassStep { index, step } => {
                    self.bass_seq.set_step(index as usize, step);
                    self.publish_bass_pattern();
                }
                Event::RegenerateBass => self.regenerate_bass(params),
```

In `Event::ClockStart`, beside `self.sequencer.rewind();`:

```rust
                    self.bass_seq.rewind();
```

In `Event::ClockStop`, beside the lead's release:

```rust
                    if self.bass_seq.release_all().is_some() {
                        self.bass.note_off();
                    }
                    self.bass.silence();
```

In `Event::AllNotesOff` / `Event::Panic`, which both route through `all_notes_off`, add to that method:

```rust
        self.bass_seq.release_all();
        self.bass.silence();
```

In the `should_play` reconcile at the bottom of the event drain, in the branch that stops the clock, add the same two lines the `ClockStop` arm uses.

Add the regenerator beside `regenerate`:

```rust
    fn regenerate_bass(&mut self, params: &Params) {
        self.bass_seq.set_seed(
            self.params.bass_gen_seed.load(core::sync::atomic::Ordering::Relaxed),
        );
        self.bass_seq
            .regenerate(&GenerativeSettings::for_bass(params));
        self.publish_bass_pattern();
    }
```

and the counter, beside the lead's `gen_regenerate` check:

```rust
        let requested = self
            .params
            .bass_regenerate
            .load(core::sync::atomic::Ordering::Relaxed);
        if requested != self.last_bass_regenerate {
            self.last_bass_regenerate = requested;
            self.regenerate_bass(params);
        }
```

- [ ] **Step 5: Expose the bass on the `Synth` facade**

In `crates/bevy_synth/src/lib.rs`, beside `pattern` / `set_step` / `toggle_step` / `regenerate`:

```rust
    /// The bass pattern, as the audio thread last published it.
    ///
    /// Same contract as [`Synth::pattern`]: a copy taken from a lock-free
    /// mirror, so it never blocks and never shows a half-written step.
    pub fn bass_pattern(&self) -> synth_core::Pattern {
        self.params.read_bass_pattern()
    }

    /// Overwrites one step of the bass pattern.
    pub fn set_bass_step(&self, index: usize, step: synth_core::Step) -> bool {
        if index > u8::MAX as usize {
            return false;
        }
        self.send(Event::SetBassStep {
            index: index as u8,
            step,
        })
    }

    /// Turns one bass step on or off, keeping its note, velocity and flags.
    pub fn toggle_bass_step(&self, index: usize) -> bool {
        let mut step = self.params.read_bass_step(index);
        step.active = !step.active;
        self.set_bass_step(index, step)
    }

    /// Turns the tie on one bass step on or off.
    pub fn toggle_bass_slide(&self, index: usize) -> bool {
        let mut step = self.params.read_bass_step(index);
        step.slide = !step.slide;
        self.set_bass_step(index, step)
    }

    /// Turns the accent on one bass step on or off.
    pub fn toggle_bass_accent(&self, index: usize) -> bool {
        let mut step = self.params.read_bass_step(index);
        step.accent = !step.accent;
        self.set_bass_step(index, step)
    }

    /// Writes a new bass line from the current `bass_gen_*` parameters.
    pub fn regenerate_bass(&self) {
        self.params.regenerate_bass();
    }

    /// Sets the bass seed and immediately regenerates, so a given seed always
    /// yields the same line.
    pub fn regenerate_bass_with_seed(&self, seed: u64) {
        self.params
            .bass_gen_seed
            .store(seed, std::sync::atomic::Ordering::Relaxed);
        self.params.regenerate_bass();
    }
```

- [ ] **Step 6: Run the tests**

```bash
cargo test -p synth_core --lib
cargo test --workspace
```

Expected: PASS throughout, `golden_vector_is_stable` included.

- [ ] **Step 7: Clippy**

```bash
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: 39.

- [ ] **Step 8: Commit**

```bash
git add crates/synth_core/src/event.rs crates/synth_core/src/engine.rs crates/bevy_synth/src/lib.rs
git commit -m "$(cat <<'EOF'
Let the bass line be edited, regenerated and stopped

Two new events the same size as the one that already sets the queue's
slot size, so the queue does not grow, and the transport now reaches
the third instrument as well as the other two.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: The BASS tab

A fourth tab holding the eight voice knobs, the bass step grid with per-step slide and accent, and the generative controls.

**Files:**
- Create: `crates/bevy_synth_ui/src/sections/bass.rs`
- Modify: `crates/bevy_synth_ui/src/sections/mod.rs`
- Modify: `crates/bevy_synth_ui/src/widgets.rs`
- Modify: `crates/bevy_synth_ui/src/lib.rs`
- Modify: `crates/bevy_synth/src/lib.rs` (`SynthTelemetry::bass_step`)

**Interfaces:**
- Consumes: `Synth::{bass_pattern, toggle_bass_step, toggle_bass_slide, toggle_bass_accent, regenerate_bass_with_seed}` (Task 8); `SharedParams`'s bass fields (Task 6); `SynthTelemetry`, which this task extends with `pub bass_step: u32` for the step readout (Task 10 adds `comp_bass_gr`, which this tab does not need); the existing `widgets::{section, knob_param, KnobSpec, palette}`, `crate::{dropdown, integer}`.
- Produces:
  - `pub(crate) fn bass(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, state: &mut SynthUi)`
  - `pub fn bass_step_grid(ui: &mut Ui, steps: &[synth_core::Step], current: usize, playing: bool) -> Option<(usize, BassEdit)>` with `pub enum BassEdit { Toggle, Slide, Accent }`
  - `Tab::Bass` with `Tab::ALL: [Self; 4]`

The grid needs three affordances per cell rather than one, so it gets its own widget beside `step_grid` rather than growing the lead's: a left-click toggles the step, and two small strips along the bottom edge toggle slide and accent. Sharing one widget would mean every lead cell carrying two dead zones.

- [ ] **Step 1: Add `Tab::Bass`**

In `crates/bevy_synth_ui/src/lib.rs`:

```rust
pub enum Tab {
    /// Oscillators through effects: the sound itself.
    #[default]
    Synth,
    /// The step sequencer and its pattern slots.
    Sequencer,
    /// The bassline: its voice, its own line, its own generator.
    Bass,
    /// The drum rack.
    Drums,
}

impl Tab {
    /// Left to right, in the order the signal is usually built up.
    pub const ALL: [Self; 4] = [Self::Synth, Self::Sequencer, Self::Bass, Self::Drums];

    /// The label on the tab.
    pub fn name(self) -> &'static str {
        match self {
            Self::Synth => "SYNTH",
            Self::Sequencer => "SEQ",
            Self::Bass => "BASS",
            Self::Drums => "DRUMS",
        }
    }
}
```

and in the `match ui_state.tab` block:

```rust
                    Tab::Bass => sections::bass(ui, &synth, &telemetry, &mut ui_state),
```

- [ ] **Step 2: Add the grid widget**

First give the bass its own colour. In `crates/bevy_synth_ui/src/widgets.rs`, inside `pub mod palette`, after `DRUM`:

```rust
    /// The bassline: its tab, its knobs and its mixer strip.
    pub const BASS: Color32 = Color32::from_rgb(226, 116, 196);
```

Magenta is the one hue the panel does not already spend. Do not reuse `OSC` — the synth bus already owns that blue in the mixer, and a bass strip painted the same colour reads as a second synth strip.

Then, in the same file, after `step_grid`:

```rust
/// What a click on the bass grid asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BassEdit {
    /// The body of the cell: turn the step on or off.
    Toggle,
    /// The left strip along the bottom: tie this step to the one before it.
    Slide,
    /// The right strip: accent it.
    Accent,
}

/// The bass pattern as a grid of steps.
///
/// A cell here carries three decisions rather than one — is there a note, is
/// it tied, is it accented — so it gets its own widget rather than growing
/// [`step_grid`] with two dead zones on every lead cell. The body toggles the
/// note; the two strips along the bottom edge toggle the tie and the accent.
pub fn bass_step_grid(
    ui: &mut Ui,
    steps: &[synth_core::sequencer::Step],
    current: usize,
    playing: bool,
) -> Option<(usize, BassEdit)> {
    let mut clicked = None;
    const PER_ROW: usize = 16;

    ui.vertical(|ui| {
        for (row_index, row_steps) in steps.chunks(PER_ROW).enumerate() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for (column, step) in row_steps.iter().enumerate() {
                    let index = row_index * PER_ROW + column;
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::new(30.0, 42.0), Sense::click());

                    // The bottom eight pixels are the two strips.
                    let strip_top = rect.bottom() - 8.0;
                    let slide_rect = Rect::from_min_max(
                        Pos2::new(rect.left(), strip_top),
                        Pos2::new(rect.center().x - 1.0, rect.bottom()),
                    );
                    let accent_rect = Rect::from_min_max(
                        Pos2::new(rect.center().x + 1.0, strip_top),
                        Pos2::new(rect.right(), rect.bottom()),
                    );

                    if response.clicked() {
                        if let Some(pos) = response.interact_pointer_pos() {
                            clicked = Some(if slide_rect.contains(pos) {
                                (index, BassEdit::Slide)
                            } else if accent_rect.contains(pos) {
                                (index, BassEdit::Accent)
                            } else {
                                (index, BassEdit::Toggle)
                            });
                        }
                    }

                    let is_current = playing && index == current;
                    let painter = ui.painter_at(rect);

                    let background = if is_current {
                        palette::SEQ
                    } else if step.active {
                        palette::BASS.gamma_multiply(0.45)
                    } else if index % 4 == 0 {
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
                            rect.center() - Vec2::new(0.0, 7.0),
                            Align2::CENTER_CENTER,
                            format!("{name}{octave}"),
                            FontId::monospace(9.5),
                            text_colour,
                        );
                    }

                    // The two strips are always drawn, lit when set: an
                    // unlit strip is what tells you the affordance is there.
                    let dim = palette::TRACK;
                    painter.rect_filled(
                        slide_rect,
                        1.0,
                        if step.slide { palette::ACCENT } else { dim },
                    );
                    painter.rect_filled(
                        accent_rect,
                        1.0,
                        if step.accent { palette::SEQ } else { dim },
                    );
                }
            });
        }
    });

    clicked
}
```

If `palette` lacks `ACCENT`, `OSC`, `SEQ`, `TRACK`, `PANEL` or `TEXT` under those exact names, use whatever `step_grid` above it uses — it is the reference for every colour here.

- [ ] **Step 3: Write the section**

Create `crates/bevy_synth_ui/src/sections/bass.rs`:

```rust
//! The bassline: its voice, its own line, and its own generator.

use egui::Ui;

use bevy_synth::{Synth, SynthTelemetry};
use synth_core::Waveform;

use crate::widgets::{self, BassEdit, KnobSpec, palette};
use crate::SynthUi;
use crate::sections::sequencer::rand_seed;

pub(crate) fn bass(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, _state: &mut SynthUi) {
    let p = &synth.params;

    widgets::section(ui, "BASS", palette::BASS, |ui| {
        let mut enabled = p.bass_enabled.get();
        if ui
            .checkbox(&mut enabled, "Play bassline")
            .on_hover_text("the bass is off until you ask for it, so adding it never changes an existing patch")
            .changed()
        {
            p.bass_enabled.set(enabled);
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Wave");
            // Two of them, not five: a 303 is a saw or a square and nothing
            // else, and the other three would only be there to be wrong.
            let current = Waveform::from_u32(p.bass.wave.get());
            for (wave, name) in [(Waveform::Saw, "Saw"), (Waveform::Pulse, "Pulse")] {
                if ui.selectable_label(current == wave, name).clicked() {
                    p.bass.wave.set(wave as u32);
                }
            }
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Tune", -12.0..=12.0)
                    .colour(palette::BASS)
                    .default(0.0)
                    .size(36.0),
                &p.bass.tune,
            )
            .on_hover_text("transpose, in semitones");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Cutoff", 20.0..=20000.0)
                    .log()
                    .colour(palette::BASS)
                    .default(300.0)
                    .size(36.0),
                &p.bass.cutoff,
            )
            .on_hover_text("where the filter sits before the envelope opens it");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Reso", 0.0..=1.0)
                    .colour(palette::BASS)
                    .default(0.7)
                    .size(36.0),
                &p.bass.resonance,
            )
            .on_hover_text("high is the point of this instrument");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Env Mod", 0.0..=6.0)
                    .colour(palette::BASS)
                    .default(3.0)
                    .size(36.0),
                &p.bass.env_mod,
            )
            .on_hover_text("how far the envelope opens the filter, in octaves");
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Decay", 0.02..=2.0)
                    .log()
                    .colour(palette::BASS)
                    .default(0.3)
                    .size(36.0),
                &p.bass.decay,
            )
            .on_hover_text("how long the filter sweep takes; the single most important knob");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Accent", 0.0..=1.0)
                    .colour(palette::ACCENT)
                    .default(0.5)
                    .size(36.0),
                &p.bass.accent,
            )
            .on_hover_text("how much an accented step adds: level, brightness and envelope depth together");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Slide", 0.01..=0.5)
                    .log()
                    .colour(palette::ACCENT)
                    .default(0.06)
                    .size(36.0),
                &p.bass.slide_time,
            )
            .on_hover_text("how long a tied note takes to reach its new pitch");
        });

        ui.add_space(4.0);
        ui.separator();

        let pattern = synth.bass_pattern();
        if let Some((index, edit)) = widgets::bass_step_grid(
            ui,
            &pattern,
            telemetry.bass_step as usize,
            p.seq_playing.get(),
        ) {
            match edit {
                BassEdit::Toggle => synth.toggle_bass_step(index),
                BassEdit::Slide => synth.toggle_bass_slide(index),
                BassEdit::Accent => synth.toggle_bass_accent(index),
            };
        }
        ui.label(
            egui::RichText::new("click a step to toggle it; the left tab under it ties, the right accents")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        ui.add_space(4.0);
        ui.separator();
        ui.label(
            egui::RichText::new("GENERATOR")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );
        ui.label(
            egui::RichText::new("key and scale come from the sequencer page")
                .color(palette::TEXT_DIM)
                .size(9.0),
        );

        ui.horizontal(|ui| {
            ui.label("Octave");
            crate::integer(ui, &p.bass_gen_octave, 0..=7, "");
            ui.label("Range");
            crate::integer(ui, &p.bass_gen_range, 1..=4, " oct");
            ui.label("Length");
            crate::integer(ui, &p.bass_seq_length, 1..=64, " steps");
        });

        ui.horizontal(|ui| {
            widgets::knob_param(
                ui,
                &KnobSpec::new("Density", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.85)
                    .size(36.0),
                &p.bass_gen_density,
            )
            .on_hover_text("how often a step has a note rather than a rest");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Jump", 1.0..=12.0)
                    .colour(palette::SEQ)
                    .default(5.0)
                    .size(36.0),
                &p.bass_gen_max_jump,
            )
            .on_hover_text("how far the line may leap, in scale degrees");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Chord", 0.0..=1.0)
                    .colour(palette::SEQ)
                    .default(0.8)
                    .size(36.0),
                &p.bass_gen_chord_bias,
            )
            .on_hover_text("how strongly downbeats land on root, third and fifth");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Slides", 0.0..=1.0)
                    .colour(palette::ACCENT)
                    .default(0.25)
                    .size(36.0),
                &p.bass_slide_chance,
            )
            .on_hover_text("how often the generator ties a step to the one before it");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Accents", 0.0..=1.0)
                    .colour(palette::ACCENT)
                    .default(0.3)
                    .size(36.0),
                &p.bass_accent_chance,
            )
            .on_hover_text("how often it accents a step off the downbeat");

            ui.vertical(|ui| {
                ui.add_space(8.0);
                if ui
                    .button("↻  New bassline")
                    .on_hover_text("write a fresh bass line from these settings")
                    .clicked()
                {
                    synth.regenerate_bass_with_seed(rand_seed());
                }
                ui.label(
                    egui::RichText::new(format!(
                        "seed {:#x}",
                        p.bass_gen_seed.load(std::sync::atomic::Ordering::Relaxed)
                    ))
                    .color(palette::TEXT_DIM)
                    .size(9.0),
                );
            });
        });
    });
}
```

`rand_seed` is today a private `fn` at the bottom of `crates/bevy_synth_ui/src/sections/sequencer.rs`. Widen it to `pub(crate) fn rand_seed()` there — no other change — and import it as `use crate::sections::sequencer::rand_seed;`. Do not write a second copy.

- [ ] **Step 4: Register the section**

In `crates/bevy_synth_ui/src/sections/mod.rs`:

```rust
mod bass;
```

in the alphabetical `mod` list, and:

```rust
pub(crate) use bass::bass;
```

in the alphabetical re-export list.

- [ ] **Step 5: Build and check**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: builds; tests green; 39 warnings. The step readout needs one new telemetry field. `SynthTelemetry` lives in `crates/bevy_synth/src/lib.rs`; add, beside the lead's `current_step`:

```rust
    /// Bass sequencer step currently playing.
    pub bass_step: u32,
```

on `SynthTelemetry`, with `telemetry.bass_step = synth.params.bass_step.load(Relaxed);` in `read_telemetry`.

- [ ] **Step 6: Look at it**

```bash
cargo run
```

Open the panel, click BASS, tick "Play bassline". Check: the tab appears fourth; the eight knobs are all present and move; the grid shows notes with two strips under each; clicking a body toggles the note, clicking the left strip lights it and the note ties, clicking the right strip accents it; "New bassline" writes a different line and the seed label changes.

- [ ] **Step 7: Commit**

```bash
git add crates/bevy_synth_ui/src/sections/bass.rs crates/bevy_synth_ui/src/sections/mod.rs crates/bevy_synth_ui/src/widgets.rs crates/bevy_synth_ui/src/lib.rs crates/bevy_synth/src/lib.rs
git commit -m "$(cat <<'EOF'
Put the bassline on the panel

A fourth tab: the eight voice knobs, a grid whose cells carry a note,
a tie and an accent, and a generator that shares the sequencer page's
key and owns everything else.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: The fourth mixer strip

The bass bus on the mixer, symmetric with the two already there.

**Files:**
- Modify: `crates/bevy_synth_ui/src/sections/mixer.rs`
- Modify: `crates/bevy_synth_ui/src/sections/compressor.rs`
- Modify: `crates/bevy_synth/src/lib.rs`

**Interfaces:**
- Consumes: `SharedParams::{bass_gain, bass_send, comp_bass, comp_bass_gr}` (Task 6), the existing `fn strip(ui: &mut Ui, name: &str, knobs: impl FnOnce(&mut Ui))` in `mixer.rs:121`, and `sections/compressor.rs`'s `fn one(ui, name, colour, shared, gr)`.
- Produces: `SynthTelemetry::comp_bass_gr: f32`.

- [ ] **Step 1: Add the telemetry field**

In `crates/bevy_synth/src/lib.rs`, on `SynthTelemetry`:

```rust
    /// Gain reduction the bass-bus compressor is applying, in positive dB.
    pub comp_bass_gr: f32,
```

and in `read_telemetry`, beside the other two:

```rust
    telemetry.comp_bass_gr = synth.params.comp_bass_gr.get();
```

`bass_step` is already there — Task 9 added it for the BASS tab's step readout. This task adds `comp_bass_gr` and nothing else to `SynthTelemetry`.

- [ ] **Step 2: Add the strip**

In `crates/bevy_synth_ui/src/sections/mixer.rs`, first update the doc comment — it currently says "three strips and a meter":

```rust
/// The output stage, as four strips and a meter.
```

Then, after the `strip(ui, "synth", ...)` block and before the drums':

```rust
            ui.separator();
            strip(ui, "bass", |ui| {
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Level", 0.0..=1.5)
                        .colour(palette::BASS)
                        .default(1.0)
                        .size(36.0),
                    &p.bass_gain,
                )
                .on_hover_text("level of the bass bus alone, under the master");
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Send", 0.0..=1.0)
                        .colour(palette::BASS)
                        .default(0.0)
                        .size(36.0),
                    &p.bass_send,
                )
                .on_hover_text("how much of the bass reaches the effects return");
            });
```

Match the `Send` knob's `KnobSpec` to the synth strip's — same colour treatment, same size — so the two read as the same control on different buses.

- [ ] **Step 3: Add the compressor**

In `crates/bevy_synth_ui/src/sections/compressor.rs`, beside the two existing calls:

```rust
        one(ui, "bass bus", palette::BASS, &p.comp_bass, telemetry.comp_bass_gr);
```

Place it between the synth-bus and master entries, so the three read in signal order.

- [ ] **Step 4: Build and check**

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
```

Expected: builds; tests green; 39 warnings.

- [ ] **Step 5: Look at it**

```bash
cargo run
```

Check: the mixer shows four strips; the bass Level knob changes the bass and nothing else; Send at 0.0 keeps the bass out of the reverb tail and at 1.0 puts it in; the COMPRESSOR section shows three entries and the bass one's GR readout moves when its threshold is low and the bass is playing, falling back to `--` when the bass stops.

- [ ] **Step 6: Commit**

```bash
git add crates/bevy_synth_ui/src/sections/mixer.rs crates/bevy_synth_ui/src/sections/compressor.rs crates/bevy_synth/src/lib.rs
git commit -m "$(cat <<'EOF'
Give the bass bus its mixer strip

Level, send and a compressor, symmetric with the two buses already
there, so the one comparison these knobs exist to make stays in one
place.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Sidechain, and the end-to-end check

The one behaviour the spec names that nothing above tests directly, plus a pass over the whole feature.

**Files:**
- Test: `crates/synth_core/src/engine.rs` (inline `mod tests`)

**Interfaces:**
- Consumes: everything. Produces: nothing new.

- [ ] **Step 1: Write the sidechain test**

```rust
    #[test]
    fn comp_bass_ducks_on_the_kick_and_not_on_other_pads() {
        // The tap the drum rack already publishes, reused: bass ducking under
        // the kick falls out with no new machinery, and this is the test that
        // says so.
        let ducking = Params {
            bass_enabled: true,
            drum_enabled: true,
            seq_playing: true,
            comp_bass: CompressorParams {
                on: true,
                threshold_db: -30.0,
                ratio: 10.0,
                sidechain: SidechainSource::Kick,
                ..CompressorParams::default()
            },
            ..Params::default()
        };
        let shared = std::sync::Arc::new(SharedParams::from_params(&ducking));
        let (tx, rx) = channel(256);
        let mut engine = Engine::new(48_000.0, shared.clone(), rx);

        // Pad 0 is the kick: `rack.rs` writes `out.kick` from pad 0, and
        // `sidechain(SidechainSource::Kick, ..)` copies that tap. `Cell` is
        // already imported into this `mod tests` as `crate::drums::Cell`.
        for step in (0..16u8).step_by(4) {
            assert!(tx.push(Event::SetDrumCell {
                step,
                pad: 0,
                cell: Cell { active: true, velocity: 1.0 },
            }));
        }
        engine_peak(&mut engine, 1_200);
        let with_kick = shared.comp_bass_gr.get();

        // Now the same pattern on a pad that is not the kick. Pad 2 is a hat
        // in the default rack; any pad but 0 proves the point.
        for step in (0..16u8).step_by(4) {
            assert!(tx.push(Event::SetDrumCell {
                step,
                pad: 0,
                cell: Cell::default(),
            }));
            assert!(tx.push(Event::SetDrumCell {
                step,
                pad: 2,
                cell: Cell { active: true, velocity: 1.0 },
            }));
        }
        engine_peak(&mut engine, 1_200);
        let without_kick = shared.comp_bass_gr.get();

        assert!(
            with_kick > without_kick + 0.5,
            "the kick should duck the bass and another pad should not:              {with_kick} vs {without_kick}"
        );
    }
```

`Cell::default()` is an inactive cell (`active: false`), so the second loop clears the kick as it programs the hat. The rack has to be enabled — `drum_enabled: true` — or there is no kick tap to duck against and both readings are zero.

- [ ] **Step 2: Run it**

```bash
cargo test -p synth_core --lib comp_bass_ducks
```

Expected: PASS. If it fails, the fault is in Task 7's `sidechain(...)` call — check it passes `params.comp_bass.sidechain` and not `params.comp_synth.sidechain`.

- [ ] **Step 3: Prove the tests can fail**

The bar for this feature is the one the previous wave used: apply a mutation, watch exactly one test fail, revert, watch it pass. Do this for four:

```bash
# 1. Break the tie: make `note_on` always retrigger.
#    In bass.rs, delete `if slide && self.is_active() { return; }`.
#    Expect: `a_tie_glides_and_does_not_retrigger` fails, alone.

# 2. Break the accent latch: read `p.accent` per sample instead of `self.accented`.
#    Expect: `accent_is_latched_at_note_on` fails, alone.

# 3. Break the conditional draw: remove the `gen.accent_chance > 0.0 &&` guard.
#    Expect: `golden_vector_is_stable` fails. Revert — do not touch the constant.

# 4. Break the routing: move the `bass_send` tap above `comp_bass.process`.
#    Expect: `bass_send_feeds_the_return_and_zero_does_not` fails, alone — it is the test
#    that pins the tap below the insert.
```

Revert each mutation before applying the next.

- [ ] **Step 4: Full sweep**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"
cargo build --release
```

Expected: green; 39 warnings; release builds.

- [ ] **Step 5: Play it**

```bash
cargo run
```

A full pass, with the bass on: pick a preset, tick "Play bassline", set Decay short and Reso high, raise Env Mod, and check that the line squelches; tie two steps and hear one note glide into the next rather than two notes; accent a step and hear it jump out; put the bass compressor on `Kick` with a low threshold and hear it duck; open the bass Send and hear it in the tail.

- [ ] **Step 6: Commit**

```bash
git add crates/synth_core/src/engine.rs
git commit -m "$(cat <<'EOF'
Test the bass compressor's kick sidechain

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Spec coverage

| Spec requirement | Task |
|---|---|
| `BassVoice` struct, fixed 24 dB lowpass, decay-only filter env | 4 |
| Cutoff as octaves off the envelope, bounded 20–20000 | 4 |
| Amp envelope 0.003 / 0.0 / 1.0 / 0.008 | 4 |
| Accent: level ×`1+a*0.5`, cutoff `+a*1.5` octaves, depth ×`1+a` | 5 |
| Accent latched at note-on, not read from velocity | 5 |
| Slide suppresses both retriggers; falls back on step 0 and after a rest | 5 |
| `BassParams`, eight fields, no `level`; `SharedBass` new/snapshot/apply | 3 |
| `Step { slide }`, still eight bytes; mirror bit 29 | 1 |
| `GenerativeSettings { slide_chance, accent_chance }`, both draws conditional | 2 |
| Bass shares `gen_root` / `gen_scale`, owns the rest | 6 |
| `Event::SetBassStep`, `Event::RegenerateBass` | 8 |
| `bass_pattern` / `bass_pattern_len` / `bass_step` mirror and methods | 6 |
| `ClockStop`, `AllNotesOff`, `Panic` reach the bass | 8 |
| Bass bus: no drive, compressor before the send tap, mono duplicated | 7 |
| `comp_bass` reusing `Compressor`; `SidechainSource::Kick` | 7, 11 |
| `comp_bass_gr` telemetry | 6, 7, 10 |
| `bass_enabled` defaults false; golden render unchanged | 6, 7 |
| Fourth BASS tab: eight knobs, grid with slide/accent, generator | 9 |
| Fourth mixer strip; doc comment updated | 10 |

Two places where this plan goes beyond the spec, both deliberate and both stated where they occur:

1. **The tie holds its gate** (Task 2). The spec defines slide at the voice but not at the sequencer; without a one-step lookahead the previous note is already released when the tie lands and there is nothing to glide from. Only fires when `slide` is set, which the lead never sets.
2. **`SeqSettings`** (Task 1). The spec asks for a second `Sequencer` without noting that `Sequencer` reads `seq_length`, `seq_swing` and `seq_gate` straight out of the global `Params`; a second instance would silently inherit the lead's. The bass owns `length` and shares `swing` and `gate`, matching the spec's division between generator settings (its own) and groove (shared).
