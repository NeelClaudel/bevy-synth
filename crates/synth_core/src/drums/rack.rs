//! The rack: eight voices, one grid, a stereo bus pair.
//!
//! The rack is where a hit becomes a sound. It owns the sequencer and the
//! voices, applies the per-pad controls the sequencer knows nothing about
//! (level, tuning, decay scaling, mute), resolves choke groups, and sums the
//! result into a scratch buffer the engine can mix.
//!
//! [`DrumRack::render`] returns whether it produced anything. That boolean is
//! the hard bypass: with an empty grid and no pad ringing the engine skips the
//! drum bus entirely, which is what keeps the golden vector byte-identical for
//! a patch that never touches the drums.

use super::sequencer::{DrumOutput, DrumSequencer};
use super::{DrumVoice, Pad, PAD_COUNT};
use crate::clock::{Advance, ClockView};
use crate::params::Params;
use crate::BLOCK;

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

pub struct DrumRack {
    sequencer: DrumSequencer,
    voices: [DrumVoice; PAD_COUNT],
    out: DrumBuses,
    sample_rate: f32,
}

impl DrumRack {
    pub fn new(sample_rate: f32, seed: u64) -> Self {
        Self {
            sequencer: DrumSequencer::new(),
            // Each voice gets its own stream, or eight noise pads would draw
            // identical numbers and sum into one loud correlated hiss.
            voices: core::array::from_fn(|i| {
                DrumVoice::new(sample_rate, seed ^ (0x9E37_79B9 * (i as u64 + 1)))
            }),
            out: DrumBuses::SILENT,
            sample_rate: sample_rate.max(1.0),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for voice in &mut self.voices {
            voice.set_sample_rate(self.sample_rate);
        }
    }

    pub fn sequencer(&mut self) -> &mut DrumSequencer {
        &mut self.sequencer
    }

    pub fn sequencer_ref(&self) -> &DrumSequencer {
        &self.sequencer
    }

    pub fn silence(&mut self) {
        for voice in &mut self.voices {
            voice.silence();
        }
        self.out = DrumBuses::SILENT;
    }

    /// The external-clock path. The engine calls this from its MIDI tick
    /// handler; the strikes land on the next `render`.
    pub fn on_tick(&mut self, ticked: bool, clock: ClockView, p: &Params) {
        let out = self.sequencer.on_tick(ticked, clock, p);
        if !p.drum_enabled {
            // Same rule as the disabled path in `render`: a mute stops the
            // events, not the playhead. This path carries it alone under an
            // external clock, where `Clock::advance` reports zero steps and
            // `render` therefore never advances anything — returning early
            // here would freeze a muted rack against a melodic track that
            // keeps moving. `render` still silences the voices and bypasses
            // the bus, so dropping the strikes is all the muting this needs.
            return;
        }
        self.strike(out, p);
    }

    /// Fill the scratch buffer. Returns false when there was nothing to make —
    /// the engine then skips the drum bus altogether.
    pub fn render(&mut self, frames: usize, adv: Advance, clock: ClockView, p: &Params) -> bool {
        let frames = frames.min(BLOCK);
        if !p.drum_enabled {
            // Sequence the block and throw the hits away. A mute stops the
            // events, not the playhead: the spec asks that unmuting four bars
            // later come back in time rather than at the top, and the melodic
            // track keeps advancing while muted, so a rack that froze here
            // would be permanently out of phase with it afterwards. This also
            // keeps tracking the length knob — the grid mirror the UI draws
            // from is published whether or not the rack is switched on, and a
            // mirror stuck at length zero would show an empty machine.
            let _ = self.sequencer.advance(frames, adv, clock, p);
            // A pad struck just before the rack was disabled would otherwise
            // sit frozen mid-decay — `voice.next()` is never called while
            // disabled, so the envelope doesn't advance — and resume that
            // stale tail the moment the rack is re-enabled. Silencing here
            // closes that gap for the enable toggle specifically; the
            // transport-stop sites already do this on their own paths.
            for voice in &mut self.voices {
                voice.silence();
            }
            self.out.clear(frames);
            return false;
        }

        let hits = self.sequencer.advance(frames, adv, clock, p);
        self.strike(hits, p);

        // Nothing struck and nothing ringing: zero the scratch so a stale tail
        // can never be read back, and tell the engine not to bother mixing.
        if self.voices.iter().all(|v| v.is_silent()) {
            self.out.clear(frames);
            return false;
        }

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
    }

    /// The block just rendered. Valid up to `BLOCK` frames; the engine only
    /// reads the `count` it asked for.
    pub fn buses(&self) -> &DrumBuses {
        &self.out
    }

    /// Turn a column of velocities into strikes, applying the per-pad controls
    /// and resolving choke groups.
    fn strike(&mut self, out: DrumOutput, p: &Params) {
        for index in 0..PAD_COUNT {
            let velocity = out.hits[index];
            if velocity <= 0.0 || p.pad_mute[index] {
                continue;
            }
            let pad = Pad::ALL[index];

            // A hat cuts its group short before it sounds. `voice.pad()` is
            // what makes this read the voice rather than assume the index —
            // an untouched voice still reports `Kick`, whose group is `None`,
            // so nothing is choked before its first strike. A column that
            // hits both hats leaves the later pad ringing: deterministic, and
            // not a pattern anyone writes on purpose.
            if let Some(group) = pad.choke_group() {
                for (other, voice) in self.voices.iter_mut().enumerate() {
                    if other != index && voice.pad().choke_group() == Some(group) {
                        voice.silence();
                    }
                }
            }

            self.voices[index].strike(
                pad,
                velocity * p.pad_level[index],
                p.pad_tune[index],
                p.pad_decay[index],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::drums::Cell;
    use crate::params::{ClockSource, Params};
    use crate::BLOCK;

    /// One block of the kick, taken from the mono rack before the stereo
    /// rewrite. Centre panning has to land on these numbers exactly, or the
    /// rewrite changed every existing patch.
    const MONO_REFERENCE: [f32; 8] = [
        -0.17237629,
        0.043552846,
        0.29023832,
        -0.06265369,
        -0.01962061,
        0.24162598,
        0.23737402,
        -0.060975004,
    ];

    fn setup(p: &Params) -> (DrumRack, Clock) {
        let mut c = Clock::new(48_000.0);
        c.set_tempo(p.tempo, p.steps_per_beat);
        c.start();
        (DrumRack::new(48_000.0, 0x51D), c)
    }

    fn peak_of(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    fn render(r: &mut DrumRack, c: &mut Clock, p: &Params) -> (bool, f32) {
        let adv = c.advance(BLOCK, ClockSource::Internal);
        let sounded = r.render(BLOCK, adv, c.view(), p);
        let bus = r.buses();
        let peak = peak_of(&bus.dry_l).max(peak_of(&bus.dry_r));
        (sounded, peak)
    }

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

    /// The whole reason the golden vector survives: an empty rack does not
    /// merely render silence, it declines to render at all.
    #[test]
    fn an_empty_rack_reports_nothing_to_mix() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        for _ in 0..64 {
            let (sounded, peak) = render(&mut rack, &mut clock, &p);
            assert!(!sounded);
            assert_eq!(peak, 0.0);
        }
    }

    /// One hit on the grid and the rack starts producing audio.
    #[test]
    fn a_grid_with_a_hit_makes_sound() {
        let p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });

        let mut peak = 0.0f32;
        for _ in 0..200 {
            let (_, block_peak) = render(&mut rack, &mut clock, &p);
            peak = peak.max(block_peak);
        }
        assert!(peak > 0.05, "the kick never reached the bus: {peak}");
    }

    /// Mute is per pad and lives in the rack, not the grid: the pattern keeps
    /// its hits, they just do not strike anything.
    #[test]
    fn a_muted_pad_stays_silent() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        p.pad_mute[0] = true;
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });

