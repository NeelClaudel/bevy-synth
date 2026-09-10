# Instrument FX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn delay and reverb into a send/return bus fed by per-instrument sends, make the drum rack stereo with per-pad pan, and add two sidechain-capable compressors.

**Architecture:** `FxChain` stops being an insert on the synth bus and becomes a return: the engine builds a send buffer, runs the chain over it, and adds back `chain(send) - send` so the return contributes only what the effects added. The drum rack renders four buffers instead of one (`dry_l`, `dry_r`, `send_l`, `send_r`) plus a mono `kick` tap for the sidechain detector. A new `Compressor` is instantiated twice, as a synth-bus insert before the send tap and as master glue before the master gain.

**Tech Stack:** Rust workspace; `synth_core` (no_std-shaped real-time DSP, no allocation below `Engine::process`), `bevy_synth` (Bevy 0.19 plugin), `bevy_synth_ui` (bevy_egui 0.42 / egui 0.36.1). Tests are inline `#[cfg(test)]` modules, run with `cargo test`.

**Spec:** `docs/superpowers/specs/2026-09-10-instrument-fx-design.md`

## Global Constraints

- **The golden vector is never re-baselined.** `the_dry_path_matches_the_golden_vector` in `crates/synth_core/src/engine.rs` must pass unedited at every commit. If it fails, the change is wrong -- do not paste the replacement constant the failure prints.
- **Never run `cargo fmt --all`.** This repo has never been rustfmt-clean; formatting it would bury the diff.
- **Clippy baseline is 28 warnings, counted with `cargo clippy --workspace --all-targets`.** Plain `--workspace` shows only 4 of them. Count with `grep -E "^warning: .* generated"`, ignoring `(N duplicates)` lines. Never pipe clippy through `tail`.
- **Real-time safety:** no allocation, locks, `panic!` or syscalls anywhere reachable from `Engine::process`. The only `Vec`s under `crates/synth_core/src/fx/` outside `line.rs:42` are inside `#[cfg(test)]`, and it stays that way. Scratch buffers in the engine are stack arrays `[f32; BLOCK]`.
- **Every commit compiles and every test passes.** `drum_to_fx` is removed in Task 4 and not before, because params, engine and UI all name it.
- Commit messages end with `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`.

### Deviations from the spec, deliberate

The spec lists the compressor parameters flat (`comp_synth_threshold`, `comp_master_threshold`, ...). This plan groups the seven per-compressor parameters into a `CompressorParams` snapshot struct and a `SharedCompressor` atomic struct, used twice. Same atomic types, same defaults, same clamps -- the only change is that the two instances are provably identical instead of two hand-copied blocks of seven fields. Nothing else in the spec moves.

The spec describes the sidechain detector as "the mono sum of the dry drum bus" or "pad 0 alone", without saying whether the drum bus gain applies. This plan scales both taps by `drum_gain`. The spec already justifies taking the kick tap post-level "because a quieter kick should duck less", and `drum_gain` is a level; leaving it out would mean pulling the drum fader down silenced the kit but kept ducking the synth just as hard.

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/synth_core/src/params.rs` | `SidechainSource`, `CompressorParams`, `SharedCompressor`; `pad_pan`, `pad_send`, `drum_send`, `synth_send`; removal of `drum_to_fx` |
| `crates/synth_core/src/fx/compressor.rs` | **new.** The compressor DSP alone. No routing, no parameter plumbing. |
| `crates/synth_core/src/fx/mod.rs` | declares and re-exports `compressor` |
| `crates/synth_core/src/drums/rack.rs` | stereo dry and send pairs, kick tap, pan law |
| `crates/synth_core/src/engine.rs` | bus wiring: send tap, return subtract, two compressor inserts, GR publication |
| `crates/bevy_synth/src/lib.rs` | `SynthTelemetry` gains two gain-reduction fields |
| `crates/bevy_synth_ui/src/sections/compressor.rs` | **new.** Both compressor strips. |
| `crates/bevy_synth_ui/src/sections/drums.rs` | Pan and Send per pad; the `Through FX` toggle goes |
| `crates/bevy_synth_ui/src/sections/mixer.rs` | Send knob on the synth and drum strips |
| `crates/bevy_synth_ui/src/lib.rs` | `ui_enum!(SidechainSource, ..)`, compressor section in the FX column |

`crates/synth_core/src/fx/delay.rs` and `fx/reverb.rs` are not touched.

---

### Task 1: Parameters

Adds every new parameter and the two new types. Nothing consumes them yet, so this task changes no audio: it exists so Tasks 2-6 have something to compile against. `drum_to_fx` stays until Task 4.

**Files:**
- Modify: `crates/synth_core/src/params.rs`
- Test: `crates/synth_core/src/params.rs` (the `#[cfg(test)] mod tests` at the bottom, line ~1125)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum SidechainSource { Off = 0, DrumBus = 1, Kick = 2 }` with `ALL: [SidechainSource; 3]`, `from_u32(u32) -> Self`, `name(self) -> &'static str`
  - `pub struct CompressorParams { on: bool, threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32, makeup_db: f32, sidechain: SidechainSource }`, `Copy`, `Default`
  - `pub struct SharedCompressor { on: AtomicBool32, threshold_db: AtomicF32, ratio: AtomicF32, attack_ms: AtomicF32, release_ms: AtomicF32, makeup_db: AtomicF32, sidechain: AtomicEnum }` with `new(&CompressorParams)`, `snapshot() -> CompressorParams`, `apply(&CompressorParams)`
  - `Params::comp_synth`, `Params::comp_master`: `CompressorParams`
  - `SharedParams::comp_synth`, `SharedParams::comp_master`: `SharedCompressor`
  - `Params::pad_pan`, `Params::pad_send`: `[f32; PAD_COUNT]`; `SharedParams` equivalents `[AtomicF32; PAD_COUNT]`
  - `Params::drum_send`, `Params::synth_send`: `f32`; `SharedParams` equivalents `AtomicF32`

- [ ] **Step 1: Write the failing bus-parameter test**

Append to `mod tests` in `crates/synth_core/src/params.rs`:

```rust
    #[test]
    fn the_new_bus_params_default_to_todays_routing() {
        let p = Params::default();
        // Today's engine sends the synth through the effects and keeps drums
        // out of them. These two defaults are what reproduce that.
        assert_eq!(p.synth_send, 1.0);
        assert_eq!(p.drum_send, 0.0);
        assert_eq!(p.pad_pan, [0.0; PAD_COUNT]);
        assert_eq!(p.pad_send, [1.0; PAD_COUNT]);
    }

    #[test]
    fn bus_params_round_trip_and_clamp() {
        let mut p = Params::default();
        p.synth_send = 0.25;
        p.drum_send = 0.5;
        p.pad_pan[2] = -1.0;
        p.pad_send[3] = 0.125;

        let shared = SharedParams::default();
        shared.apply(&p);
        let back = shared.snapshot();

        assert_eq!(back.synth_send, 0.25);
        assert_eq!(back.drum_send, 0.5);
        assert_eq!(back.pad_pan[2], -1.0);
        assert_eq!(back.pad_send[3], 0.125);

        // Hostile values from the control side are clamped, not trusted.
        shared.synth_send.set(9.0);
        shared.drum_send.set(f32::NAN);
        shared.pad_pan[0].set(-4.0);
        shared.pad_send[0].set(2.0);
        let back = shared.snapshot();
        assert_eq!(back.synth_send, 1.0);
        assert_eq!(back.drum_send, 0.0);
        assert_eq!(back.pad_pan[0], -1.0);
        assert_eq!(back.pad_send[0], 1.0);
    }
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test -p synth_core bus_params`
Expected: FAIL to compile, `no field 'synth_send' on type 'Params'`.

- [ ] **Step 3: Add the four bus parameters**

In `crates/synth_core/src/params.rs`, add to the drum block of `Params` (near `drum_level`, line ~381):

```rust
    /// How much of the drum send pair reaches the return bus.
    pub drum_send: f32,
    /// Per-pad position in the stereo field, -1.0 hard left to 1.0 hard right.
    pub pad_pan: [f32; PAD_COUNT],
    /// Per-pad contribution to the drum send pair, before `drum_send`.
    pub pad_send: [f32; PAD_COUNT],
```

and next to `synth_gain`:

```rust
    /// How much of the compressed synth bus reaches the return bus.
    pub synth_send: f32,
```

In `impl Default for Params` (line ~470):

```rust
            synth_send: 1.0,
            drum_send: 0.0,
            pad_pan: [0.0; PAD_COUNT],
            pad_send: [1.0; PAD_COUNT],
```

In `SharedParams` (line ~595), mirroring the existing pad arrays:

```rust
    pub synth_send: AtomicF32,
    pub drum_send: AtomicF32,
    pub pad_pan: [AtomicF32; PAD_COUNT],
    pub pad_send: [AtomicF32; PAD_COUNT],
```

In `SharedParams::from_params` (line ~780):

```rust
            synth_send: AtomicF32::new(p.synth_send),
            drum_send: AtomicF32::new(p.drum_send),
            pad_pan: core::array::from_fn(|i| AtomicF32::new(p.pad_pan[i])),
            pad_send: core::array::from_fn(|i| AtomicF32::new(p.pad_send[i])),
```

In `snapshot` (line ~900):

```rust
            synth_send: clamp01(self.synth_send.get()),
            drum_send: clamp01(self.drum_send.get()),
            pad_pan: core::array::from_fn(|i| sane(self.pad_pan[i].get(), 0.0).clamp(-1.0, 1.0)),
            pad_send: core::array::from_fn(|i| clamp01(self.pad_send[i].get())),
```

In `apply` (line ~1007), the two scalars above the existing `for i in 0..PAD_COUNT` loop and the two arrays inside it:

```rust
        self.synth_send.set(p.synth_send);
        self.drum_send.set(p.drum_send);
        // ...and inside the existing pad loop:
            self.pad_pan[i].set(p.pad_pan[i]);
            self.pad_send[i].set(p.pad_send[i]);
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p synth_core`
Expected: PASS, whole crate green, `the_dry_path_matches_the_golden_vector` included.

- [ ] **Step 5: Write the failing compressor-parameter test**

