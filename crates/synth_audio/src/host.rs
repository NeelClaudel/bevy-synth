//! cpal output stream.
//!
//! # Why the stream lives on its own thread
//!
//! `cpal::Stream` is not `Send` on every platform — on some backends it is tied
//! to the thread that created it. A Bevy `Resource` must be `Send + Sync`, so
//! holding the stream directly in one would fail to compile on those platforms
//! even though it works on Linux.
//!
//! So the stream is built on a dedicated thread that then parks, keeping it
//! alive, and the handle returned here holds only `Send` things: the shared
//! parameters, an event producer and a shutdown flag. That also gives a clean
//! shutdown story — dropping [`Synth`] stops the stream and joins the thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use synth_core::event::{channel, Consumer, Producer};
use synth_core::{Engine, SharedParams};

/// How many events the game-side queue holds.
///
/// A dense sequencer plus two-handed playing plus MIDI clock is a few hundred
/// events a second, and the queue drains every few milliseconds, so this is
/// roomy by two orders of magnitude. Oversizing costs a few kilobytes once.
const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// The largest block the engine renders in one go inside the callback.
///
/// cpal hands over buffers of whatever size the driver chose, which can be
/// large. Rendering in bounded chunks keeps the scratch buffer a fixed
/// allocation made before the stream starts, so the callback never allocates
/// however big a buffer it is handed.
const MAX_SCRATCH: usize = 8192;

#[derive(Debug)]
pub enum SynthError {
    /// No audio device. Common in CI, containers and on machines with no sound
    /// card; worth handling rather than unwrapping, because a game that cannot
    /// open audio should still start.
    NoOutputDevice,
    Cpal(cpal::Error),
    /// The device offered a sample format this crate cannot convert to.
    UnsupportedSampleFormat(SampleFormat),
}

impl std::fmt::Display for SynthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SynthError::NoOutputDevice => write!(f, "no audio output device available"),
            SynthError::Cpal(e) => write!(f, "audio device error: {e}"),
            SynthError::UnsupportedSampleFormat(fmt) => {
                write!(f, "unsupported sample format: {fmt:?}")
            }
        }
    }
}

impl std::error::Error for SynthError {}

impl From<cpal::Error> for SynthError {
    fn from(e: cpal::Error) -> Self {
        SynthError::Cpal(e)
    }
}

/// A running synthesizer: an audio stream plus the handles to control it.
pub struct Synth {
    /// Every knob. Shared with the audio thread; set from anywhere.
    pub params: Arc<SharedParams>,
    /// Note and transport events for the audio thread.
    pub events: Producer,
    pub sample_rate: f32,
    pub channels: u16,

    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Synth {
    /// Starts on the default output device with default parameters.
    pub fn start() -> Result<Self, SynthError> {
        SynthBuilder::new().start()
    }

    /// Stops the stream and joins the audio thread. Called automatically on
    /// drop; exposed for callers that want to handle the timing themselves.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if let Some(handle) = thread.thread().name() {
                let _ = handle;
            }
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl Drop for Synth {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Configures a [`Synth`] before starting it.
#[derive(Default)]
pub struct SynthBuilder {
    params: Option<Arc<SharedParams>>,
    extra_sources: Vec<Consumer>,
    queue_capacity: Option<usize>,
}

impl SynthBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Uses an existing parameter block, so a UI can hold the same `Arc`.
    pub fn params(mut self, params: Arc<SharedParams>) -> Self {
        self.params = Some(params);
        self
    }

    /// Adds another event queue for the engine to drain — typically the MIDI
    /// input's, so hardware events do not have to be forwarded through the game
    /// thread and pick up its latency.
    pub fn event_source(mut self, consumer: Consumer) -> Self {
        self.extra_sources.push(consumer);
        self
    }

    pub fn queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = Some(capacity);
        self
    }