        for _ in 0..200 {
            let (sounded, peak) = render(&mut rack, &mut clock, &p);
            assert!(!sounded);
            assert_eq!(peak, 0.0);
        }
    }

    /// The choke group is what makes a hat pair sound like one hat: closing it
    /// has to cut the open one short.
    #[test]
    fn a_closed_hat_chokes_the_open_one() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        // Open hat on 0, closed hat two steps later.
        rack.sequencer()
            .set_cell(0, 3, Cell { active: true, velocity: 1.0 });
        rack.sequencer()
            .set_cell(2, 2, Cell { active: true, velocity: 1.0 });
        p.drum_length = 16;

        // Measure the tail well after the closed hat has itself decayed
        // (45 ms) but while a free-running open hat (380 ms) would still ring.
        let mut choked_tail = 0.0f32;
        let mut steps = 0;
        for _ in 0..4_000 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            let stepped = adv.steps > 0;
            rack.render(BLOCK, adv, clock.view(), &p);
            if stepped {
                steps += 1;
            }
            // Between step 3 and step 4: past the closed hat's own decay.
            if steps == 4 {
                let peak = peak_of(&rack.buses().dry_l);
                choked_tail = choked_tail.max(peak);
            }
        }

        // Now the same pattern with the closed hat removed.
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 3, Cell { active: true, velocity: 1.0 });
        let mut free_tail = 0.0f32;
        let mut steps = 0;
        for _ in 0..4_000 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            let stepped = adv.steps > 0;
            rack.render(BLOCK, adv, clock.view(), &p);
            if stepped {
                steps += 1;
            }
            if steps == 4 {
                let peak = peak_of(&rack.buses().dry_l);
                free_tail = free_tail.max(peak);
            }
        }

        assert!(free_tail > 0.0, "the open hat never rang at all");
        assert!(
            choked_tail < free_tail * 0.5,
            "the closed hat did not choke the open one: {choked_tail} vs {free_tail}"
        );
    }

    /// Commissioned alongside Task 8's enable checkbox: `drum_enabled` can now
    /// flip independently of the transport, and `render`'s early-return
    /// skips `voice.next()` entirely — so without an explicit silence there,
    /// a pad struck moments before being disabled would sit frozen
    /// mid-decay and pick its old tail back up the instant the rack came
    /// back on.
    #[test]
    fn disabling_mid_decay_does_not_resume_the_old_tail() {
        let mut p = Params { drum_enabled: true, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        // The open hat rings the longest (~380 ms), so there is plenty of
        // tail left to leak back out if the fix is missing.
        rack.sequencer()
            .set_cell(0, 3, Cell { active: true, velocity: 1.0 });

        // Run until the strike actually lands and the voice is audibly
        // ringing.
        let mut rang = false;
        for _ in 0..250 {
            let (_, peak) = render(&mut rack, &mut clock, &p);
            if peak > 0.0 {
                rang = true;
                break;
            }
        }
        assert!(rang, "the open hat never sounded before being disabled");

        // Disable mid-decay: the voice is still ringing at this instant.
        p.drum_enabled = false;
        let (sounded, peak) = render(&mut rack, &mut clock, &p);
        assert!(!sounded);
        assert_eq!(peak, 0.0);

        // Re-enable with nothing new landing on the grid — the next step is
        // ~187 blocks away at this tempo, so this stays well inside step 0 —
        // and confirm the old tail does not pick back up.
        p.drum_enabled = true;
        for _ in 0..20 {
            let (_, peak) = render(&mut rack, &mut clock, &p);
            assert_eq!(peak, 0.0, "the old tail resumed after re-enabling");
        }
    }

    /// The spec is explicit about what a mute is
    /// (`docs/superpowers/specs/2026-09-09-drum-rack-design.md`): "A mute
    /// stops a sequencer producing events; it does not stop the clock or reset
    /// the position. Muting a track and unmuting it four bars later comes back
    /// in time rather than at the top." A rack that stopped advancing while
    /// disabled came back however many steps behind it had been muted for, and
    /// stayed permanently out of phase with the melodic track — which does keep
    /// advancing — for the rest of the session.
    #[test]
    fn muting_the_rack_and_coming_back_lands_in_time() {
        let enabled = Params { drum_enabled: true, drum_length: 16, ..Default::default() };
        let (mut reference, _) = setup(&enabled);
        let (mut muted, mut clock) = setup(&enabled);

        // Both racks read the same advance from the same clock, exactly as the
        // engine drives one; only `drum_enabled` differs between them.
        let mut p = enabled;
        let mut steps = 0usize;
        for _ in 0..12_000 {
            let adv = clock.advance(BLOCK, ClockSource::Internal);
            steps += adv.steps as usize;
            // Muted across steps 2..10 — most of a bar, long enough that a
            // frozen playhead is unmistakable.
            p.drum_enabled = !(2..10).contains(&steps);
            reference.render(BLOCK, adv, clock.view(), &enabled);
            muted.render(BLOCK, adv, clock.view(), &p);
        }

        assert!(steps > 12, "the clock never ran past the mute: {steps}");
        assert_eq!(
            reference.sequencer_ref().position(),
            (steps - 1) % 16,
            "the reference rack is not where an always-on rack should be"
        );
        assert_eq!(
            muted.sequencer_ref().position(),
            reference.sequencer_ref().position(),
            "the muted rack came back out of time"
        );
    }

    /// Under an external clock nothing steps through `render` — `Clock::advance`
    /// reports zero steps for `ClockSource::ExternalMidi` — so the mute rule has
    /// to hold on the tick path too. A rack muted while slaved to a DAW would
    /// otherwise come back permanently behind the melody.
    #[test]
    fn muting_the_rack_under_an_external_clock_lands_in_time() {
        let enabled = Params {
            drum_enabled: true,
            drum_length: 16,
            clock_source: ClockSource::ExternalMidi,
            ..Default::default()
        };
        let (mut reference, _) = setup(&enabled);
        let (mut muted, mut clock) = setup(&enabled);

        let mut p = enabled;
        for step in 0..24usize {
            // Muted across steps 2..10, as in the internal-clock twin.
            p.drum_enabled = !(2..10).contains(&step);
            let adv = clock.advance(BLOCK, ClockSource::ExternalMidi);
            reference.on_tick(true, clock.view(), &enabled);
            muted.on_tick(true, clock.view(), &p);
            reference.render(BLOCK, adv, clock.view(), &enabled);
            muted.render(BLOCK, adv, clock.view(), &p);
        }

        assert_eq!(
            reference.sequencer_ref().position(),
            23 % 16,
            "the reference rack is not where an always-on rack should be"
        );
        assert_eq!(
            muted.sequencer_ref().position(),
            reference.sequencer_ref().position(),
            "the muted rack came back out of time"
        );
    }

    /// The enable switch is checked in the rack, so a disabled rack costs one
    /// branch per block and nothing else.
    #[test]
    fn a_disabled_rack_never_renders() {
        let p = Params { drum_enabled: false, ..Default::default() };
        let (mut rack, mut clock) = setup(&p);
        rack.sequencer()
            .set_cell(0, 0, Cell { active: true, velocity: 1.0 });

        for _ in 0..200 {
            let (sounded, peak) = render(&mut rack, &mut clock, &p);
            assert!(!sounded);
            assert_eq!(peak, 0.0);
        }
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
}