```rust
    #[test]
    fn compressor_params_default_to_bypassed() {
        let p = Params::default();
        for c in [p.comp_synth, p.comp_master] {
            assert!(!c.on);
            assert_eq!(c.threshold_db, -12.0);
            assert_eq!(c.ratio, 4.0);
            assert_eq!(c.attack_ms, 10.0);
            assert_eq!(c.release_ms, 100.0);
            assert_eq!(c.makeup_db, 0.0);
            assert_eq!(c.sidechain, SidechainSource::Off);
        }
    }

    #[test]
    fn compressor_params_round_trip_and_clamp() {
        let mut p = Params::default();
        p.comp_synth.on = true;
        p.comp_synth.threshold_db = -24.0;
        p.comp_synth.sidechain = SidechainSource::Kick;
        p.comp_master.ratio = 2.0;

        let shared = SharedParams::default();
        shared.apply(&p);
        let back = shared.snapshot();

        assert!(back.comp_synth.on);
        assert_eq!(back.comp_synth.threshold_db, -24.0);
        assert_eq!(back.comp_synth.sidechain, SidechainSource::Kick);
        assert_eq!(back.comp_master.ratio, 2.0);
        // The master is untouched by the synth compressor's settings.
        assert!(!back.comp_master.on);

        shared.comp_synth.threshold_db.set(12.0);
        shared.comp_synth.ratio.set(f32::NAN);
        shared.comp_master.attack_ms.set(0.0);
        shared.comp_master.release_ms.set(9999.0);
        let back = shared.snapshot();
        assert_eq!(back.comp_synth.threshold_db, 0.0);
        assert_eq!(back.comp_synth.ratio, 4.0);
        assert_eq!(back.comp_master.attack_ms, 0.1);
        assert_eq!(back.comp_master.release_ms, 1000.0);
    }
```

- [ ] **Step 6: Run the test and watch it fail**

Run: `cargo test -p synth_core compressor_params`
Expected: FAIL to compile, `cannot find type 'SidechainSource' in this scope`.

- [ ] **Step 7: Add the compressor types**

In `crates/synth_core/src/params.rs`, after `ClockSource` (line ~168):

```rust
/// Where a compressor's detector listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum SidechainSource {
    /// Detect on the signal being compressed.
    #[default]
    Off = 0,
    /// Detect on the mono sum of the dry drum bus.
    DrumBus = 1,
    /// Detect on pad 0 alone.
    Kick = 2,
}

impl SidechainSource {
    pub const ALL: [SidechainSource; 3] = [
        SidechainSource::Off,
        SidechainSource::DrumBus,
        SidechainSource::Kick,
    ];
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => SidechainSource::DrumBus,
            2 => SidechainSource::Kick,
            _ => SidechainSource::Off,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            SidechainSource::Off => "Off",
            SidechainSource::DrumBus => "Drums",
            SidechainSource::Kick => "Kick",
        }
    }
}

/// One compressor's settings.
///
/// Grouped rather than flattened into [`Params`] because there are two
/// instances -- the synth bus insert and the master glue -- and grouping makes
/// them identical by construction instead of two hand-copied blocks of seven
/// fields.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorParams {
    pub on: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    pub sidechain: SidechainSource,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            on: false,
            threshold_db: -12.0,
            ratio: 4.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: 0.0,
            sidechain: SidechainSource::Off,
        }
    }
}

/// The atomic mirror of [`CompressorParams`].
#[derive(Debug)]
pub struct SharedCompressor {
    pub on: AtomicBool32,
    pub threshold_db: AtomicF32,
    pub ratio: AtomicF32,
    pub attack_ms: AtomicF32,
    pub release_ms: AtomicF32,
    pub makeup_db: AtomicF32,
    pub sidechain: AtomicEnum,
}

impl SharedCompressor {
    fn new(p: &CompressorParams) -> Self {
        Self {
            on: AtomicBool32::new(p.on),
            threshold_db: AtomicF32::new(p.threshold_db),
            ratio: AtomicF32::new(p.ratio),
            attack_ms: AtomicF32::new(p.attack_ms),
            release_ms: AtomicF32::new(p.release_ms),
            makeup_db: AtomicF32::new(p.makeup_db),
            sidechain: AtomicEnum::new(p.sidechain as u32),
        }
    }

    fn snapshot(&self) -> CompressorParams {
        CompressorParams {
            on: self.on.get(),
            threshold_db: sane(self.threshold_db.get(), -12.0).clamp(-60.0, 0.0),
            ratio: sane(self.ratio.get(), 4.0).clamp(1.0, 20.0),
            attack_ms: sane(self.attack_ms.get(), 10.0).clamp(0.1, 100.0),
            release_ms: sane(self.release_ms.get(), 100.0).clamp(5.0, 1000.0),
            makeup_db: sane(self.makeup_db.get(), 0.0).clamp(0.0, 24.0),
            sidechain: SidechainSource::from_u32(self.sidechain.get()),
        }
    }

    fn apply(&self, p: &CompressorParams) {
        self.on.set(p.on);
        self.threshold_db.set(p.threshold_db);
        self.ratio.set(p.ratio);
        self.attack_ms.set(p.attack_ms);
        self.release_ms.set(p.release_ms);
        self.makeup_db.set(p.makeup_db);
        self.sidechain.set(p.sidechain as u32);
    }
}
```

Then the six wiring points, next to the ones added in Step 3:

```rust
// Params
    pub comp_synth: CompressorParams,
    pub comp_master: CompressorParams,
// impl Default for Params
            comp_synth: CompressorParams::default(),
            comp_master: CompressorParams::default(),
// SharedParams
    pub comp_synth: SharedCompressor,
    pub comp_master: SharedCompressor,
// SharedParams::from_params
            comp_synth: SharedCompressor::new(&p.comp_synth),
            comp_master: SharedCompressor::new(&p.comp_master),
// snapshot
            comp_synth: self.comp_synth.snapshot(),
            comp_master: self.comp_master.snapshot(),
// apply
        self.comp_synth.apply(&p.comp_synth);
        self.comp_master.apply(&p.comp_master);
```

Finally re-export from `crates/synth_core/src/lib.rs`, extending the existing `pub use params::{...}` line:

```rust
pub use params::{CompressorParams, Params, SharedParams, SidechainSource, Smoothed, VoiceMode};
```

- [ ] **Step 8: Run the tests and watch them pass**

Run: `cargo test -p synth_core`
Expected: PASS, all of it. Nothing reads the new parameters yet, so the audio cannot have moved and the golden vector must still pass.

- [ ] **Step 9: Check clippy has not regressed**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated" | grep -v duplicates`
Expected: the 28-warning baseline, unchanged. `SidechainSource::name` is unused until Task 6; if clippy flags it, leave it and finish the wiring in Task 6 rather than adding `#[allow(dead_code)]`.

- [ ] **Step 10: Commit**

```bash
git add crates/synth_core/src/params.rs crates/synth_core/src/lib.rs
git commit -F - <<'MSG'
Add the bus send and compressor parameters

Nothing reads them yet. CompressorParams and SharedCompressor are grouped
rather than flattened so the synth and master instances are identical by
construction.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
```

---

### Task 2: Compressor DSP

The compressor as a self-contained effect: buffers in, buffers out, an optional detector. No routing, no parameter plumbing, no knowledge of drums. Task 4 wires it up.

**Files:**
- Create: `crates/synth_core/src/fx/compressor.rs`
- Modify: `crates/synth_core/src/fx/mod.rs` (add `mod compressor;` and the re-export)
- Modify: `crates/synth_core/src/lib.rs` (extend `pub use fx::{...}`)
- Test: `crates/synth_core/src/fx/compressor.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `CompressorParams`, `SidechainSource` from Task 1.
- Produces:
  - `pub struct Compressor` with
    - `pub fn new(sample_rate: f32) -> Self`
    - `pub fn set_sample_rate(&mut self, sample_rate: f32)`
    - `pub fn process(&mut self, left: &mut [f32], right: &mut [f32], detector: Option<&[f32]>, p: &CompressorParams)`
    - `pub fn gain_reduction_db(&self) -> f32`
  - `detector` of `None` means detect on the signal being compressed. When `Some`, it must be at least as long as `left`.
  - `process` is length-agnostic: all state advances per sample, so it does not need `BLOCK`-sized slices.

- [ ] **Step 1: Write the failing bypass and below-threshold tests**

Create `crates/synth_core/src/fx/compressor.rs` containing only the module doc and this test module:

```rust
//! A feed-forward peak compressor with an optional external detector.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::CompressorParams;

    const SR: f32 = 48_000.0;

    /// Turns a linear gain into dB, for asserting on what the compressor did.
    fn db(gain: f32) -> f32 {
        20.0 * gain.log10()
    }

    #[test]
    fn a_bypassed_compressor_is_a_bit_exact_pass_through() {
        let mut comp = Compressor::new(SR);
        let p = CompressorParams::default(); // on: false
        let mut l = [0.9, -0.7, 0.3, 0.0];
        let mut r = [0.1, 0.2, -0.9, 0.5];
        let want_l = l;
        let want_r = r;

        comp.process(&mut l, &mut r, None, &p);

        assert_eq!(l, want_l);
        assert_eq!(r, want_r);
        assert_eq!(comp.gain_reduction_db(), 0.0);
    }

    #[test]
    fn a_signal_below_threshold_passes_at_unity() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0; // 0.2512 linear; -6 dB knee floor is -15 dB
        // -40 dB, far below the knee.
        let mut l = [0.01; 512];
        let mut r = [0.01; 512];

        comp.process(&mut l, &mut r, None, &p);

        assert!((l[511] - 0.01).abs() < 1e-9, "got {}", l[511]);
        assert!(comp.gain_reduction_db() < 1e-6);
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Add `mod compressor;` to `crates/synth_core/src/fx/mod.rs` first so the file is part of the crate, then run:

Run: `cargo test -p synth_core compressor`
Expected: FAIL to compile, `cannot find type 'Compressor' in this scope`.

- [ ] **Step 3: Write the compressor**

Above the test module in `crates/synth_core/src/fx/compressor.rs`:

