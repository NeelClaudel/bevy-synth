//! MIDI input: hardware keyboard, controllers and clock.
//!
//! # What arrives, and where it goes
//!
//! midir delivers messages on its own OS thread, not the audio thread and not
//! the game thread. Those messages are parsed into [`Event`]s here and pushed
//! straight onto a queue the engine drains, so a keypress reaches the
//! oscillators without waiting for a game frame. Routing MIDI through the ECS
//! instead would add up to a frame of latency — 16 ms, right at the threshold
//! where a keyboard starts to feel unresponsive.
//!
//! # Why the connection lives on its own thread
//!
//! `midir::MidiInputConnection` is `Send` but not `Sync` on ALSA, because the
//! underlying handle contains a raw pointer. A Bevy `Resource` must be
//! `Send + Sync`, so holding one directly in a resource does not compile.
//!
//! The fix mirrors what [`crate::host`] does for the cpal stream: a keeper
//! thread owns the connection and parks, and the handle returned here holds
//! only `Send + Sync` things. Dropping [`MidiInput`] closes the port.
//!
//! # MIDI clock
//!
//! Clock is 24 ticks per quarter note, sent continuously whether or not the
//! transport is running, plus Start (0xFA), Continue (0xFB) and Stop (0xFC).
//! Forwarding all four is what lets the sequencer lock to a DAW or a drum
//! machine — see [`synth_core::clock`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use midir::Ignore;

use synth_core::event::{channel, Consumer, Producer};
use synth_core::{Event, Note};

/// Which MIDI port to open.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PortSelector {
    /// The first port the system reports.
    #[default]
    First,
    /// The first port whose name contains this string, case-insensitively.
    ///
    /// Substring rather than exact match on purpose: port names carry a device
    /// index that changes between plug-ins, so `"Keystep"` keeps working where
    /// the full string would not.
    Matching(String),
    /// A specific index into [`list_ports`].
    Index(usize),
}

#[derive(Debug)]
pub enum MidiError {
    /// No MIDI subsystem at all. Normal on machines without one; not fatal.
    Unavailable,
    NoPorts,
    Connect(String),
}

impl std::fmt::Display for MidiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MidiError::Unavailable => write!(f, "no MIDI subsystem available"),
            MidiError::NoPorts => write!(f, "no matching MIDI input port"),
            MidiError::Connect(e) => write!(f, "could not connect to the MIDI port: {e}"),
        }
    }
}

impl std::error::Error for MidiError {}

/// Names of every MIDI input the system can see, in index order.
///
/// Names rather than port handles: midir's `MidiInputPort` is not `Send`, so a
/// handle could not cross to the keeper thread that has to do the connecting.
/// [`PortSelector`] refers to ports by name or index for the same reason.
pub fn list_ports() -> Result<Vec<String>, MidiError> {
    let input = midir::MidiInput::new("synth-scan").map_err(|_| MidiError::Unavailable)?;
    Ok(input
        .ports()
        .into_iter()
        .map(|port| {
            input
                .port_name(&port)
                .unwrap_or_else(|_| "<unnamed>".to_string())
        })
        .collect())
}

