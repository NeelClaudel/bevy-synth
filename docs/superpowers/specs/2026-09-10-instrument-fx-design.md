# Instrument FX, stereo drums and sidechain compression

Date: 2026-09-10
Status: approved design, not yet implemented

## The problem

Four requests that turned out to be one:

1. The drum rack is mono.
2. `drum_to_fx` is a bool where a float belongs.
3. Effects serve the synth only; they should be reachable per instrument,
   "kind of like in Ableton".
4. There is no compressor, and no way to sidechain one.

They are not independent. Where the effects live decides what stereo and
sidechain mean, so the bus topology has to be settled first and the rest
follows from it.

## What exists today

```
voices -> drive -> DC block -> synth_gain --+
                                            +-> FX (delay -> reverb) --+
drums (MONO) -> drum_gain ------------------+   ..or here, if !drum_to_fx
                                                                       +-> master_gain -> out
```

Three facts constrain the design:

- `drum_to_fx` is implemented as *which side of `fx.process_block` the
  identical mono sum happens on*. It is a routing switch with two positions
  and no middle.
- `drums/rack.rs` is mono deliberately -- `buffer: [f32; BLOCK]`, commented
  "The engine pans it; the rack only sums." The engine never pans it. It
  duplicates it into both channels.
- `FxChain::new` allocates roughly 850 KB (a 2 s stereo delay plus a ~21 k
  sample reverb tank). Duplicating one per bus is affordable in memory. The
  expensive part is not memory, it is the parameter and UI surface.

## Chosen topology: return bus, cheap inserts

Delay and reverb become a **send/return**. The compressor is an **insert**,
instantiated per bus.

```
drums -> per-pad pan -> stereo dry ------------------------+
      \-> per-pad send * drum_send -> stereo send --+      |
                                                    |      |
synth -> drive -> DC -> synth_gain -> [comp] --+----|------+-> [master comp]
                                               |    |         -> master_gain -> out
                                    synth_send |    |                  ^
                                               v    v                  |
                                         RETURN: delay -> reverb ------+
                                              (contributes wet only)
```

Rejected alternatives:

- **Per-bus insert chains** (two `FxChain`s). Buys independent reverb
  *settings* at the cost of doubling the delay and reverb parameter surface
  and the panel that drives it, for a control most people set once.
- **Minimal** (stereo drums, a `drum_send` float, one master compressor).
  Skips per-instrument effects entirely, which was the request.

The return gives independent reverb *amounts* per instrument for free, and
per-pad sends on top, which the per-bus-insert design does not provide
either. Reverb on a return is also how sessions in Ableton are actually
built.

## The return contributes wet only

`FxChain` is an insert: at `delay_mix = reverb_mix = 0.0` it is a bit-exact
pass-through. Summing its output into a master that already carries the dry
signal would therefore double the dry.

The return contributes the difference the chain made:

```
return = chain(send) - send
```

This has three consequences worth stating plainly.

- At `mix = 0.0` the chain returns its input unchanged, so `return` is
  exactly `0.0` and adds nothing. **The golden vector is untouched.**
- At `synth_send = 1.0` the master sum is `dry + (chain(dry) - dry)`, which
  is today's insert output. **No existing patch changes**, at any mix value.
  Float addition is not associative, so results differ from today by roughly
  one part in 10^7 once a mix is raised. Nothing asserts on that.
- `delay.rs` and `reverb.rs` need no changes at all.

## Stereo drums

`DrumRack` grows from one mono scratch buffer to four, plus a detector tap:

| Buffer | Contents |
| --- | --- |
| `dry_l`, `dry_r` | Summed pads, per-pad level and pan applied |
| `send_l`, `send_r` | The same, additionally scaled by `pad_send[i]` |
| `kick` | Pad 0 alone, post-level, pre-pan, mono |

Per-pad sends cannot be derived from the summed output, which is why the
send pair is rendered rather than computed downstream. `drum_send` scales
the whole send pair, so `pad_send` defaults of `1.0` make `drum_send` behave
exactly like the old `drum_to_fx` bool.

The `kick` tap exists so the sidechain detector can follow one pad. It is
post-level because a quieter kick should duck less.

### Pan law

`pan` runs -1.0 to 1.0 and must be **unity at centre**, because centre has to
reproduce today's behaviour bit for bit -- the mono sum written to both
channels at gain 1.0.

```
l = min(1.0, 1.0 - pan)
r = min(1.0, 1.0 + pan)
```

The trade-off is deliberate and worth naming: this is constant amplitude in
the surviving channel, not constant power, so a hard-panned pad is about
3 dB quieter in perceived loudness than a centred one. The alternative,
equal-power normalised to unity at centre, pushes the extremes 3 dB *up*,
which is worse. Constant power with unity at the extremes would drop centre
by 3 dB and change every existing patch.