```rust
use crate::params::CompressorParams;

/// The knee width in dB. Fixed rather than exposed: a knee control is one more
/// knob for something almost nobody adjusts away from "a few dB".
const KNEE_DB: f32 = 6.0;

/// Below this the detector is treated as silence, so `log10` never sees zero.
const FLOOR: f32 = 1.0e-9;

/// A feed-forward peak compressor.
///
/// State is one float plus the sample rate -- no delay lines, no lookahead, no
/// allocation. Two instances cost almost nothing next to one reverb.
#[derive(Debug, Clone)]
pub struct Compressor {
    /// Current gain reduction in dB, positive meaning "turned down by".
    gr_db: f32,
    sample_rate: f32,
}

impl Compressor {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            gr_db: 0.0,
            sample_rate,
        }
    }

    /// Changing the sample rate invalidates the envelope coefficients, so the
    /// envelope is cleared rather than left holding a reduction computed for a
    /// different rate.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.gr_db = 0.0;
    }

    /// The reduction currently being applied, in dB, for a GR meter.
    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db
    }

    /// Compresses `left` and `right` in place.
    ///
    /// `detector` supplies the signal the gain is computed from. `None` means
    /// detect on the signal being compressed. When `Some`, it must be at least
    /// as long as `left`.
    pub fn process(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
        detector: Option<&[f32]>,
        p: &CompressorParams,
    ) {
        if !p.on {
            // Bypass is bit-exact: the buffers are not touched at all.
            self.gr_db = 0.0;
            return;
        }

        // One exp per parameter per call, not per sample. The parameters are
        // constant for the length of the call.
        let attack = coefficient(p.attack_ms, self.sample_rate);
        let release = coefficient(p.release_ms, self.sample_rate);
        let slope = 1.0 - 1.0 / p.ratio;
        let half_knee = KNEE_DB * 0.5;
        let makeup = p.makeup_db;

        let count = left.len().min(right.len());
        for i in 0..count {
            let level = match detector {
                Some(d) => d[i].abs(),
                None => left[i].abs().max(right[i].abs()),
            };
            let level_db = 20.0 * level.max(FLOOR).log10();
            let over = level_db - p.threshold_db;

            let target = if over <= -half_knee {
                0.0
            } else if over >= half_knee {
                over * slope
            } else {
                // Quadratic interpolation across the knee: zero slope at the
                // lower corner, full slope at the upper one.
                let x = over + half_knee;
                slope * x * x / (2.0 * KNEE_DB)
            };

            let coef = if target > self.gr_db { attack } else { release };
            self.gr_db = target + (self.gr_db - target) * coef;

            let gain = db_to_gain(makeup - self.gr_db);
            left[i] *= gain;
            right[i] *= gain;
        }
    }
}

/// The one-pole coefficient that reaches 1 - 1/e of its travel in `time_ms`.
fn coefficient(time_ms: f32, sample_rate: f32) -> f32 {
    let samples = (time_ms * 0.001 * sample_rate).max(1.0);
    (-1.0 / samples).exp()
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}
```

Then in `crates/synth_core/src/fx/mod.rs`, next to the existing `pub use`:

```rust
pub use compressor::Compressor;
```

and extend the `pub use fx::{...}` line in `crates/synth_core/src/lib.rs`:

```rust
pub use fx::{Compressor, FxChain, NoteDivision};
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p synth_core compressor`
Expected: PASS, both tests.

- [ ] **Step 5: Write the failing gain-law tests**

Append to the test module. The arithmetic is worked out rather than
copy-pasted from a run: at threshold -12 dB, ratio 4, an amplitude of 0.5 is
-6.0206 dB, so `over` is 5.9794 dB -- past the 3 dB half-knee, so the
above-knee branch applies and the reduction is `5.9794 * 0.75 = 4.4845` dB.

```rust
    #[test]
    fn a_steady_signal_settles_at_the_gain_reduction_the_ratio_implies() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;
        // One second is a thousand attack time constants: fully settled.
        let mut l = [0.5; 48_000];
        let mut r = [0.5; 48_000];

        comp.process(&mut l, &mut r, None, &p);

        // -6.0206 dB is 5.9794 over the threshold; a 4:1 ratio removes 3/4.
        let want_gr = 5.9794 * 0.75;
        assert!(
            (comp.gain_reduction_db() - want_gr).abs() < 0.01,
            "gr {} want {}",
            comp.gain_reduction_db(),
            want_gr
        );
        // 0.5 turned down by 4.4845 dB is 0.2982.
        assert!((l[47_999] - 0.2982).abs() < 1e-3, "got {}", l[47_999]);
    }

    #[test]
    fn the_attack_reaches_63_percent_of_its_travel_in_one_time_constant() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 10.0;

        // 10 ms at 48 kHz is exactly 480 samples, which is the time constant.
        let mut l = [0.5; 480];
        let mut r = [0.5; 480];
        comp.process(&mut l, &mut r, None, &p);

        let settled = 5.9794 * 0.75;
        let want = settled * (1.0 - (-1.0f32).exp()); // 63.2% of the travel
        assert!(
            (comp.gain_reduction_db() - want).abs() < 0.01,
            "gr {} want {}",
            comp.gain_reduction_db(),
            want
        );
    }

    #[test]
    fn the_release_gives_the_gain_back() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;
        p.release_ms = 10.0;

        let mut l = [0.5; 48_000];
        let mut r = [0.5; 48_000];
        comp.process(&mut l, &mut r, None, &p);
        let compressed = comp.gain_reduction_db();
        assert!(compressed > 4.0);

        // Silence for 480 samples: one release time constant of decay.
        let mut l = [0.0; 480];
        let mut r = [0.0; 480];
        comp.process(&mut l, &mut r, None, &p);

        let want = compressed * (-1.0f32).exp(); // 36.8% of the way left
        assert!(
            (comp.gain_reduction_db() - want).abs() < 0.01,
            "gr {} want {}",
            comp.gain_reduction_db(),
            want
        );
    }

    #[test]
    fn gain_reduction_telemetry_matches_the_gain_actually_applied() {
        let mut comp = Compressor::new(SR);
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -12.0;
        p.ratio = 4.0;
        p.attack_ms = 1.0;
        p.makeup_db = 6.0;

        let mut l = [0.5; 48_000];
        let mut r = [0.5; 48_000];
        comp.process(&mut l, &mut r, None, &p);

        let applied = db(l[47_999] / 0.5);
        let reported = p.makeup_db - comp.gain_reduction_db();
        assert!((applied - reported).abs() < 1e-3, "{applied} vs {reported}");
    }

    #[test]
    fn an_external_detector_drives_the_gain_instead_of_the_signal() {
        let mut p = CompressorParams::default();
        p.on = true;
        p.threshold_db = -40.0;
        p.ratio = 8.0;
        p.attack_ms = 1.0;

        // A loud signal with a silent detector is left alone.
        let mut comp = Compressor::new(SR);
        let mut l = [0.5; 4_800];
        let mut r = [0.5; 4_800];
        let silent = [0.0; 4_800];
        comp.process(&mut l, &mut r, Some(&silent), &p);
        assert!(comp.gain_reduction_db() < 1e-6);
        assert_eq!(l[4_799], 0.5);

        // A quiet signal with a loud detector is ducked.
        let mut comp = Compressor::new(SR);
        let mut l = [0.01; 4_800];
        let mut r = [0.01; 4_800];
        let loud = [1.0; 4_800];
        comp.process(&mut l, &mut r, Some(&loud), &p);
        assert!(comp.gain_reduction_db() > 30.0, "{}", comp.gain_reduction_db());
        assert!(l[4_799] < 0.001, "got {}", l[4_799]);
    }
```

- [ ] **Step 6: Run the tests and watch them pass**

Run: `cargo test -p synth_core compressor`
Expected: PASS. These follow from the code written in Step 3, so they should pass without further edits -- but they were written to a hand-computed expectation, not to observed output, so a failure here means the DSP is wrong, not the numbers.

- [ ] **Step 7: Confirm real-time safety**

Run: `grep -n "Vec\|Box\|String\|println\|unwrap\|panic" crates/synth_core/src/fx/compressor.rs`
Expected: matches only inside `#[cfg(test)] mod tests`, if any at all.

- [ ] **Step 8: Run the whole suite and clippy**

Run: `cargo test -p synth_core` then `cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated" | grep -v duplicates`
Expected: all green; clippy at the 28-warning baseline.

- [ ] **Step 9: Commit**

```bash
git add crates/synth_core/src/fx/compressor.rs crates/synth_core/src/fx/mod.rs crates/synth_core/src/lib.rs
git commit -F - <<'MSG'
Add a feed-forward compressor with an optional external detector

Peak detection, a fixed 6 dB soft knee, attack and release on the
gain-reduction envelope. Nothing calls it yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
```

---

---

### Task 3: Stereo drum rack

Turn `DrumRack`'s single mono scratch buffer into five: a stereo dry pair, a
stereo send pair, and a mono kick tap for the sidechain detector. The engine's
two mix sites move to the dry pair. Behaviour at defaults (`pad_pan = 0.0`)
is bit-for-bit what it is today; `drum_to_fx` is still in place and still
decides the routing. Nothing consumes `send_l`/`send_r`/`kick` yet — Task 4
does.

**Files:**
- Modify: `crates/synth_core/src/drums/rack.rs` (whole file: struct, `new`,
  `silence`, `render`, `output` -> `buses`, and the test helpers)
- Modify: `crates/synth_core/src/drums/mod.rs:14` (export `DrumBuses`)
- Modify: `crates/synth_core/src/lib.rs:44` (re-export `DrumBuses`)
- Modify: `crates/synth_core/src/engine.rs:348-368` (two mix sites)
- Test: `crates/synth_core/src/drums/rack.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes (Task 1): `Params::pad_pan: [f32; PAD_COUNT]`,
  `Params::pad_send: [f32; PAD_COUNT]`.
- Produces:
  ```rust
  pub struct DrumBuses {
      pub dry_l: [f32; BLOCK],
      pub dry_r: [f32; BLOCK],
      pub send_l: [f32; BLOCK],
      pub send_r: [f32; BLOCK],
      pub kick: [f32; BLOCK],
  }

  impl DrumRack {
      pub fn buses(&self) -> &DrumBuses;
  }
  ```
  `DrumRack::output` is deleted. `render`'s signature does not change.

**The pan law** (spec, "Pan law"), unity at centre:

```rust
let l = (1.0 - pan).min(1.0);
let r = (1.0 + pan).min(1.0);
```

At `pan = 0.0` both weights are exactly `1.0`, so `sample * weight == sample`
bit for bit. That, plus summing before applying `drum_level` exactly as the
current loop does, is what makes centre reproduce today's mono value with no
epsilon.

- [ ] **Step 1: Capture today's mono output as a characterization reference**

This is the only way to prove "equal to today's mono value **exactly**" —
the voices are synthesized, so there is no closed form to assert against.
Capture the numbers from the *unmodified* rack first, then port the test to
the new API in Step 5. This is a new characterization capture, not a
re-baseline of an existing assertion.

Add to `mod tests` in `rack.rs`, against the code as it stands today:

```rust
    /// Prints the reference the stereo rewrite has to reproduce at centre.
    /// Temporary: replaced by `a_centred_pad_reproduces_the_mono_sum` below.
    #[test]
    fn capture_mono_reference() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });
        // Advance until the kick has actually struck, then take one block.
        let mut got = [0.0f32; 8];
        for _ in 0..200 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            rack.render(BLOCK, adv, clock.view(), &p);
            let out = rack.output(BLOCK);
            if out.iter().any(|s| *s != 0.0) {
                got.copy_from_slice(&out[..8]);
                break;
            }
        }
        panic!("MONO_REFERENCE = {got:?}");
    }