/// A live MIDI input connection.
///
/// Keep it alive for as long as you want MIDI: dropping it closes the port.
pub struct MidiInput {
    pub port_name: String,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MidiInput {
    /// Opens the first available port.
    pub fn open_first() -> Result<(Self, Consumer), MidiError> {
        Self::open(PortSelector::First)
    }

    /// Opens the first port whose name contains `needle`, case-insensitively.
    pub fn open_matching(needle: &str) -> Result<(Self, Consumer), MidiError> {
        Self::open(PortSelector::Matching(needle.to_string()))
    }

    /// Opens a port and starts delivering events.
    ///
    /// Returns the connection and a [`Consumer`] to hand to
    /// [`crate::SynthBuilder::event_source`].
    pub fn open(selector: PortSelector) -> Result<(Self, Consumer), MidiError> {
        let (producer, consumer) = channel(1024);
        let shutdown = Arc::new(AtomicBool::new(false));

        let (ready_tx, ready_rx) = mpsc::channel::<Result<String, MidiError>>();
        let thread_shutdown = shutdown.clone();

        let thread = std::thread::Builder::new()
            .name("synth-midi".into())
            .spawn(move || {
                let connection = match connect(selector, producer) {
                    Ok((connection, name)) => {
                        let _ = ready_tx.send(Ok(name));
                        connection
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                // midir delivers on its own thread; this one exists only to own
                // the connection and keep the port open.
                while !thread_shutdown.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_millis(200));
                }

                // Closing here, on the thread that opened it.
                drop(connection);
            })
            .expect("failed to spawn the MIDI thread");

        match ready_rx.recv() {
            Ok(Ok(port_name)) => Ok((
                Self {
                    port_name,
                    shutdown,
                    thread: Some(thread),
                },
                consumer,
            )),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(MidiError::Unavailable),
        }
    }

    /// Closes the port and joins the keeper thread. Called automatically on
    /// drop.
    pub fn close(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl Drop for MidiInput {
    fn drop(&mut self) {
        self.close();
    }
}

/// Opens the selected port. Runs on the keeper thread.
fn connect(
    selector: PortSelector,
    producer: Producer,
) -> Result<(midir::MidiInputConnection<Producer>, String), MidiError> {
    let mut input = midir::MidiInput::new("synth-input").map_err(|_| MidiError::Unavailable)?;

    // Timing must NOT be ignored — that is the clock, and the sequencer needs
    // it. SysEx and active sensing are noise for our purposes: active sensing
    // in particular arrives every 300 ms from some keyboards and would do
    // nothing but burn queue slots.
    input.ignore(Ignore::SysexAndActiveSense);

    let ports = input.ports();
    let port = match &selector {
        PortSelector::First => ports.first().cloned(),
        PortSelector::Index(i) => ports.get(*i).cloned(),
        PortSelector::Matching(needle) => {
            let needle = needle.to_lowercase();
            ports
                .iter()
                .find(|port| {
                    input
                        .port_name(port)
                        .map(|name| name.to_lowercase().contains(&needle))
                        .unwrap_or(false)
                })
                .cloned()
        }
    }
    .ok_or(MidiError::NoPorts)?;

    let name = input
        .port_name(&port)
        .unwrap_or_else(|_| "<unnamed>".to_string());

    let connection = input
        .connect(
            &port,
            "synth",
            |_timestamp, message, producer: &mut Producer| {
                handle_message(message, producer);
            },
            producer,
        )
        .map_err(|e| MidiError::Connect(e.to_string()))?;

    Ok((connection, name))
}

/// Parses one MIDI message and pushes whatever it means onto the queue.
fn handle_message(message: &[u8], producer: &mut Producer) {
    let Some(&status) = message.first() else {
        return;
    };

    // System real-time messages (0xF8..0xFF) have no channel and can arrive in
    // the middle of anything, so they are checked first.
    match status {
        0xF8 => {
            producer.push(Event::ClockTick);
            return;
        }
        0xFA => {
            producer.push(Event::ClockStart);
            return;
        }
        0xFB => {
            producer.push(Event::ClockContinue);
            return;
        }
        0xFC => {
            producer.push(Event::ClockStop);
            return;
        }
        _ => {}
    }

    let kind = status & 0xF0;
    let data1 = message.get(1).copied().unwrap_or(0);
    let data2 = message.get(2).copied().unwrap_or(0);

    match kind {
        // Note-on with velocity 0 is a note-off. The spec allows it and most
        // keyboards use it, so they can send running status; treating it as a
        // note-on would leave every note hanging.
        0x90 => {
            if data2 == 0 {
                producer.push(Event::NoteOff { note: data1 });
            } else {
                let note = Note::from_midi(data1, data2);
                producer.push(Event::NoteOn {
                    note: note.pitch,
                    velocity: note.velocity,
                });
            }
        }
        0x80 => {
            producer.push(Event::NoteOff { note: data1 });
        }
        0xE0 => {
            // 14-bit value, LSB first, centred at 8192.
            let raw = ((data2 as i32) << 7) | data1 as i32;
            let normalised = (raw - 8192) as f32 / 8192.0;
            // +/-2 semitones is the near-universal default range.
            producer.push(Event::PitchBend(normalised * 2.0));
        }
        0xB0 => match data1 {
            1 => {
                producer.push(Event::ModWheel(data2 as f32 / 127.0));
            }
            // CC 120 All Sound Off, CC 123 All Notes Off.
            120 => {
                producer.push(Event::Panic);
            }
            123 => {
                producer.push(Event::AllNotesOff);
            }
            _ => {}
        },
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(message: &[u8]) -> Vec<Event> {
        let (mut producer, mut consumer) = channel(64);
        handle_message(message, &mut producer);
        let mut out = Vec::new();
        while let Some(e) = consumer.pop() {
            out.push(e);
        }
        out
    }

    /// The handle must be usable as a Bevy resource, which is the whole reason
    /// the connection lives on a keeper thread.
    #[test]
    fn the_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MidiInput>();
        assert_send_sync::<Consumer>();
    }

    #[test]
    fn note_on_and_off() {
        assert_eq!(
            parse(&[0x90, 60, 127]),
            vec![Event::NoteOn {
                note: 60,
                velocity: 1.0
            }]
        );
        assert_eq!(parse(&[0x80, 60, 0]), vec![Event::NoteOff { note: 60 }]);
    }

    /// The one MIDI quirk that hangs notes if you miss it.
    #[test]
    fn note_on_with_zero_velocity_is_a_note_off() {
        assert_eq!(parse(&[0x90, 60, 0]), vec![Event::NoteOff { note: 60 }]);
    }

    #[test]
    fn channel_is_ignored() {
        // The same message on channel 8 must behave identically.
        assert_eq!(
            parse(&[0x97, 64, 100]),
            vec![Event::NoteOn {
                note: 64,
                velocity: 100.0 / 127.0
            }]
        );
    }

    #[test]
    fn pitch_bend_centres_at_zero() {
        // 8192 = 0x2000: LSB 0, MSB 64.
        assert_eq!(parse(&[0xE0, 0, 64]), vec![Event::PitchBend(0.0)]);
        assert_eq!(parse(&[0xE0, 0, 0]), vec![Event::PitchBend(-2.0)]);
        // Full up is one step short of +2: the 14-bit range is asymmetric.
        match parse(&[0xE0, 127, 127]).as_slice() {
            [Event::PitchBend(v)] => assert!((*v - 2.0).abs() < 0.01),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn clock_messages_are_forwarded() {
        assert_eq!(parse(&[0xF8]), vec![Event::ClockTick]);
        assert_eq!(parse(&[0xFA]), vec![Event::ClockStart]);
        assert_eq!(parse(&[0xFB]), vec![Event::ClockContinue]);
        assert_eq!(parse(&[0xFC]), vec![Event::ClockStop]);
    }

    #[test]
    fn mod_wheel_and_panic_ccs() {
        assert_eq!(parse(&[0xB0, 1, 127]), vec![Event::ModWheel(1.0)]);
        assert_eq!(parse(&[0xB0, 120, 0]), vec![Event::Panic]);
        assert_eq!(parse(&[0xB0, 123, 0]), vec![Event::AllNotesOff]);
        // An unmapped CC must be ignored, not misinterpreted.
        assert_eq!(parse(&[0xB0, 74, 64]), vec![]);
    }

    #[test]
    fn malformed_messages_do_not_panic() {
        for message in [
            vec![],
            vec![0x90],
            vec![0x90, 60],
            vec![0xFF],
            vec![0x00, 0x00, 0x00],
        ] {
            let _ = parse(&message);
        }
    }
}