    /// Opens the device and starts the stream.
    pub fn start(self) -> Result<Synth, SynthError> {
        let params = self.params.unwrap_or_else(|| Arc::new(SharedParams::default()));
        let (producer, consumer) = channel(self.queue_capacity.unwrap_or(DEFAULT_QUEUE_CAPACITY));

        let mut sources = vec![consumer];
        sources.extend(self.extra_sources);

        let shutdown = Arc::new(AtomicBool::new(false));

        // The thread reports back whether the device opened, so `start` can
        // return a real error instead of failing silently in the background.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(f32, u16), SynthError>>();

        let thread_params = params.clone();
        let thread_shutdown = shutdown.clone();

        let thread = std::thread::Builder::new()
            .name("synth-audio".into())
            .spawn(move || {
                let stream = match build_stream(thread_params, sources) {
                    Ok((stream, rate, channels)) => {
                        let _ = ready_tx.send(Ok((rate, channels)));
                        stream
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };

                if let Err(e) = stream.play() {
                    eprintln!("synth: could not start the audio stream: {e}");
                    return;
                }

                // Park rather than spin: the stream runs on the driver's own
                // thread, and this one exists only to own it.
                while !thread_shutdown.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_millis(100));
                }

                // Dropping the stream here, on the thread that built it, is
                // what makes the not-`Send` platforms work.
                drop(stream);
            })
            .expect("failed to spawn the audio thread");

        match ready_rx.recv() {
            Ok(Ok((sample_rate, channels))) => Ok(Synth {
                params,
                events: producer,
                sample_rate,
                channels,
                shutdown,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            // The thread died before reporting: treat as no device.
            Err(_) => Err(SynthError::NoOutputDevice),
        }
    }
}

/// Opens the default device and builds a stream around a fresh [`Engine`].
fn build_stream(
    params: Arc<SharedParams>,
    sources: Vec<Consumer>,
) -> Result<(cpal::Stream, f32, u16), SynthError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(SynthError::NoOutputDevice)?;

    let supported = device.default_output_config()?;
    let sample_format = supported.sample_format();
    let sample_rate = supported.sample_rate() as f32;
    let channels = supported.channels();
    let config: cpal::StreamConfig = supported.into();

    let engine = Engine::with_sources(sample_rate, params, sources);

    let stream = match sample_format {
        SampleFormat::F32 => build_typed::<f32>(&device, config, engine, channels),
        SampleFormat::I16 => build_typed::<i16>(&device, config, engine, channels),
        SampleFormat::U16 => build_typed::<u16>(&device, config, engine, channels),
        SampleFormat::I32 => build_typed::<i32>(&device, config, engine, channels),
        SampleFormat::F64 => build_typed::<f64>(&device, config, engine, channels),
        other => return Err(SynthError::UnsupportedSampleFormat(other)),
    }?;

    Ok((stream, sample_rate, channels))
}

fn build_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut engine: Engine,
    channels: u16,
) -> Result<cpal::Stream, SynthError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = channels as usize;

    // Allocated once, here, before the stream starts. Nothing inside the
    // callback below allocates, locks or blocks.
    let mut scratch = vec![0.0f32; MAX_SCRATCH];

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels.max(1);
            let mut done = 0;

            while done < frames {
                let n = (frames - done).min(scratch.len());
                let block = &mut scratch[..n];
                engine.process(block);

                for (frame_index, value) in block.iter().enumerate() {
                    let sample = T::from_sample(*value);
                    let base = (done + frame_index) * channels;
                    // The engine is mono; the same signal goes to every
                    // channel. Real stereo would mean per-voice panning, which
                    // belongs in the engine, not here.
                    for channel in 0..channels {
                        data[base + channel] = sample;
                    }
                }

                done += n;
            }
        },
        move |err| {
            // Reaching here usually means an underrun or the device vanishing.
            // Printing is the right amount of noise: the stream keeps running,
            // and a game should not die because a USB interface was unplugged.
            eprintln!("synth: audio stream error: {err}");
        },
        None,
    )?;

    Ok(stream)
}