```

- [ ] **Step 2: Run it and record the numbers**

Run: `cargo test -p synth_core --lib drums::rack::tests::capture_mono_reference -- --nocapture`
Expected: FAIL with `MONO_REFERENCE = [ ... eight floats ... ]`.

Copy those eight floats verbatim (full precision, as printed) — they become
`MONO_REFERENCE` in Step 5. Then **delete `capture_mono_reference`**; it has
done its job and must not be committed.

- [ ] **Step 3: Write the failing tests**

Replace the captured helper with the real tests. Put `MONO_REFERENCE` at the
top of `mod tests`, next to `setup`:

```rust
    /// One block of the kick, taken from the mono rack before the stereo
    /// rewrite. Centre panning has to land on these numbers exactly, or the
    /// rewrite changed every existing patch.
    const MONO_REFERENCE: [f32; 8] = [ /* the eight floats from Step 2 */ ];

    /// Renders until the rack makes sound, then returns that block's
    /// `dry_l` and `dry_r`.
    fn first_sounding_block(
        rack: &mut DrumRack,
        clock: &mut Clock,
        p: &Params,
    ) -> ([f32; 8], [f32; 8]) {
        for _ in 0..200 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            rack.render(BLOCK, adv, clock.view(), p);
            let bus = rack.buses();
            if bus.dry_l.iter().chain(bus.dry_r.iter()).any(|s| *s != 0.0) {
                let mut l = [0.0f32; 8];
                let mut r = [0.0f32; 8];
                l.copy_from_slice(&bus.dry_l[..8]);
                r.copy_from_slice(&bus.dry_r[..8]);
                return (l, r);
            }
        }
        panic!("the pad never sounded");
    }

    fn kick_grid(rack: &mut DrumRack) {
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });
    }

    /// Spec: `pad_pan = 0.0` yields `l == r`, equal to today's mono value
    /// exactly.
    #[test]
    fn a_centred_pad_reproduces_the_mono_sum() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        kick_grid(&mut rack);

        let (l, r) = first_sounding_block(&mut rack, &mut clock, &p);
        assert_eq!(l, r, "centre is not centred");
        assert_eq!(l, MONO_REFERENCE, "centre no longer matches the mono rack");
    }

    /// Spec: `pad_pan = -1.0` yields `r == 0.0` and `l` unchanged. Constant
    /// amplitude in the surviving channel is the deliberate trade-off.
    #[test]
    fn a_hard_left_pad_empties_the_right_channel_and_leaves_the_left_alone() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        p.pad_pan[0] = -1.0;
        let (mut rack, mut clock) = setup(&p);
        kick_grid(&mut rack);

        let (l, r) = first_sounding_block(&mut rack, &mut clock, &p);
        assert_eq!(r, [0.0; 8], "the right channel is not empty");
        assert_eq!(l, MONO_REFERENCE, "hard left changed the left channel");
    }

    /// The mirror image, so a sign slip in the pan law cannot pass.
    #[test]
    fn a_hard_right_pad_empties_the_left_channel() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        p.pad_pan[0] = 1.0;
        let (mut rack, mut clock) = setup(&p);
        kick_grid(&mut rack);

        let (l, r) = first_sounding_block(&mut rack, &mut clock, &p);
        assert_eq!(l, [0.0; 8], "the left channel is not empty");
        assert_eq!(r, MONO_REFERENCE, "hard right changed the right channel");
    }

    /// At the default `pad_send = 1.0` the send pair is the dry pair. The
    /// engine scales it by `drum_send`, which defaults to 0.0, so this is
    /// what makes `drum_send` behave like the old bool.
    #[test]
    fn the_send_pair_matches_the_dry_pair_at_full_send() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        kick_grid(&mut rack);

        for _ in 0..200 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            rack.render(BLOCK, adv, clock.view(), &p);
        }
        let bus = rack.buses();
        assert_eq!(bus.send_l, bus.dry_l);
        assert_eq!(bus.send_r, bus.dry_r);
    }

    /// Spec: per-pad send differences have to be rendered, not derived — the
    /// summed output cannot tell you which pad contributed what.
    #[test]
    fn a_pad_with_its_send_closed_stays_out_of_the_send_pair() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        p.pad_send[0] = 0.0;
        let (mut rack, mut clock) = setup(&p);
        kick_grid(&mut rack);

        let mut dry_peak = 0.0f32;
        let mut send_peak = 0.0f32;
        for _ in 0..200 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            rack.render(BLOCK, adv, clock.view(), &p);
            let bus = rack.buses();
            dry_peak = dry_peak.max(peak_of(&bus.dry_l));
            send_peak = send_peak.max(peak_of(&bus.send_l));
        }
        assert!(dry_peak > 0.05, "the kick never reached the dry bus");
        assert_eq!(send_peak, 0.0, "a closed send still leaked");
    }

    /// The detector tap follows pad 0 alone, post-level and pre-pan, so a
    /// hard-panned kick still ducks the same amount.
    #[test]
    fn the_kick_tap_ignores_pan() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        p.pad_pan[0] = -1.0;
        let (mut rack, mut clock) = setup(&p);
        kick_grid(&mut rack);

        let mut kick_peak = 0.0f32;
        for _ in 0..200 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            rack.render(BLOCK, adv, clock.view(), &p);
            kick_peak = kick_peak.max(peak_of(&rack.buses().kick));
        }
        assert!(kick_peak > 0.05, "the detector tap is silent: {kick_peak}");
    }

    /// A disabled rack must leave every buffer clean, including the three
    /// the engine does not read yet — a stale send would ring into the
    /// return the moment Task 4 wires it up.
    #[test]
    fn disabling_clears_every_buffer() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        kick_grid(&mut rack);
        for _ in 0..200 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            rack.render(BLOCK, adv, clock.view(), &p);
        }

        p.drum_enabled = false;
        let adv = clock.advance(BLOCK, ClockSource::Internal);
        rack.render(BLOCK, adv, clock.view(), &p);

        let bus = rack.buses();
        assert_eq!(bus.dry_l, [0.0; BLOCK]);
        assert_eq!(bus.dry_r, [0.0; BLOCK]);
        assert_eq!(bus.send_l, [0.0; BLOCK]);
        assert_eq!(bus.send_r, [0.0; BLOCK]);
        assert_eq!(bus.kick, [0.0; BLOCK]);
    }
