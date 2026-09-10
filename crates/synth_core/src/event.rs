//! Control events and the lock-free queue that carries them to the audio thread.
//!
//! Parameters ([`crate::params`]) are *state*: the audio thread only needs the
//! latest value, so an atomic per knob is enough. Events are different — they
//! are *things that happened*, and dropping one loses a note. So they need a
//! queue, and it must be one the audio thread can drain without locking.
//!
//! This is a single-producer single-consumer ring buffer. SPSC is the weakest
//! and therefore fastest flavour, and it is all that is needed: one control
//! thread pushes, the audio callback pops. If a second producer is ever needed
//! (say, MIDI on its own thread), give it its own queue rather than reaching
//! for a multi-producer structure — the engine drains as many queues as you
//! hand it, and two SPSC queues are cheaper than one MPSC.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::drums::Cell;
use crate::sequencer::Step;

/// Something that happened, to be applied at the next audio block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// Velocity is `0.0..=1.0`. A note-on with zero velocity is *not* treated
    /// as a note-off here; the MIDI layer normalises that before it gets in.
    NoteOn { note: u8, velocity: f32 },
    NoteOff { note: u8 },
    /// Releases everything gracefully, respecting release times.
    AllNotesOff,
    /// Cuts everything dead. For when something has gone wrong and a stuck note
    /// is screaming.
    Panic,

    /// Pitch bend in semitones.
    PitchBend(f32),
    /// Mod wheel, `0.0..=1.0`.
    ModWheel(f32),

    /// One MIDI clock tick. 24 of these per quarter note, by the spec.
    ClockTick,
    /// Transport start: rewind to step zero and play.
    ClockStart,
    ClockStop,
    /// Transport continue: play from where it stopped.
    ClockContinue,

    /// Overwrites one step of the pattern.
    SetStep { index: u8, step: Step },
    /// Throws away the current pattern and generates a new one.
    Regenerate,

    /// Overwrites one step of the bass pattern.
    ///
    /// A separate variant rather than a channel field on `SetStep`: additive,
    /// no existing call site changes, and it is the same size as the variant
    /// that already sets the queue's slot size, so the queue does not grow.
    SetBassStep { index: u8, step: Step },
    /// Writes a fresh bass line from the current `bass_gen_*` parameters.
    RegenerateBass,

    /// Set one cell of the drum grid.
    ///
    /// Small enough to travel by value: `Cell` is a `bool` and an `f32`, so
    /// this variant does not make the enum any larger than `SetStep` already
    /// does, and every queue slot is sized by the largest variant.
    SetDrumCell { step: u8, pad: u8, cell: Cell },
}

/// The shared ring buffer behind a [`Producer`]/[`Consumer`] pair.
struct Inner {
    /// Capacity is always a power of two so the wrap is a mask, not a modulo.
    buffer: Box<[UnsafeCell<Event>]>,
    mask: usize,
    /// Next slot the producer will write.
    head: AtomicUsize,
    /// Next slot the consumer will read.
    tail: AtomicUsize,
}

// SAFETY: access to `buffer` is disjoint by construction. The producer only
// writes to the slot at `head` before publishing it by advancing `head` with a
// `Release` store; the consumer only reads slots strictly below `head`, which it
// loads with `Acquire`. That pair establishes happens-before between the write
// and the read, so no slot is ever touched by both threads at once. Only one
// `Producer` and one `Consumer` can exist, because `channel` is the sole
// constructor and neither handle is `Clone`.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

/// Creates a connected producer/consumer pair.
///
/// `capacity` is rounded up to a power of two. Size it for the worst case: a
/// dense sequencer plus a two-handed keyboard part plus MIDI clock at 24 ticks
/// per beat is still only a few hundred events per second, so 1024 is roomy.
pub fn channel(capacity: usize) -> (Producer, Consumer) {
    let capacity = capacity.next_power_of_two().max(2);
    let mut buffer = Vec::with_capacity(capacity);
    for _ in 0..capacity {
        buffer.push(UnsafeCell::new(Event::Panic));
    }

    let inner = Arc::new(Inner {
        buffer: buffer.into_boxed_slice(),
        mask: capacity - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    });

    (
        Producer {
            inner: inner.clone(),
        },
        Consumer { inner },
    )
}

/// The control-thread end of the queue.
pub struct Producer {
    inner: Arc<Inner>,
}

