//! Voice audio pipeline (M3 phase 3).
//!
//! The canonical voice format is **48 kHz mono, 20 ms frames** (960 samples) —
//! Opus-native, low-latency, and the shape the gateway SFU forwards. This
//! module owns the codec (behind [`VoiceCodec`] so a passthrough/other codec
//! can drop in) and, in a later step, the `cpal` capture/playback engine that
//! wires mic → encode → [`GatewayHandle::send_voice`] and `VoiceData` →
//! jitter buffer → decode → speaker.
//!
//! Codec correctness is unit-tested here (encode→decode round-trip) without any
//! audio hardware; the device I/O is verified on-hardware.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use dice_network_core::client::VoiceSender;
use dice_protocol::bytes::Bytes;
use dice_voice_core::{JitterBuffer, JitterConfig, Playout, VoiceFrame};

/// Voice sample rate (Hz). Opus-native; the whole pipeline runs at this rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Frame duration (ms). 20 ms is the Opus/voice default.
pub const FRAME_MS: u32 = 20;
/// Samples in one mono frame (960 @ 48 kHz / 20 ms).
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;

/// Encode/decode one 20 ms mono frame. Behind a trait so the rest of the
/// pipeline never names a concrete codec.
pub trait VoiceCodec: Send {
    /// Encode `pcm` (exactly [`FRAME_SAMPLES`] i16 mono samples) to a packet.
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, CodecError>;
    /// Decode `packet` into `out` ([`FRAME_SAMPLES`] capacity), returning the
    /// sample count. `packet = None` signals packet loss → the codec runs PLC
    /// and still produces a frame.
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> Result<usize, CodecError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus: {0}")]
    Opus(String),
}

/// Opus codec (libopus via `audiopus`), VoIP-tuned, 48 kHz mono.
pub struct OpusCodec {
    encoder: audiopus::coder::Encoder,
    decoder: audiopus::coder::Decoder,
    /// Reusable encode scratch (Opus packets are < 1 KiB at voice bitrates).
    scratch: Vec<u8>,
}

impl OpusCodec {
    pub fn new() -> Result<Self, CodecError> {
        use audiopus::{Application, Channels, SampleRate};
        let encoder =
            audiopus::coder::Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
                .map_err(opus_err)?;
        let decoder =
            audiopus::coder::Decoder::new(SampleRate::Hz48000, Channels::Mono).map_err(opus_err)?;
        Ok(Self {
            encoder,
            decoder,
            scratch: vec![0u8; 4000],
        })
    }
}

fn opus_err(e: audiopus::Error) -> CodecError {
    CodecError::Opus(e.to_string())
}

impl VoiceCodec for OpusCodec {
    fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, CodecError> {
        let n = self
            .encoder
            .encode(pcm, &mut self.scratch)
            .map_err(opus_err)?;
        Ok(self.scratch[..n].to_vec())
    }

    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> Result<usize, CodecError> {
        self.decoder.decode(packet, out, false).map_err(opus_err)
    }
}

// ---------------------------------------------------------------- conversions

/// Clamp + scale a normalized f32 sample to i16 (Opus input).
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

/// i16 sample → normalized f32 (cpal output).
fn i16_to_f32(s: i16) -> f32 {
    f32::from(s) / 32_768.0
}