```

And the shared peak helper, next to `setup`:

```rust
    fn peak_of(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p synth_core --lib drums::rack`
Expected: FAIL to compile — `no method named 'buses' found for struct 'DrumRack'`.

- [ ] **Step 5: Rewrite the rack**

`crates/synth_core/src/drums/rack.rs`. Change the module doc's last clause
from "one mono bus" to "a stereo bus pair". Then:

```rust
/// The rack's outputs for one block.
///
/// The send pair exists because per-pad sends cannot be recovered from the
/// summed dry pair; the kick tap exists so a sidechain detector can follow
/// one pad. All three are rendered together in the same pass so a voice is
/// only advanced once.
#[derive(Clone, Copy)]
pub struct DrumBuses {
    pub dry_l: [f32; BLOCK],
    pub dry_r: [f32; BLOCK],
    pub send_l: [f32; BLOCK],
    pub send_r: [f32; BLOCK],
    /// Pad 0 alone, post-level, pre-pan, mono.
    pub kick: [f32; BLOCK],
}

impl DrumBuses {
    const SILENT: Self = Self {
        dry_l: [0.0; BLOCK],
        dry_r: [0.0; BLOCK],
        send_l: [0.0; BLOCK],
        send_r: [0.0; BLOCK],
        kick: [0.0; BLOCK],
    };

    fn clear(&mut self, frames: usize) {
        let n = frames.min(BLOCK);
        self.dry_l[..n].fill(0.0);
        self.dry_r[..n].fill(0.0);
        self.send_l[..n].fill(0.0);
        self.send_r[..n].fill(0.0);
        self.kick[..n].fill(0.0);
    }
}
```

The struct field `buffer: [f32; BLOCK]` becomes `out: DrumBuses`, and
`new` initialises it with `out: DrumBuses::SILENT`.

`silence()` becomes `self.out = DrumBuses::SILENT;`.

Both early returns in `render` (the disabled path at ~line 103 and the
all-silent path at ~line 113) replace `self.buffer[..frames].fill(0.0);`
with `self.out.clear(frames);`.

The render loop replaces lines 117-125:

```rust
        // `drum_level` is applied after the pads are summed, exactly as the
        // mono rack did: `(a + b) * level` and `a * level + b * level` are
        // not the same float, and centre has to match the old value bit for
        // bit.
        let level = p.drum_level;
        for f in 0..frames {
            let mut dry_l = 0.0;
            let mut dry_r = 0.0;
            let mut send_l = 0.0;
            let mut send_r = 0.0;
            let mut kick = 0.0;
            for (i, voice) in self.voices.iter_mut().enumerate() {
                let sample = voice.next();
                if i == 0 {
                    kick = sample;
                }
                // Unity at centre, constant amplitude in the surviving
                // channel. Equal power would push the extremes 3 dB up.
                let pan = p.pad_pan[i];
                let l = sample * (1.0 - pan).min(1.0);
                let r = sample * (1.0 + pan).min(1.0);
                dry_l += l;
                dry_r += r;
                let send = p.pad_send[i];
                send_l += l * send;
                send_r += r * send;
            }
            self.out.dry_l[f] = dry_l * level;
            self.out.dry_r[f] = dry_r * level;
            self.out.send_l[f] = send_l * level;
            self.out.send_r[f] = send_r * level;
            self.out.kick[f] = kick * level;
        }
        true
```

And `output` becomes:

```rust
    /// The block just rendered. Valid up to `BLOCK` frames; the engine only
    /// reads the `count` it asked for.
    pub fn buses(&self) -> &DrumBuses {
        &self.out
    }
```

- [ ] **Step 6: Update the rack's own test helper**

The existing `render` helper folds `output(BLOCK)` into a peak. Point it at
the dry pair:

```rust
    fn render(r: &mut DrumRack, c: &mut Clock, p: &Params) -> (bool, f32) {
        let adv = c.advance(BLOCK, ClockSource::Internal);
        let sounded = r.render(BLOCK, adv, c.view(), p);
        let bus = r.buses();
        let peak = peak_of(&bus.dry_l).max(peak_of(&bus.dry_r));
        (sounded, peak)
    }
```

`a_closed_hat_chokes_the_open_one` also folds `rack.output(BLOCK)` inline, at
two places. Both become:

```rust
                let peak = peak_of(&rack.buses().dry_l);
```

- [ ] **Step 7: Export the new type**

`crates/synth_core/src/drums/mod.rs:14`:

```rust
pub use rack::{DrumBuses, DrumRack};
```

`crates/synth_core/src/lib.rs:44`:

```rust
pub use drums::{Cell, DrumBuses, DrumPattern, DrumRack, DrumVoice, Pad, PAD_COUNT};
```

- [ ] **Step 8: Update the engine's two mix sites**

`crates/synth_core/src/engine.rs`. Both blocks currently read the mono bus
into both channels. `drum_to_fx` stays exactly as it is — Task 4 removes it.
At ~line 348:

```rust
        if drums_playing && params.drum_to_fx {
            let bus = self.drums.buses();
            for i in 0..count {
                left[i] += bus.dry_l[i] * drum_gain;
                right[i] += bus.dry_r[i] * drum_gain;
            }
        }
```

and the same substitution at ~line 362 for the `!params.drum_to_fx` block.
At the default `pad_pan = 0.0` these are the same numbers as before, so
every existing engine test keeps passing unchanged.

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test -p synth_core`
Expected: PASS, including `the_dry_path_matches_the_golden_vector` (drums are
off in it, so it never touched these buffers),
`a_zeroed_drum_gain_silences_the_drums_on_either_routing` and
`a_zeroed_synth_gain_leaves_the_drums_alone`.

Then the whole workspace: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 10: Check clippy against the baseline**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"`
Expected: the same 28 warnings as before the task. Ignore any
`(N duplicates)` line. Do not pipe through `tail`.

- [ ] **Step 11: Verify real-time safety**

Run: `grep -n "Vec\|Box\|vec!\|String\|lock()\|println!" crates/synth_core/src/drums/rack.rs`
Expected: no match outside `#[cfg(test)]`. `DrumBuses` is five fixed arrays
by value; nothing in `render` allocates.

- [ ] **Step 12: Commit**

```bash
git add crates/synth_core/src/drums/rack.rs crates/synth_core/src/drums/mod.rs \
        crates/synth_core/src/lib.rs crates/synth_core/src/engine.rs
git commit -F - <<'MSG'
Give the drum rack a stereo pair, a send pair and a kick tap

The rack summed eight pads into one mono buffer and the engine copied it
into both channels, so per-pad pan had nowhere to live and per-pad sends
could not be recovered downstream. It now renders five buffers in one
pass: a panned dry pair, the same scaled by each pad's send, and pad 0
alone for a sidechain detector to follow.

The pan law is unity at centre -- `min(1, 1 -/+ pan)` -- and the level
still lands after the pads are summed, so a centred pad reproduces the
old mono value bit for bit. A characterization test pins those eight
samples so it cannot drift.

Nothing reads the send pair or the kick tap yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
```

---

### Task 4: The return bus, the sends and the two compressor inserts

This is the task the other five exist for. `FxChain` stops being an insert on
the synth and becomes a return fed by two sends; the two compressors go in as
inserts; `drum_to_fx` is deleted everywhere it appears.

`drum_to_fx` cannot be removed in a smaller step: it is read in `params.rs`
(six sites), `engine.rs` (two) and `sections/drums.rs` (two), and deleting the
field breaks all three crates at once. The UI toggle is therefore deleted
here; Task 6 adds the knobs that replace it.

**Files:**
- Modify: `crates/synth_core/src/engine.rs` — two `Compressor` fields, the
  `sidechain` helper, the rewritten tail of `render_chunk` (~340-370),
  `set_sample_rate` (~155-176)
- Modify: `crates/synth_core/src/params.rs` — delete `drum_to_fx` at `:383`,
  `:501`, `:625`, `:788`, `:911`, `:1011`
- Modify: `crates/bevy_synth_ui/src/sections/drums.rs:48-55` — delete the
  toggle
- Test: `crates/synth_core/src/engine.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes (Task 1): `Params::synth_send`, `Params::drum_send`,
  `Params::comp_synth: CompressorParams`, `Params::comp_master`,
  `SidechainSource`.
- Consumes (Task 2): `Compressor::new`, `Compressor::set_sample_rate`,
  `Compressor::process(&mut [f32], &mut [f32], Option<&[f32]>, &CompressorParams)`.
- Consumes (Task 3): `DrumRack::buses() -> &DrumBuses`.
- Produces (Task 5 reads it): `Compressor::gain_reduction_db(&self) -> f32`
  on `Engine::comp_synth` and `Engine::comp_master`.

**The signal path this builds** (spec, "Chosen topology"):

```
synth voices -> drive -> DC -> synth_gain -> comp_synth --+--------------+
                                                          |              |
                                   send = that * synth_send              |
drums -> dry pair * drum_gain -----------------------------------> master sum
      \-> send pair * drum_gain * drum_send -> send        |              |
                                               |           v              |
                                          RETURN: delay -> reverb         |
                                               |                          |
                          return = chain(send) - send --------------------+
                                                          |
                                        master sum -> comp_master -> master_gain
```

**Why the subtraction** (spec, "The return contributes wet only"): `FxChain`
is an insert, so at `mix = 0` it hands back its input unchanged. Summing that
into a master that already carries the dry would double the dry. Subtracting
the send leaves the wet alone: `x - x` is exactly `+0.0`, so at default mixes
the master sum is untouched and the golden vector cannot move.

- [ ] **Step 1: Write the routing tests**

Add to `mod tests` in `engine.rs`. `engine_preset` and `kick_on_one` already
exist there; reuse them.

```rust
    /// The claim the golden vector rests on: with both mixes at zero the
    /// return hands back exactly what it was sent, so `chain(send) - send`
    /// is `0.0` and the send knob is inaudible. Bit-identical, not close.
    #[test]
    fn the_return_adds_nothing_at_zero_mix() {
        let (mut full, tx_full) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.synth_send.set(1.0);
        });
        let (mut none, tx_none) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.synth_send.set(0.0);
        });
        for tx in [&tx_full, &tx_none] {
            tx.push(Event::NoteOn { note: 60, velocity: 0.8 });
        }

        assert_eq!(render(&mut full, 4096), render(&mut none, 4096));
    }

    /// The send/return has to reproduce the insert it replaced. The reference
    /// is an `FxChain` of its own, fed the engine's dry output in the same
    /// 32-frame chunks the engine uses: that *is* the old code path.
    #[test]
    fn a_full_send_reproduces_the_insert_it_replaced() {
        // The dry signal. At zero mix the return contributes nothing, so
        // this is the engine's dry path with the master fader wide open.
        let (mut dry_engine, tx_dry) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
        });
        tx_dry.push(Event::NoteOn { note: 60, velocity: 0.8 });
        let dry = render(&mut dry_engine, 4096);

        // The same signal through a fresh chain, as an insert.
        let mut l: Vec<f32> = dry.iter().step_by(2).copied().collect();
        let mut r: Vec<f32> = dry.iter().skip(1).step_by(2).copied().collect();
        let mut chain = crate::fx::FxChain::new(48_000.0);
        let mut reference_params = Params::default();
        reference_params.delay_mix = 0.5;
        for (cl, cr) in l.chunks_mut(BLOCK).zip(r.chunks_mut(BLOCK)) {
            chain.process_block(cl, cr, &reference_params, reference_params.tempo);
        }

        // And the same signal through the engine, sent to the return at 1.0.
        let (mut wet_engine, tx_wet) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
            p.synth_send.set(1.0);
            p.delay_mix.set(0.5);
        });
        tx_wet.push(Event::NoteOn { note: 60, velocity: 0.8 });
        let wet = render(&mut wet_engine, 4096);

        // Sanity: the delay actually did something, or this proves nothing.
        assert!(peak(&wet) > 0.0);
        assert_ne!(wet, dry, "the delay was inaudible; the test is vacuous");

        for (i, sample) in wet.iter().enumerate() {
            let want = if i % 2 == 0 { l[i / 2] } else { r[i / 2] };
            assert!(
                (sample - want).abs() < 1.0e-6,
                "sample {i}: return {sample} vs insert {want}"
            );
        }
    }

    /// `drum_send` replaces the old bool's `false` position: at 0.0 the rack
    /// puts nothing into the return, so raising a mix changes nothing.
    #[test]
    fn a_closed_drum_send_keeps_the_rack_out_of_the_return() {
        let (mut wet, tx_wet) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(0.0);
            p.delay_mix.set(1.0);
        });
        let (mut dry, tx_dry) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(0.0);
            p.delay_mix.set(0.0);
        });
        for tx in [&tx_wet, &tx_dry] {
            kick_on_one(tx);
            tx.push(Event::ClockStart);
        }

        let wet_out = render(&mut wet, 8192);
        assert!(peak(&wet_out) > 0.0, "the rack never sounded");
        assert_eq!(wet_out, render(&mut dry, 8192));
    }

    /// The other position of the old bool: opened up, the rack reaches the
    /// effects. The algebra that makes this equal the old insert is the same
    /// code `a_full_send_reproduces_the_insert_it_replaced` pins; what is
    /// specific here is that the rack's send pair is wired to it at all.
    #[test]
    fn an_open_drum_send_reaches_the_effects() {
        let (mut open, tx_open) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(1.0);
            p.delay_mix.set(1.0);
        });
        let (mut shut, tx_shut) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(0.0);
            p.delay_mix.set(1.0);
        });
        for tx in [&tx_open, &tx_shut] {
            kick_on_one(tx);
            tx.push(Event::ClockStart);
        }

        assert_ne!(render(&mut open, 8192), render(&mut shut, 8192));
    }

    /// Per-pad sends are why the rack renders a send pair instead of the
    /// engine scaling the dry one. Pad 0 muted out of the send has to leave
    /// the return empty even with the bus send wide open.
    #[test]
    fn a_pad_send_of_zero_survives_to_the_return() {
        let (mut sent, tx_sent) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(1.0);
            p.delay_mix.set(1.0);
            p.pad_send[0].set(1.0);
        });
        let (mut held_back, tx_held) = engine_preset(|p| {
            p.melody_enabled.set(false);
            p.drum_enabled.set(true);
            p.drum_send.set(1.0);
            p.delay_mix.set(1.0);
            p.pad_send[0].set(0.0);
        });
        for tx in [&tx_sent, &tx_held] {
            kick_on_one(tx);
            tx.push(Event::ClockStart);
        }

        assert_ne!(render(&mut sent, 8192), render(&mut held_back, 8192));
    }