impl Producer {
    /// Pushes an event. Returns `false` if the queue is full and the event was
    /// dropped.
    ///
    /// Dropping is the right failure mode here. The alternative — blocking, or
    /// growing the buffer — would either stall the game thread or allocate,
    /// and a full queue means the audio thread has stopped draining, at which
    /// point there is no audio to be late for anyway.
    pub fn push(&self, event: Event) -> bool {
        let head = self.inner.head.load(Ordering::Relaxed);
        let tail = self.inner.tail.load(Ordering::Acquire);

        // One slot is always left empty, so a full buffer is distinguishable
        // from an empty one without a separate counter.
        if head.wrapping_sub(tail) >= self.inner.buffer.len() - 1 {
            return false;
        }

        let slot = head & self.inner.mask;
        // SAFETY: the consumer never reads this slot, because it only reads
        // below `head` and we have not advanced `head` yet.
        unsafe {
            *self.inner.buffer[slot].get() = event;
        }

        // Release: publishes the write above to any thread that Acquire-loads
        // `head`.
        self.inner.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// How many events are waiting. Advisory only — the consumer may drain more
    /// between this call and the next push.
    pub fn len(&self) -> usize {
        let head = self.inner.head.load(Ordering::Relaxed);
        let tail = self.inner.tail.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The audio-thread end of the queue.
pub struct Consumer {
    inner: Arc<Inner>,
}

impl Consumer {
    /// Pops the next event, or `None` if the queue is empty. Never blocks,
    /// never allocates.
    #[inline]
    pub fn pop(&mut self) -> Option<Event> {
        let tail = self.inner.tail.load(Ordering::Relaxed);
        // Acquire: pairs with the producer's Release store, making its write to
        // the slot visible to us.
        let head = self.inner.head.load(Ordering::Acquire);

        if tail == head {
            return None;
        }

        let slot = tail & self.inner.mask;
        // SAFETY: `tail < head`, so the producer has finished writing this slot
        // and will not touch it again until we advance `tail` past it.
        let event = unsafe { *self.inner.buffer[slot].get() };

        self.inner.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(event)
    }
}

/// A producer and consumer sharing one queue, for tests and single-threaded use.
pub struct EventQueue {
    pub producer: Producer,
    pub consumer: Consumer,
}

impl EventQueue {
    pub fn new(capacity: usize) -> Self {
        let (producer, consumer) = channel(capacity);
        Self { producer, consumer }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_come_out_in_order() {
        let (tx, mut rx) = channel(16);
        for i in 0..10u8 {
            assert!(tx.push(Event::NoteOn {
                note: i,
                velocity: 1.0
            }));
        }
        for i in 0..10u8 {
            assert_eq!(
                rx.pop(),
                Some(Event::NoteOn {
                    note: i,
                    velocity: 1.0
                })
            );
        }
        assert_eq!(rx.pop(), None);
    }

    #[test]
    fn a_full_queue_drops_rather_than_blocking() {
        let (tx, mut rx) = channel(4);
        // Capacity 4, one slot reserved: 3 usable.
        assert!(tx.push(Event::ClockTick));
        assert!(tx.push(Event::ClockTick));
        assert!(tx.push(Event::ClockTick));
        assert!(!tx.push(Event::ClockTick), "should have reported full");

        // Draining one makes room again.
        assert!(rx.pop().is_some());
        assert!(tx.push(Event::ClockTick));
    }

    #[test]
    fn wraps_around_indefinitely() {
        let (tx, mut rx) = channel(8);
        for i in 0..10_000u32 {
            assert!(tx.push(Event::PitchBend(i as f32)));
            assert_eq!(rx.pop(), Some(Event::PitchBend(i as f32)));
        }
    }

    /// The real usage pattern: one thread pushing while another drains.
    #[test]
    fn survives_concurrent_producer_and_consumer() {
        let (tx, mut rx) = channel(64);
        let sent = 100_000u32;

        let producer = std::thread::spawn(move || {
            let mut pushed = 0;
            while pushed < sent {
                if tx.push(Event::PitchBend(pushed as f32)) {
                    pushed += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });

        let mut received = 0u32;
        while received < sent {
            match rx.pop() {
                // Ordering must be preserved exactly, with nothing lost.
                Some(Event::PitchBend(v)) => {
                    assert_eq!(v, received as f32);
                    received += 1;
                }
                Some(other) => panic!("unexpected event {other:?}"),
                None => std::thread::yield_now(),
            }
        }

        producer.join().unwrap();
    }
}