/// Clamp a mixed (summed) i32 accumulator back to i16.
fn mix_clip(acc: i32) -> i16 {
    acc.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

// ------------------------------------------------------------ voice control

/// Shared, lock-free mic/output gating the audio thread reads each tick. Owned
/// by `ClientCore` (set from the `voice_state` command) and read by the running
/// [`VoiceEngine`] — so muting/deafening takes effect without restarting it.
/// `muted` stops capture from being transmitted; `deafened` stops remote audio
/// from being played (and drops inbound so the jitter buffers can't grow).
#[derive(Default)]
pub struct VoiceControl {
    muted: AtomicBool,
    deafened: AtomicBool,
}

impl VoiceControl {
    pub fn set(&self, muted: bool, deafened: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        self.deafened.store(deafened, Ordering::Relaxed);
    }
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    pub fn deafened(&self) -> bool {
        self.deafened.load(Ordering::Relaxed)
    }
}

// -------------------------------------------------------------- voice engine

/// One remote speaker's playout state: a jitter buffer + its own Opus decoder
/// (decoder state is per-stream) + a reusable decode scratch.
struct RemoteStream {
    jitter: JitterBuffer,
    codec: OpusCodec,
    pcm: Vec<i16>,
}

impl RemoteStream {
    fn new() -> Result<Self, CodecError> {
        Ok(Self {
            jitter: JitterBuffer::new(JitterConfig::default()),
            codec: OpusCodec::new()?,
            pcm: vec![0i16; FRAME_SAMPLES],
        })
    }
}

/// A live voice session. A dedicated thread owns the (Windows-`!Send`) cpal
/// streams and runs capture→encode→send and recv→jitter→decode→playback; the
/// bridge talks to it only through `Send` channels. Created on join, dropped on
/// leave (drop stops the thread + streams).
pub struct VoiceEngine {
    /// Inbound voice datagrams (raw `VoiceFrame` bytes) → playback.
    inbox: mpsc::Sender<Bytes>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VoiceEngine {
    /// Start capture + playback. `ssrc` stamps this client's outgoing frames;
    /// `sender` ships them on the active QUIC connection; `control` gates the
    /// mic (mute) and output (deafen) live.
    pub fn start(ssrc: u32, sender: VoiceSender, control: Arc<VoiceControl>) -> Self {
        let (inbox_tx, inbox_rx) = mpsc::channel::<Bytes>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("dice-voice".to_owned())
            .spawn(move || {
                if let Err(error) = run_engine(ssrc, &sender, &inbox_rx, &stop_thread, &control) {
                    tracing::error!(%error, "voice engine stopped");
                }
            })
            .expect("spawn voice thread");
        Self {
            inbox: inbox_tx,
            stop,
            thread: Some(thread),
        }
    }

    /// Feed an inbound voice datagram to playback (best-effort; dropped if the
    /// engine is gone — voice is loss-tolerant).
    pub fn on_voice_data(&self, bytes: Bytes) {
        let _ = self.inbox.send(bytes);
    }
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

type SharedPcm = Arc<Mutex<VecDeque<f32>>>;

/// Runs on the dedicated voice thread: owns the cpal streams + the encode/decode
/// loop until `stop` is set.
fn run_engine(
    ssrc: u32,
    sender: &VoiceSender,
    inbox: &mpsc::Receiver<Bytes>,
    stop: &AtomicBool,
    control: &VoiceControl,
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let input = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no default input device"))?;
    let output = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let in_cfg = input.default_input_config()?;
    let out_cfg = output.default_output_config()?;
    if in_cfg.sample_rate().0 != SAMPLE_RATE || out_cfg.sample_rate().0 != SAMPLE_RATE {
        // No resampler yet (Step-1 devices are 48 kHz); warn and proceed.
        tracing::warn!(
            in_rate = in_cfg.sample_rate().0,
            out_rate = out_cfg.sample_rate().0,
            "voice device not at 48 kHz — pitch will be off until resampling lands"
        );
    }

    let capture: SharedPcm = Arc::new(Mutex::new(VecDeque::new()));
    let playback: SharedPcm = Arc::new(Mutex::new(VecDeque::new()));

    let in_stream = build_capture(&input, &in_cfg, Arc::clone(&capture))?;
    let out_stream = build_playback(&output, &out_cfg, Arc::clone(&playback))?;
    in_stream.play()?;
    out_stream.play()?;
    tracing::info!(
        ssrc,
        quic_voice_path = sender.is_connected(),
        in_rate = in_cfg.sample_rate().0,
        out_rate = out_cfg.sample_rate().0,
        in_format = ?in_cfg.sample_format(),
        out_format = ?out_cfg.sample_format(),
        "voice engine running"
    );

    let mut encoder = OpusCodec::new()?;
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    // Warn ONCE if we're capturing audio but have no QUIC datagram path (a WSS
    // session is silent — voice rides QUIC only). Makes "no audio" diagnosable.
    let mut warned_no_path = false;
    let mut frame = vec![0i16; FRAME_SAMPLES];
    let mut remotes: HashMap<u32, RemoteStream> = HashMap::new();
    // Keep ~2 frames (40 ms) queued for the output device.
    let target_backlog = FRAME_SAMPLES * 2;

    while !stop.load(Ordering::Relaxed) {
        let muted = control.muted();
        let deafened = control.deafened();

        // 1. Capture → encode → send every full 20 ms frame. When muted we still
        //    drain the capture buffer (so it can't back up) but transmit nothing.
        loop {
            let chunk = match capture.lock() {
                Ok(mut cap) if cap.len() >= FRAME_SAMPLES => {
                    cap.drain(..FRAME_SAMPLES).collect::<Vec<f32>>()
                }
                _ => break,
            };
            if muted {
                continue;
            }
            for (dst, &s) in frame.iter_mut().zip(chunk.iter()) {
                *dst = f32_to_i16(s);
            }
            if let Ok(payload) = encoder.encode(&frame) {
                let vf = VoiceFrame {
                    ssrc,
                    seq,
                    timestamp,
                    marker: seq == 0,
                    payload: Bytes::from(payload),
                };
                if sender.is_connected() {
                    sender.send(vf.encode());
                } else if !warned_no_path {
                    warned_no_path = true;
                    tracing::warn!(
                        "voice: capturing audio but no QUIC datagram path — the session is \
                         on WSS (voice rides QUIC datagrams only), so nothing is transmitted; \
                         confirm the status bar shows QUIC"
                    );
                }
                seq = seq.wrapping_add(1);
                timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
            }
        }

        // 2. Inbound datagrams → per-ssrc jitter buffers. Deafened: drop them
        //    on the floor so the buffers can't grow while we're not playing out.
        while let Ok(bytes) = inbox.try_recv() {
            if deafened {
                continue;
            }
            if let Ok(vf) = VoiceFrame::decode(bytes) {
                match remotes.entry(vf.ssrc) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        e.get_mut().jitter.push(vf);
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        if let Ok(rs) = RemoteStream::new() {
                            e.insert(rs).jitter.push(vf);
                        }
                    }
                }
            }
        }

        // 3. Playout: while the output backlog is low, pop one frame per remote,
        //    decode (PLC on a gap), mix, and queue. The backlog gate paces this
        //    to the real-time output rate.
        let backlog = playback.lock().map(|p| p.len()).unwrap_or(target_backlog);
        if !deafened && backlog < target_backlog {
            let mut mixed = [0i32; FRAME_SAMPLES];
            let mut produced = false;
            for rs in remotes.values_mut() {
                let decoded = match rs.jitter.pop() {
                    Some(Playout::Frame(vf)) => {
                        rs.codec.decode(Some(vf.payload.as_ref()), &mut rs.pcm)
                    }
                    Some(Playout::Conceal) => rs.codec.decode(None, &mut rs.pcm),
                    None => continue,
                };
                if let Ok(n) = decoded {
                    for (m, &s) in mixed.iter_mut().zip(rs.pcm[..n.min(FRAME_SAMPLES)].iter()) {
                        *m += i32::from(s);
                    }
                    produced = true;
                }
            }
            if produced && let Ok(mut pb) = playback.lock() {
                for &m in &mixed {
                    pb.push_back(i16_to_f32(mix_clip(m)));
                }
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
    tracing::info!("voice engine stopped");
    Ok(())
}

/// Capture stream: downmix each frame to mono f32 into `buf`, bounding backlog.
fn build_capture(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    buf: SharedPcm,
) -> anyhow::Result<cpal::Stream> {
    let config = cfg.config();
    Ok(match cfg.sample_format() {
        cpal::SampleFormat::F32 => capture_stream::<f32>(device, &config, buf)?,
        cpal::SampleFormat::I16 => capture_stream::<i16>(device, &config, buf)?,
        cpal::SampleFormat::U16 => capture_stream::<u16>(device, &config, buf)?,
        other => anyhow::bail!("unsupported input sample format: {other:?}"),
    })
}

fn capture_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buf: SharedPcm,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let max_backlog = config.sample_rate.0 as usize; // ~1 s safety cap
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let Ok(mut b) = buf.lock() else { return };
            for frame in data.chunks(channels) {
                let mono =
                    frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32;
                b.push_back(mono);
            }
            while b.len() > max_backlog {
                b.pop_front();
            }
        },
        |error| tracing::warn!(%error, "voice input stream error"),
        None,
    )?;
    Ok(stream)
}

/// Playback stream: pull mono f32 from `buf` (silence on underrun), fan to all
/// output channels.
fn build_playback(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    buf: SharedPcm,
) -> anyhow::Result<cpal::Stream> {
    let config = cfg.config();
    Ok(match cfg.sample_format() {
        cpal::SampleFormat::F32 => playback_stream::<f32>(device, &config, buf)?,
        cpal::SampleFormat::I16 => playback_stream::<i16>(device, &config, buf)?,
        cpal::SampleFormat::U16 => playback_stream::<u16>(device, &config, buf)?,
        other => anyhow::bail!("unsupported output sample format: {other:?}"),
    })
}

fn playback_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buf: SharedPcm,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let mut guard = buf.lock().ok();
            for out_frame in data.chunks_mut(channels) {
                let mono = guard.as_mut().and_then(|b| b.pop_front()).unwrap_or(0.0);
                let sample = T::from_sample(mono);
                for x in out_frame.iter_mut() {
                    *x = sample;
                }
            }
        },
        |error| tracing::warn!(%error, "voice output stream error"),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Opus actually links and round-trips a 20 ms frame — no audio hardware.
    #[test]
    fn opus_round_trips_a_frame() {
        let mut codec = OpusCodec::new().unwrap();
        // A quiet 440 Hz-ish tone over one 20 ms frame.
        let pcm: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 6000.0) as i16
            })
            .collect();

        let packet = codec.encode(&pcm).unwrap();
        assert!(
            !packet.is_empty() && packet.len() < 400,
            "voice packet should be small, got {}",
            packet.len()
        );

        let mut out = vec![0i16; FRAME_SAMPLES];
        let decoded = codec.decode(Some(&packet), &mut out).unwrap();
        assert_eq!(decoded, FRAME_SAMPLES, "one frame decodes to one frame");

        // Packet loss: PLC still yields a full frame.
        let concealed = codec.decode(None, &mut out).unwrap();
        assert_eq!(concealed, FRAME_SAMPLES, "PLC produces a frame on loss");
    }

    #[test]
    fn sample_conversions_round_trip_and_clamp() {
        // f32 → i16 → f32 stays close for in-range samples.
        for &s in &[-1.0f32, -0.5, 0.0, 0.5, 0.99] {
            let back = i16_to_f32(f32_to_i16(s));
            assert!((back - s).abs() < 0.001, "{s} -> {back}");
        }
        // Out-of-range f32 clamps, not wraps.
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX);
        // Mixing clamps instead of overflowing.
        assert_eq!(mix_clip(i32::from(i16::MAX) * 3), i16::MAX);
        assert_eq!(mix_clip(i32::from(i16::MIN) * 3), i16::MIN);
        assert_eq!(mix_clip(100), 100);
    }
}