```

- [ ] **Step 2: Write the compressor-in-the-engine tests**

```rust
    /// A compressor whose detector never crosses the threshold computes
    /// `10^((0 - 0)/20)` — exactly 1.0 — and multiplying by 1.0 is bit-exact.
    /// So an armed sidechain over a silent rack has to be *identical*, not
    /// merely close: anything else means the detector is picking up the synth.
    #[test]
    fn an_armed_sidechain_over_a_silent_rack_changes_nothing() {
        let (mut armed, tx_armed) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.drum_enabled.set(true);
            p.comp_synth.on.set(true);
            p.comp_synth.sidechain.set(SidechainSource::Kick as u32);
        });
        let (mut bypassed, tx_bypassed) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.drum_enabled.set(true);
        });
        for tx in [&tx_armed, &tx_bypassed] {
            tx.push(Event::NoteOn { note: 60, velocity: 0.8 });
            tx.push(Event::ClockStart);
        }

        assert_eq!(render(&mut armed, 4096), render(&mut bypassed, 4096));
    }

    /// The point of the whole feature: pad 0 ducks the synth. Both engines
    /// hold the same note with the same compressor armed; only one has a
    /// kick programmed, and the ducked one has to come out quieter.
    #[test]
    fn the_kick_ducks_the_synth_through_the_sidechain() {
        let arm = |p: &SharedParams| {
            p.seq_playing.set(false);
            p.drum_enabled.set(true);
            p.drum_gain.set(0.0); // Hear the ducking, not the kick.
            p.comp_synth.on.set(true);
            p.comp_synth.sidechain.set(SidechainSource::Kick as u32);
            p.comp_synth.threshold_db.set(-30.0);
            p.comp_synth.ratio.set(10.0);
            p.comp_synth.attack_ms.set(1.0);
            p.comp_synth.release_ms.set(200.0);
        };
        let (mut ducked, tx_ducked) = engine_preset(arm);
        let (mut steady, tx_steady) = engine_preset(arm);
        for tx in [&tx_ducked, &tx_steady] {
            tx.push(Event::NoteOn { note: 60, velocity: 0.8 });
        }
        kick_on_one(&tx_ducked);
        tx_ducked.push(Event::ClockStart);
        tx_steady.push(Event::ClockStart);

        let ducked_out = render(&mut ducked, 8192);
        let steady_out = render(&mut steady, 8192);
        assert!(peak(&steady_out) > 0.0, "the synth never sounded");
        assert!(
            peak(&ducked_out) < peak(&steady_out) * 0.9,
            "the kick did not duck the synth: {} vs {}",
            peak(&ducked_out),
            peak(&steady_out)
        );
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p synth_core --lib engine`
Expected: FAIL to compile — `no field 'synth_send' on type 'SharedParams'`
would mean Task 1 was skipped; the expected failure is on `p.comp_synth` and
the routing behaviour, i.e. `assert_ne!` firing because the sends are not
wired yet.

- [ ] **Step 4: Add the compressor fields to the engine**

`crates/synth_core/src/engine.rs`. Import at the top, alongside the existing
`use crate::fx::FxChain;`:

```rust
use crate::fx::{Compressor, FxChain};
use crate::params::SidechainSource;
```

New fields, next to `fx`:

```rust
    /// Bus inserts. The synth one sits before the send tap so the return
    /// hears the compressed signal; the master one is glue on the sum.
    comp_synth: Compressor,
    comp_master: Compressor,
```

In `with_sources`, next to `fx: FxChain::new(sample_rate),`:

```rust
            comp_synth: Compressor::new(sample_rate),
            comp_master: Compressor::new(sample_rate),
```

In `set_sample_rate`, next to `self.fx.set_sample_rate(sample_rate);`:

```rust
        self.comp_synth.set_sample_rate(sample_rate);
        self.comp_master.set_sample_rate(sample_rate);
```

- [ ] **Step 5: Add the sidechain helper**

A free function, at the bottom of `engine.rs` next to `soft_clip`. It is free
rather than a method so the `&DrumBuses` borrow ends at the call: the returned
slice borrows the caller's scratch array, leaving `self.comp_synth` free to be
taken by `&mut`.

```rust
/// Fills `scratch` with the detector signal and hands back a view of it, or
/// `None` when the compressor should detect on its own input.
///
/// `gain` is the drum fader: a rack pulled to silence must not keep ducking
/// something the listener cannot hear.
fn sidechain<'a>(
    source: SidechainSource,
    bus: &DrumBuses,
    gain: f32,
    scratch: &'a mut [f32; BLOCK],
    count: usize,
) -> Option<&'a [f32]> {
    match source {
        SidechainSource::Off => None,
        SidechainSource::DrumBus => {
            for i in 0..count {
                scratch[i] = (bus.dry_l[i] + bus.dry_r[i]) * 0.5 * gain;
            }
            Some(&scratch[..count])
        }
        SidechainSource::Kick => {
            for i in 0..count {
                scratch[i] = bus.kick[i] * gain;
            }
            Some(&scratch[..count])
        }
    }
}
```

Add `DrumBuses` to the existing drums import at `engine.rs:17`:

```rust
use crate::drums::{Column, DrumBuses, DrumPattern, DrumRack};
```

- [ ] **Step 6: Rewrite the tail of `render_chunk`**

Replace everything from the `if drums_playing && params.drum_to_fx {` block
(~line 348) down to just before the final `for i in 0..count {` master-gain
loop (~line 370). The synth loop above it is unchanged.

```rust
        // Insert, before the send tap: the return hears the compressed
        // signal, which is what makes a pumped synth pump in the reverb too.
        let mut detector = [0.0f32; BLOCK];
        let source = params.comp_synth.sidechain;
        let key = sidechain(source, self.drums.buses(), drum_gain, &mut detector, count);
        self.comp_synth
            .process(&mut left[..count], &mut right[..count], key, &params.comp_synth);

        // The sends. The synth is tapped post-compressor; the drums bring
        // their own per-pad send pair, scaled by the bus knob.
        let mut send_l = [0.0f32; BLOCK];
        let mut send_r = [0.0f32; BLOCK];
        for i in 0..count {
            send_l[i] = left[i] * params.synth_send;
            send_r[i] = right[i] * params.synth_send;
        }

        // Drums are summed after the drive stage and the DC blocker: a kick
        // through the soft clipper at drive 3.0 is a different instrument,
        // and not a better one.
        if drums_playing {
            let bus = self.drums.buses();
            let send = drum_gain * params.drum_send;
            for i in 0..count {
                left[i] += bus.dry_l[i] * drum_gain;
                right[i] += bus.dry_r[i] * drum_gain;
                send_l[i] += bus.send_l[i] * send;
                send_r[i] += bus.send_r[i] * send;
            }
        }

        // The clock, not `params.tempo`: when an external MIDI clock is
        // driving the sequencer, that is the tempo the delay must lock to.
        let tempo = self.clock.tempo_bpm(params.steps_per_beat);

        // The return. `FxChain` is an insert, so it hands back dry + wet;
        // subtracting the send leaves the wet, and at zero mix that is
        // exactly `0.0` rather than approximately.
        let mut ret_l = send_l;
        let mut ret_r = send_r;
        self.fx
            .process_block(&mut ret_l[..count], &mut ret_r[..count], params, tempo);
        for i in 0..count {
            left[i] += ret_l[i] - send_l[i];
            right[i] += ret_r[i] - send_r[i];
        }

        // Glue on the sum.
        let source = params.comp_master.sidechain;
        let key = sidechain(source, self.drums.buses(), drum_gain, &mut detector, count);
        self.comp_master
            .process(&mut left[..count], &mut right[..count], key, &params.comp_master);
```

Note `ret_l = send_l` copies the array by value — `[f32; BLOCK]` is `Copy`,
and BLOCK is 32, so this is two 128-byte stack copies, no allocation.

- [ ] **Step 7: Delete `drum_to_fx`**

Six sites in `crates/synth_core/src/params.rs` — the `Params` field (`:383`),
its `Default` (`:501`), the `SharedParams` field (`:625`), `from_params`
(`:788`), `snapshot` (`:911`) and `apply` (`:1011`). Find them all with:

```bash
grep -rn "drum_to_fx" crates/
```

and delete every line the grep reports, including the two in
`crates/bevy_synth_ui/src/sections/drums.rs` (the `ui.checkbox` call at `:48`
and the `params.drum_to_fx.set(...)` at `:54`) and their surrounding
`if`/block if the checkbox stands alone. Re-run the grep afterwards; it must
print nothing.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS. Specifically:
- `the_dry_path_matches_the_golden_vector` — unedited, and the hash is never
  re-baselined. If it fails, the return is not contributing exactly `0.0`;
  fix the code, not the constant.
- `an_enabled_but_empty_rack_leaves_the_output_untouched`
- `a_zeroed_drum_gain_silences_the_drums_on_either_routing` — its name
  mentions a routing that no longer has two positions. Rename it
  `a_zeroed_drum_gain_silences_the_drums` and drop whichever half of it set
  `drum_to_fx`; keep the assertion.

- [ ] **Step 9: Check clippy against the baseline**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"`
Expected: the 28-warning baseline, unchanged. Ignore `(N duplicates)` lines.

- [ ] **Step 10: Verify real-time safety**

Run: `grep -n "vec!\|Vec::\|Box::\|String\|\.lock()\|println!" crates/synth_core/src/engine.rs | grep -v "^7[89][0-9]:\|test"`
Expected: only the existing `held: Vec` / `block: vec!` in `with_sources`,
which run at construction. `render_chunk` gained three `[f32; BLOCK]` stack
arrays and no allocation.

- [ ] **Step 11: Commit**

```bash
git add crates/synth_core/src/engine.rs crates/synth_core/src/params.rs \
        crates/bevy_synth_ui/src/sections/drums.rs
git commit -F - <<'MSG'
Turn the effects into a return bus and put compressors on the inserts

The delay and reverb were an insert on the synth, and the only way to
get drums into them was `drum_to_fx`, a bool that chose which side of
`process_block` the drum sum happened on. That is a routing switch with
two positions and no middle, and it left the drums with no way to be
half-wet.

They are now a return fed by two sends. The chain still runs as an
insert internally, so the return contributes `chain(send) - send`: the
wet alone, and exactly `+0.0` when both mixes are down. That is what
keeps the golden vector where it is and every existing patch sounding
the same at `synth_send = 1.0`.

Two compressors go in with it -- one on the synth before the send tap,
one across the master sum -- each able to key off the drum bus or pad 0.
`drum_to_fx` is gone; `drum_send` at 0.0 is where it used to sit.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
```

---

### Task 5: Gain-reduction telemetry

A compressor with no meter is a knob you turn until it sounds wrong. Two
atomics carry each compressor's current gain reduction out to the UI, on the
same road `output_peak` already travels.

Unlike `output_peak`, this is **last value, not peak-hold**. A GR meter is
meant to fall back when the compressor lets go; a held maximum would stick at
the loudest moment of the session and stop telling you anything.

**Files:**
- Modify: `crates/synth_core/src/params.rs` — two telemetry atomics
  (declaration ~`:699`, `from_params` ~`:821`)
- Modify: `crates/synth_core/src/engine.rs` — `publish_telemetry` (~`:748`)
- Modify: `crates/bevy_synth/src/lib.rs` — `SynthTelemetry` (~`:336`),
  `read_telemetry` (~`:350`)
- Test: `crates/synth_core/src/engine.rs`

**Interfaces:**
- Consumes (Task 2): `Compressor::gain_reduction_db(&self) -> f32`.
- Consumes (Task 4): `Engine::comp_synth`, `Engine::comp_master`.
- Produces (Task 6 reads them): `SharedParams::comp_synth_gr: AtomicF32`,
  `SharedParams::comp_master_gr: AtomicF32`, both holding a **positive** dB
  count of reduction; `SynthTelemetry::comp_synth_gr: f32`,
  `SynthTelemetry::comp_master_gr: f32`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `engine.rs`:

```rust
    /// The meter has to agree with the gain actually applied. Drive a
    /// compressed engine hard, then check the reported reduction against the
    /// difference the compressor made to the peak.
    #[test]
    fn the_meter_reports_the_reduction_the_compressor_applied() {
        let squash = |p: &SharedParams| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
            p.comp_master.on.set(true);
            p.comp_master.threshold_db.set(-24.0);
            p.comp_master.ratio.set(8.0);
            p.comp_master.attack_ms.set(0.1);
        };
        let (mut squashed, tx_squashed) = engine_preset(squash);
        let (mut open, tx_open) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.master_gain.set(1.0);
        });
        for tx in [&tx_squashed, &tx_open] {
            tx.push(Event::NoteOn { note: 60, velocity: 1.0 });
        }

        // Long enough for the attack to settle at the steady-state reduction.
        let squashed_out = render(&mut squashed, 8_192);
        let open_out = render(&mut open, 8_192);
        assert!(peak(&open_out) > 0.0, "the synth never sounded");

        let reported = squashed.params.comp_master_gr.get();
        assert!(reported > 0.0, "the meter reported no reduction");

        let measured = 20.0 * (peak(&open_out) / peak(&squashed_out)).log10();
        assert!(
            (reported - measured).abs() < 3.0,
            "meter says {reported} dB, the output moved {measured} dB"
        );
    }

    /// A bypassed compressor reads zero, so the meter empties rather than
    /// freezing at whatever it last saw.
    #[test]
    fn the_meter_empties_when_the_compressor_is_off() {
        let (mut engine, tx) = engine_preset(|p| {
            p.seq_playing.set(false);
            p.comp_master.on.set(true);
            p.comp_master.threshold_db.set(-40.0);
        });
        tx.push(Event::NoteOn { note: 60, velocity: 1.0 });
        render(&mut engine, 4_096);
        assert!(engine.params.comp_master_gr.get() > 0.0, "never compressed");

        engine.params.comp_master.on.set(false);
        render(&mut engine, 4_096);
        assert_eq!(engine.params.comp_master_gr.get(), 0.0);
    }
```

The tolerance is 3 dB because the two figures are not the same measurement:
the meter reports the reduction at the last sample of the block, while
`peak` compares the loudest sample of each run, and the synth's own envelope
moves underneath both. A tighter bound would fail on a change to the
oscillator, which is not what this test is for.

`engine.params` is the `Arc<SharedParams>` the engine holds; `engine_preset`
returns only the engine and the producer, so read it back through the engine
rather than keeping a second handle.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p synth_core --lib engine::tests::the_meter`
Expected: FAIL to compile — `no field 'comp_master_gr' on type 'SharedParams'`.

- [ ] **Step 3: Add the atomics**

`crates/synth_core/src/params.rs`, in the telemetry block after
`output_peak` (~`:699`):

```rust
    /// Gain reduction each compressor is currently applying, in positive dB.
    /// Last value rather than a held peak: a GR meter that never falls back
    /// tells you nothing about what the compressor is doing now.
    pub comp_synth_gr: AtomicF32,
    pub comp_master_gr: AtomicF32,
```

and in `from_params` after `output_peak: AtomicF32::new(0.0),` (~`:821`):

```rust
            comp_synth_gr: AtomicF32::new(0.0),
            comp_master_gr: AtomicF32::new(0.0),
```

Nothing goes into `snapshot` or `apply`. These are telemetry: the audio
thread writes them and the control thread reads them, the opposite direction
from every parameter, and `Params` has no field for them.

- [ ] **Step 4: Publish them**

`crates/synth_core/src/engine.rs`, at the end of `publish_telemetry`, after
the `output_peak` block:

```rust
        // Straight set, no `max`: the meter should follow the compressor down
        // as it releases.
        self.params
            .comp_synth_gr
            .set(self.comp_synth.gain_reduction_db());
        self.params
            .comp_master_gr
            .set(self.comp_master.gain_reduction_db());
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p synth_core --lib engine::tests::the_meter`
Expected: PASS.

- [ ] **Step 6: Carry them into the Bevy resource**

`crates/bevy_synth/src/lib.rs`, in `SynthTelemetry` after `peak`:

```rust
    /// Gain reduction the synth-bus compressor is applying, in positive dB.
    pub comp_synth_gr: f32,
    /// Gain reduction the master compressor is applying, in positive dB.
    pub comp_master_gr: f32,
```

and in `read_telemetry`, after the `take_peak` line:

```rust
    // Plain reads, not `take_*`: these are levels, not accumulators, so
    // there is nothing to reset.
    telemetry.comp_synth_gr = synth.params.comp_synth_gr.get();
    telemetry.comp_master_gr = synth.params.comp_master_gr.get();
```

`SynthTelemetry` derives `Default`, so the new fields need no other change.

- [ ] **Step 7: Run the whole suite**

Run: `cargo test --workspace`
Expected: PASS, golden vector included — telemetry does not touch the audio
path.

- [ ] **Step 8: Check clippy against the baseline**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"`
Expected: the 28-warning baseline, unchanged. Ignore `(N duplicates)` lines.

- [ ] **Step 9: Commit**

```bash
git add crates/synth_core/src/params.rs crates/synth_core/src/engine.rs \
        crates/bevy_synth/src/lib.rs
git commit -F - <<'MSG'
Report each compressor's gain reduction as telemetry

A compressor you cannot see is a threshold knob you turn until
something sounds wrong. Both compressors now publish the reduction
they are applying, in positive dB, on the road `output_peak` already
travels.

It is the last value rather than a held peak, unlike the output meter:
a GR reading that never falls back would sit at the loudest moment of
the session and stop describing the present.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
```

---

### Task 6: The panel

Everything the previous five tasks added is currently unreachable from the
window. This task puts the knobs on it: a COMPRESSOR section in the FX
column, Pan and Send on the selected pad, and Send on the synth and drum
strips of the mixer.

The `drum_to_fx` checkbox is already gone (Task 4). `drum_send` at 0.0 is
where it stood, and the knob added here is what replaces it.

**Files:**
- Create: `crates/bevy_synth_ui/src/sections/compressor.rs`
- Modify: `crates/bevy_synth_ui/src/sections/mod.rs` — declare and re-export
- Modify: `crates/bevy_synth_ui/src/lib.rs` — `ui_enum!` for
  `SidechainSource` (~`:202`), the FX column and `synth_columns`' signature
  (~`:361`), its call site (~`:324`)
- Modify: `crates/bevy_synth_ui/src/sections/drums.rs:117-153` — two knobs on
  the pad row
- Modify: `crates/bevy_synth_ui/src/sections/mixer.rs` — a Send knob in the
  synth strip and one in the drums strip
- Test: `crates/bevy_synth_ui/src/sections/compressor.rs`

**Interfaces:**
- Consumes (Task 1): `SharedParams::comp_synth`, `comp_master` (each a
  `SharedCompressor` with `on: AtomicBool32`, `threshold_db`, `ratio`,
  `attack_ms`, `release_ms`, `makeup_db`, `sidechain: AtomicEnum`),
  `pad_pan`, `pad_send`, `drum_send`, `synth_send`; `SidechainSource::ALL`,
  `SidechainSource::from_u32`, `SidechainSource::name`.
- Consumes (Task 5): `SynthTelemetry::comp_synth_gr`,
  `SynthTelemetry::comp_master_gr`.
- Produces: `sections::compressor_section(&mut Ui, &Synth, &SynthTelemetry)`.

- [ ] **Step 1: Teach the UI about `SidechainSource`**

`crates/bevy_synth_ui/src/lib.rs`. Extend the `synth_core::params` import at
`:39`:

```rust
use synth_core::params::{AtomicEnum, ClockSource, SidechainSource, VoiceMode};
```

and add to the `ui_enum!` block after the `NoteDivision` line (`:201`):

```rust
ui_enum!(SidechainSource, &SidechainSource::ALL, |s: SidechainSource| s
    .name());
```

That is all `selector::<SidechainSource>` needs: three options, so the button
row rather than a dropdown.

- [ ] **Step 2: Write the failing test**

The section itself draws; there is nothing in it worth a headless egui
harness. What *is* worth pinning is the one piece of logic the section
carries — turning a gain-reduction figure into the label it shows — because
that is where a sign error would silently invert the meter.

Create `crates/bevy_synth_ui/src/sections/compressor.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_reduction_reads_as_a_dash_rather_than_zero_decibels() {
        // "0.0 dB" and "not compressing" look the same at a glance and mean
        // different things; only one of them should be able to appear.
        assert_eq!(gain_reduction_label(0.0), "--");
    }

    #[test]
    fn reduction_reads_as_negative_decibels() {
        // The compressor reports a positive count of dB removed; a meter
        // showing "4.5 dB" reads as a boost.
        assert_eq!(gain_reduction_label(4.5), "-4.5 dB");
    }

    #[test]
    fn a_reduction_too_small_to_see_still_reads_as_a_dash() {
        assert_eq!(gain_reduction_label(0.02), "--");
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p bevy_synth_ui --lib compressor`
Expected: FAIL to compile — `cannot find function 'gain_reduction_label'`.

(The module is not declared in `sections/mod.rs` yet, so it may instead
report nothing to run. Add the `mod compressor;` line from Step 5 first if
so, then re-run and get the missing-function error.)

- [ ] **Step 4: Write the section**

Prepend to `crates/bevy_synth_ui/src/sections/compressor.rs`, above the test
module:

```rust
//! The two compressors: one across the synth bus, one across the master.

use egui::Ui;

use bevy_synth::{Synth, SynthTelemetry};
use synth_core::params::{SharedCompressor, SidechainSource};

use crate::selector;
use crate::widgets::{self, KnobSpec, palette};

/// Both compressors, stacked, in one section.
///
/// They are the same six controls twice, so they share a drawing function and
/// differ only in caption and colour. Keeping them together rather than
/// filing each next to the bus it compresses is deliberate: the decision you
/// are making is how much of the squashing happens per-instrument and how
/// much on the sum, and that is a comparison, not two settings.
pub(crate) fn compressor_section(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry) {
    let p = &synth.params;
    widgets::section(ui, "COMPRESSOR", palette::FX, |ui| {
        one(ui, "synth bus", palette::OSC, &p.comp_synth, telemetry.comp_synth_gr);
        ui.add_space(6.0);
        ui.separator();
        one(ui, "master", palette::ACCENT, &p.comp_master, telemetry.comp_master_gr);
    });
}

/// One compressor: a bypass, six knobs' worth of settings and a GR readout.
fn one(ui: &mut Ui, name: &str, colour: egui::Color32, c: &SharedCompressor, gr: f32) {
    ui.horizontal(|ui| {
        let mut on = c.on.get();
        if ui.checkbox(&mut on, name).changed() {
            c.on.set(on);
        }

        // A number, not a bar. The reading that matters is "how many dB am I
        // taking off", and a meter that only moves is harder to answer that
        // with than the figure itself.
        ui.label(
            egui::RichText::new(gain_reduction_label(gr))
                .color(palette::TEXT_DIM)
                .size(10.0)
                .monospace(),
        )
        .on_hover_text("gain reduction being applied right now");
    });

    ui.horizontal(|ui| {
        widgets::knob_param(
            ui,
            &KnobSpec::new("Thresh", -60.0..=0.0)
                .colour(colour)
                .unit("dB")
                .default(-12.0)
                .size(36.0),
            &c.threshold_db,
        )
        .on_hover_text("level above which the compressor starts working");
        widgets::knob_param(
            ui,
            &KnobSpec::new("Ratio", 1.0..=20.0)
                .log()
                .colour(colour)
                .unit(":1")
                .default(4.0)
                .size(36.0),
            &c.ratio,
        );
        widgets::knob_param(
            ui,
            &KnobSpec::new("Makeup", 0.0..=24.0)
                .colour(colour)
                .unit("dB")
                .default(0.0)
                .size(36.0),
            &c.makeup_db,
        )
        .on_hover_text("gain added after the compressor, to put back what it took");
    });

    ui.horizontal(|ui| {
        widgets::knob_param(
            ui,
            &KnobSpec::new("Attack", 0.1..=100.0)
                .log()
                .colour(colour)
                .unit("ms")
                .default(10.0)
                .size(36.0),
            &c.attack_ms,
        );
        widgets::knob_param(
            ui,
            &KnobSpec::new("Release", 5.0..=1000.0)
                .log()
                .colour(colour)
                .unit("ms")
                .default(100.0)
                .size(36.0),
            &c.release_ms,
        );
    });

    ui.label(
        egui::RichText::new("Sidechain")
            .color(palette::TEXT_DIM)
            .size(10.0),
    );
    selector::<SidechainSource>(ui, &c.sidechain);
}

/// The GR readout.
///
/// The compressor reports a positive count of decibels removed; the meter
/// shows it as the negative number a level meter would, and shows nothing at
/// all rather than a rounded "0.0 dB" when it is not working.
fn gain_reduction_label(gr_db: f32) -> String {
    if gr_db < 0.05 {
        "--".to_string()
    } else {
        format!("-{gr_db:.1} dB")
    }
}
```

`selector` and `widgets` are `pub(crate)` in `lib.rs` already — the other
sections reach them the same way. If `selector` is declared plain `fn`
rather than `pub(crate) fn`, leave it: `sections` is a child module of the
crate root, so a private item at the root is in scope for it.

- [ ] **Step 5: Wire the module up**

`crates/bevy_synth_ui/src/sections/mod.rs`, in the alphabetical lists:

```rust
mod compressor;
```
(after `mod delay;`? no — before it, the list is alphabetical: `compressor`
sorts above `delay`.)

```rust
pub(crate) use compressor::compressor_section;
```
(likewise first in the `pub(crate) use` list.)

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p bevy_synth_ui --lib compressor`
Expected: PASS, three tests.

- [ ] **Step 7: Put the section in the FX column**

`crates/bevy_synth_ui/src/lib.rs`. `compressor_section` needs telemetry and
`synth_columns` does not currently have it, so thread it through. At the call
site (~`:324`), `telemetry` is already in scope — the neighbouring
`sections::mixer` and `sections::drums` calls both take it:

```rust
                        synth_columns(ui, &synth, &telemetry, visible_width);
```

and the signature (~`:361`):

```rust
fn synth_columns(ui: &mut Ui, synth: &Synth, telemetry: &SynthTelemetry, width: f32) {
```

Then extend the fourth column:

```rust
        &|ui| {
            sections::delay_section(ui, synth);
            sections::reverb_section(ui, synth);
            sections::compressor_section(ui, synth, telemetry);
        },
```

The column array is `[&dyn Fn(&mut Ui); 4]` and stays four wide: the
compressor joins the existing FX column rather than opening a fifth, because
`per_row` is computed from the window width and a fifth column would push the
layout to two rows on any window narrower than about 1325 px.

- [ ] **Step 8: Add Pan and Send to the pad row**

`crates/bevy_synth_ui/src/sections/drums.rs`, in the `ui.horizontal` that
draws the selected pad's knobs, after the Decay knob (~`:153`):

```rust
            widgets::knob_param(
                ui,
                &KnobSpec::new("Pan", -1.0..=1.0)
                    .colour(palette::DRUM)
                    .default(0.0)
                    .size(36.0),
                &p.pad_pan[pad],
            )
            .on_hover_text("hard left to hard right; centre is unity in both channels");
            widgets::knob_param(
                ui,
                &KnobSpec::new("Send", 0.0..=1.0)
                    .colour(palette::FX)
                    .default(1.0)
                    .size(36.0),
                &p.pad_send[pad],
            )
            .on_hover_text("how much of this pad reaches the effects, under the rack's Send");
```

Five knobs on the row rather than three. The pad row is inside the DRUMS
section, which owns a full tab rather than a 265 px column, so there is room.

- [ ] **Step 9: Add the bus sends to the mixer**

`crates/bevy_synth_ui/src/sections/mixer.rs`. In the `synth` strip, after
Drive:

```rust
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Send", 0.0..=1.0)
                        .colour(palette::FX)
                        .default(1.0)
                        .size(36.0),
                    &p.synth_send,
                )
                .on_hover_text("how much of the synth bus reaches delay and reverb");
```

and in the `drums` strip, after Bus:

```rust
                widgets::knob_param(
                    ui,
                    &KnobSpec::new("Send", 0.0..=1.0)
                        .colour(palette::FX)
                        .default(0.0)
                        .size(36.0),
                    &p.drum_send,
                )
                .on_hover_text("how much of the drum bus reaches delay and reverb");
```

Both are `palette::FX` rather than their strip's colour, because what they
have in common is the destination, not the source.

- [ ] **Step 10: Run the whole suite**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 11: Check clippy against the baseline**

Run: `cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning: .* generated"`
Expected: the 28-warning baseline, unchanged. Ignore `(N duplicates)` lines.

- [ ] **Step 12: Look at it**

Run: `cargo run --release`

Check by hand, because none of it is under test:
- COMPRESSOR appears under REVERB in the fourth column, both halves drawn.
- Turning the master compressor on with a low threshold makes the GR readout
  move and fall back to `--` when the sound stops.
- The selected pad has five knobs; Pan moves the pad across the stereo field.
- The synth strip's Send at 0.0 kills the reverb tail on the synth; the drum
  strip's Send at 1.0 puts the rack into it.

- [ ] **Step 13: Commit**

```bash
git add crates/bevy_synth_ui/src/sections/compressor.rs \
        crates/bevy_synth_ui/src/sections/mod.rs \
        crates/bevy_synth_ui/src/lib.rs \
        crates/bevy_synth_ui/src/sections/drums.rs \
        crates/bevy_synth_ui/src/sections/mixer.rs
git commit -F - <<'MSG'
Put the sends, the pan and the compressors on the panel

Five tasks of routing with no way to reach it. The FX column gains a
COMPRESSOR section -- both compressors together, because the decision
being made is how much squashing happens per-instrument versus on the
sum, and that is a comparison rather than two settings. Each shows the
reduction it is applying as a number: a bar answers "is it working",
and the question is "by how much".

The selected pad gains Pan and Send, and the mixer's synth and drum
strips gain a Send apiece. Those two knobs are where the `Through FX`
checkbox used to be, with every position in between it never had.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
MSG
```