## Compressor

New file `crates/synth_core/src/fx/compressor.rs`. Feed-forward, peak
detector, gain computed in dB with a fixed 6 dB soft knee (no parameter),
attack and release applied to the gain-reduction envelope. State is a
handful of floats -- no delay lines, no allocation, so two instances cost
almost nothing next to one reverb.

Two instances: a **synth bus insert** and a **master glue** compressor. The
synth insert sits before the send tap, so the return hears the compressed
signal.

Each reports its current gain reduction in dB as telemetry, for a GR meter.

### Sidechain source

```rust
#[repr(u32)]
pub enum SidechainSource {
    Off = 0,      // detect on the signal being compressed
    DrumBus = 1,  // mono sum of the dry drum bus
    Kick = 2,     // pad 0 only
}
```

`self.drums.render()` already runs at `engine.rs:302`, above the voice loop,
so this block's drum samples are available to the synth compressor's
detector without reordering anything.

## Parameters

18 parameter names added, one removed. Two of them are per-pad arrays, so
32 individual values. All flat, all fitting the existing
`f32` / `AtomicF32` / `[AtomicF32; PAD_COUNT]` pattern -- no new machinery.

**Removed:** `drum_to_fx: bool`.

**Per pad** (`[f32; PAD_COUNT]`):

| Name | Default | Clamp |
| --- | --- | --- |
| `pad_pan` | 0.0 | -1.0 ..= 1.0 |
| `pad_send` | 1.0 | 0.0 ..= 1.0 |

**Bus:**

| Name | Default | Clamp | Note |
| --- | --- | --- | --- |
| `drum_send` | 0.0 | 0.0 ..= 1.0 | replaces `drum_to_fx: false` |
| `synth_send` | 1.0 | 0.0 ..= 1.0 | preserves today's routing |

**Each compressor**, prefixed `comp_synth_` and `comp_master_`:

| Name | Default | Clamp |
| --- | --- | --- |
| `on` | false | -- |
| `threshold` (dB) | -12.0 | -60.0 ..= 0.0 |
| `ratio` | 4.0 | 1.0 ..= 20.0 |
| `attack` (ms) | 10.0 | 0.1 ..= 100.0 |
| `release` (ms) | 100.0 | 5.0 ..= 1000.0 |
| `makeup` (dB) | 0.0 | 0.0 ..= 24.0 |
| `sidechain` | `Off` | -- |

Every default is chosen so the engine at defaults produces exactly what it
produces today.

## Files touched

| File | Change |
| --- | --- |
| `synth_core/src/fx/compressor.rs` | new |
| `synth_core/src/fx/mod.rs` | export `Compressor` |
| `synth_core/src/drums/rack.rs` | stereo dry and send pairs, kick tap, pan law |
| `synth_core/src/params.rs` | new params, `SidechainSource`, drop `drum_to_fx` |
| `synth_core/src/engine.rs` | bus wiring, return subtract, two compressors |
| `bevy_synth/` | gain-reduction telemetry |
| `bevy_synth_ui/src/sections/` | new `compressor.rs`; pan and send per pad in `drums.rs`; send knobs in `mixer.rs`; drop the `drum_to_fx` toggle |

`fx/delay.rs` and `fx/reverb.rs` are not touched.

## Testing

Every item below is a test to write before the code it covers.

**Must not move:**

- The golden vector. `the_dry_path_matches_the_golden_vector` passes
  unedited. The hash is never re-baselined.

**Return:**

- At `delay_mix = reverb_mix = 0.0` the return output is exactly `0.0`, not
  approximately.
- At `synth_send = 1.0` with a mix raised, the master sum matches today's
  insert output within float epsilon.

**Drums:**

- `pad_pan = 0.0` yields `l == r`, equal to today's mono value exactly.
- `pad_pan = -1.0` yields `r == 0.0` and `l` unchanged.
- `drum_send = 0.0` puts nothing into the return.
- `drum_send = 1.0` with all `pad_send = 1.0` reproduces the old
  `drum_to_fx = true` path.
- `pad_send` differences survive to the return: one pad sent, another not.

**Compressor:**

- Bypassed is a bit-exact pass-through.
- A signal below threshold passes at unity.
- A steady signal above threshold settles at the gain reduction the ratio
  implies.
- Attack and release reach ~63% of their travel in the time constant set.
- `Kick` sidechain ducks a steady synth tone in time with pad 0, and does
  not duck when pad 0 is silent.
- Gain-reduction telemetry matches the gain actually applied.

**Real-time safety:** no allocation, locks or syscalls added to the audio
callback. The only `Vec`s in `fx/` remain inside `#[cfg(test)]`.
