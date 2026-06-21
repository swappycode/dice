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
use dice_voice_core::{
    AudioProcessor, EchoNoiseProcessor, JitterBuffer, JitterConfig, Passthrough, Playout,
    PreprocConfig, VoiceFrame,
};

/// Voice sample rate (Hz). Opus-native; the whole pipeline runs at this rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Frame duration (ms). 20 ms is the Opus/voice default.
pub const FRAME_MS: u32 = 20;
/// Samples in one mono frame (960 @ 48 kHz / 20 ms).
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;

/// VAD: i16 RMS at/above this over a 20 ms frame counts as speech (tuned for a
/// quiet-room noise floor vs. normal speech; refine once measured on real mics).
const VAD_RMS_THRESHOLD: f32 = 900.0;
/// Hold "speaking" this many frames (20 ms each) after the level dips, so the
/// orb doesn't flicker between syllables. 15 ≈ 300 ms.
const VAD_HANGOVER_FRAMES: u32 = 15;

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

/// Shared mic/output gating the audio thread reads each tick. Owned by
/// `ClientCore` (set from the `voice_state` command) and read by the running
/// [`VoiceEngine`] — so muting/deafening takes effect without restarting it.
/// `muted` stops capture from being transmitted; `deafened` stops remote audio
/// from being played (and drops inbound so the jitter buffers can't grow). The
/// engine's VAD pushes `speaking` here on each transition; a `ClientCore` task
/// watches it and fans it out as `VoiceState` (driving the per-user orbs).
pub struct VoiceControl {
    muted: AtomicBool,
    deafened: AtomicBool,
    /// Push-to-talk mode: when on, the mic only transmits while `ptt_held`.
    ptt_enabled: AtomicBool,
    ptt_held: AtomicBool,
    /// Chosen input/output device NAMES (`None` = system default). Read by the
    /// engine when it (re)starts; a change bumps `device_gen` so the bridge can
    /// restart the engine in place.
    input_device: Mutex<Option<String>>,
    output_device: Mutex<Option<String>>,
    /// Bumped on every `set_devices`; the bridge watches it to restart the
    /// running engine so a device change applies live (no rejoin / re-login).
    device_gen: tokio::sync::watch::Sender<u64>,
    speaking: tokio::sync::watch::Sender<bool>,
}

impl VoiceControl {
    /// Create the control plus the receiver a `ClientCore` task watches to
    /// publish speaking transitions as `VoiceState`.
    pub fn new() -> (Arc<Self>, tokio::sync::watch::Receiver<bool>) {
        let (speaking, rx) = tokio::sync::watch::channel(false);
        let (device_gen, _) = tokio::sync::watch::channel(0u64);
        let control = Arc::new(Self {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            ptt_enabled: AtomicBool::new(false),
            ptt_held: AtomicBool::new(false),
            input_device: Mutex::new(None),
            output_device: Mutex::new(None),
            device_gen,
            speaking,
        });
        (control, rx)
    }

    /// Choose input/output devices by name (`None` = system default). Bumps the
    /// device generation so the bridge restarts a running engine onto them.
    pub fn set_devices(&self, input: Option<String>, output: Option<String>) {
        *self.input_device.lock().expect("device lock") = input;
        *self.output_device.lock().expect("device lock") = output;
        self.device_gen.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// A receiver that fires whenever the device selection changes.
    pub fn subscribe_device_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.device_gen.subscribe()
    }
    fn input_device(&self) -> Option<String> {
        self.input_device.lock().expect("device lock").clone()
    }
    fn output_device(&self) -> Option<String> {
        self.output_device.lock().expect("device lock").clone()
    }

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
    pub fn set_ptt_enabled(&self, enabled: bool) {
        self.ptt_enabled.store(enabled, Ordering::Relaxed);
    }
    pub fn set_ptt_held(&self, held: bool) {
        self.ptt_held.store(held, Ordering::Relaxed);
    }
    /// Whether the mic should transmit right now: not muted, and either open-mic
    /// or (push-to-talk on AND its key currently held).
    pub fn transmitting(&self) -> bool {
        !self.muted.load(Ordering::Relaxed)
            && (!self.ptt_enabled.load(Ordering::Relaxed) || self.ptt_held.load(Ordering::Relaxed))
    }
    /// Publish a VAD speaking transition (call only on change).
    pub fn set_speaking(&self, speaking: bool) {
        let _ = self.speaking.send_replace(speaking);
    }
}

/// xorshift64 → uniform f64 in `[0, 1)`. Only used by the `DICE_VOICE_LOSS`
/// test aid, so a tiny non-crypto PRNG (no `rand` dependency) is plenty.
fn next_unit(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 11) as f64 / (1u64 << 53) as f64
}

/// Root-mean-square level of a frame (i16 scale). Cheap voice-activity signal:
/// near-zero on silence, hundreds-to-thousands on speech.
fn rms_i16(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / frame.len() as f64).sqrt() as f32
}

// ----------------------------------------------------------- device picking

/// The available capture/playback devices + the system defaults, for the picker.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

/// Enumerate the host's input/output devices by name (best-effort: a device
/// whose name can't be read is skipped). Blocking — call off the async runtime.
pub fn list_devices() -> AudioDevices {
    let host = cpal::default_host();
    // `input_devices()` / `output_devices()` return different iterator types, so
    // collect each inline rather than through one shared helper.
    let inputs = host
        .input_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    AudioDevices {
        inputs,
        outputs,
        default_input: host.default_input_device().and_then(|d| d.name().ok()),
        default_output: host.default_output_device().and_then(|d| d.name().ok()),
    }
}

/// The chosen input device by name, falling back to the system default.
fn pick_input(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = name
        && let Ok(mut devs) = host.input_devices()
        && let Some(dev) = devs.find(|d| d.name().ok().as_deref() == Some(name))
    {
        return Some(dev);
    }
    host.default_input_device()
}

/// The chosen output device by name, falling back to the system default.
fn pick_output(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = name
        && let Ok(mut devs) = host.output_devices()
        && let Some(dev) = devs.find(|d| d.name().ok().as_deref() == Some(name))
    {
        return Some(dev);
    }
    host.default_output_device()
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
    let input = pick_input(&host, control.input_device().as_deref())
        .ok_or_else(|| anyhow::anyhow!("no input device"))?;
    let output = pick_output(&host, control.output_device().as_deref())
        .ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let in_cfg = input.default_input_config()?;
    let out_cfg = output.default_output_config()?;
    if in_cfg.sample_rate().0 != SAMPLE_RATE || out_cfg.sample_rate().0 != SAMPLE_RATE {
        // Off-rate devices are linearly resampled to/from 48 kHz (see below).
        tracing::info!(
            in_rate = in_cfg.sample_rate().0,
            out_rate = out_cfg.sample_rate().0,
            "voice device not at 48 kHz — resampling (linear) to/from 48 kHz"
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
        in_device = %input.name().unwrap_or_default(),
        out_device = %output.name().unwrap_or_default(),
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
    // VAD speaking state + a short hangover so the orb doesn't flicker.
    let mut speaking = false;
    let mut hangover: u32 = 0;
    // Test aid: DICE_VOICE_LOSS=0.05 drops 5% of INBOUND frames so the headline
    // "graceful at 5% loss" gate is testable (jitter buffer + PLC conceal the
    // gaps). 0 / unset = off (production path untouched).
    let loss: f64 = std::env::var("DICE_VOICE_LOSS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|v| v.clamp(0.0, 1.0))
        .unwrap_or(0.0);
    if loss > 0.0 {
        tracing::warn!(
            loss,
            "DICE_VOICE_LOSS active: dropping inbound voice frames (test aid)"
        );
    }
    let mut rng: u64 = u64::from(ssrc).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut frame = vec![0i16; FRAME_SAMPLES];
    let mut remotes: HashMap<u32, RemoteStream> = HashMap::new();
    // Keep ~2 frames (40 ms) queued for the output device.
    let target_backlog = FRAME_SAMPLES * 2;

    // Optional near-end pre-processor: echo cancellation + noise suppression,
    // gated behind DICE_VOICE_AEC (default OFF -> a zero-cost Passthrough). When
    // on it cleans the mic frame BEFORE the VAD + Opus encode; the far-end echo
    // reference is the mixed playout frame (fed via push_far in step 3). A fresh
    // engine (device switch / rejoin) builds a fresh processor, so the adaptive
    // state resets implicitly.
    let aec_on = std::env::var("DICE_VOICE_AEC")
        .ok()
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(false);
    let mut pre: Box<dyn AudioProcessor> = if aec_on {
        let mut cfg = PreprocConfig::default();
        if let Some(ms) = std::env::var("DICE_VOICE_AEC_DELAY_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            cfg.pre_delay_ms = ms;
        }
        if let Some(ms) = std::env::var("DICE_VOICE_AEC_TAIL_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            cfg.tail_ms = ms;
        }
        tracing::info!("DICE_VOICE_AEC active: echo cancellation + noise suppression on the mic");
        Box::new(EchoNoiseProcessor::new(cfg))
    } else {
        Box::new(Passthrough)
    };

    while !stop.load(Ordering::Relaxed) {
        // `transmitting` folds in mute AND push-to-talk (open mic, or PTT held).
        let transmitting = control.transmitting();
        let deafened = control.deafened();

        // Not transmitting is never "speaking"; clear the orb the instant the
        // mic is cut (mute, or releasing the PTT key).
        if !transmitting && speaking {
            speaking = false;
            hangover = 0;
            control.set_speaking(false);
        }

        // 1. Capture → encode → send every full 20 ms frame. When not
        //    transmitting we still drain the capture buffer (so it can't back
        //    up) but send nothing.
        loop {
            let chunk = match capture.lock() {
                Ok(mut cap) if cap.len() >= FRAME_SAMPLES => {
                    cap.drain(..FRAME_SAMPLES).collect::<Vec<f32>>()
                }
                _ => break,
            };
            // The AEC/NS pre-processor (when DICE_VOICE_AEC is set) runs on EVERY
            // captured frame — its far reference must advance even while muted —
            // cleaning `near` in place only when transmitting.
            let cleaned: Option<[f32; FRAME_SAMPLES]> = if aec_on {
                let mut near: [f32; FRAME_SAMPLES] =
                    chunk.as_slice().try_into().expect("drained exactly one frame");
                pre.process(&mut near, transmitting);
                Some(near)
            } else {
                None
            };
            if !transmitting {
                continue;
            }
            match &cleaned {
                Some(near) => {
                    for (dst, &s) in frame.iter_mut().zip(near.iter()) {
                        *dst = f32_to_i16(s);
                    }
                }
                None => {
                    for (dst, &s) in frame.iter_mut().zip(chunk.iter()) {
                        *dst = f32_to_i16(s);
                    }
                }
            }
            // Voice activity → speaking orb (publish only on a transition).
            if rms_i16(&frame) >= VAD_RMS_THRESHOLD {
                hangover = VAD_HANGOVER_FRAMES;
            } else {
                hangover = hangover.saturating_sub(1);
            }
            let now_speaking = hangover > 0;
            if now_speaking != speaking {
                speaking = now_speaking;
                control.set_speaking(speaking);
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
            if loss > 0.0 && next_unit(&mut rng) < loss {
                continue; // simulated network loss (test aid)
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
            // Feed the produced playout frame to the speaker AND (when AEC is on)
            // to the pre-processor as the far-end echo reference.
            if produced {
                if aec_on {
                    let far: [f32; FRAME_SAMPLES] =
                        std::array::from_fn(|i| i16_to_f32(mix_clip(mixed[i])));
                    pre.push_far(&far);
                    if let Ok(mut pb) = playback.lock() {
                        for &s in &far {
                            pb.push_back(s);
                        }
                    }
                } else if let Ok(mut pb) = playback.lock() {
                    for &m in &mixed {
                        pb.push_back(i16_to_f32(mix_clip(m)));
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
    control.set_speaking(false);
    tracing::info!("voice engine stopped");
    Ok(())
}

// ------------------------------------------------------------- resampling

// The pipeline is 48 kHz; a device at another rate is resampled to/from 48 kHz
// with a windowed-sinc (Blackman) POLYPHASE FIR — a proper band-limited
// interpolator that rejects the images/aliases a naive linear interpolation
// would pass through. Bypassed entirely at 48 kHz, so the common path is
// untouched. Capture pushes arriving blocks through `PushResampler`; playback
// pulls one device-rate sample at a time through `PullResampler`. Both wrap one
// streaming `SincResampler` core.

/// Filter taps (input samples spanned per output). Even; half on each side.
const RS_TAPS: usize = 32;
/// Half-width in input samples (`RS_TAPS / 2`).
const RS_HALF: i64 = (RS_TAPS / 2) as i64;
/// Fractional-position quantization of the polyphase bank. 512 sub-phases keep
/// the phase error under ~1/512 of a sample (inaudible at voice rates).
const RS_PHASES: usize = 512;

/// Build the polyphase coefficient bank (`RS_PHASES * RS_TAPS`, phase-major): a
/// Blackman-windowed sinc low-pass at `fc` (cycles/sample), each phase row
/// normalized to unit DC gain so a constant passes through unchanged.
fn build_bank(fc: f64) -> Vec<f32> {
    use std::f64::consts::PI;
    let sinc = |x: f64| {
        if x.abs() < 1e-9 {
            1.0
        } else {
            (PI * x).sin() / (PI * x)
        }
    };
    let half = RS_HALF as f64;
    let mut bank = vec![0f32; RS_PHASES * RS_TAPS];
    for p in 0..RS_PHASES {
        let frac = p as f64 / RS_PHASES as f64;
        let mut sum = 0.0;
        for j in 0..RS_TAPS {
            // Distance (in input samples) from tap j to the output position.
            let x = frac + (RS_HALF - 1 - j as i64) as f64;
            let w = if x.abs() >= half {
                0.0
            } else {
                let t = x / half; // [-1, 1]
                0.42 + 0.5 * (PI * t).cos() + 0.08 * (2.0 * PI * t).cos()
            };
            let c = 2.0 * fc * sinc(2.0 * fc * x) * w;
            bank[p * RS_TAPS + j] = c as f32;
            sum += c;
        }
        // Normalize this phase to unit DC gain (exact constant passthrough).
        if sum.abs() > 1e-9 {
            let inv = (1.0 / sum) as f32;
            for j in 0..RS_TAPS {
                bank[p * RS_TAPS + j] *= inv;
            }
        }
    }
    bank
}

/// Streaming windowed-sinc resampler core (any in→out ratio). Feed input with
/// [`Self::push`], take output with [`Self::try_next`]. Linear phase; a fixed
/// small latency (~`RS_HALF` input samples). Built only when the rates differ.
struct SincResampler {
    bank: Vec<f32>,
    /// Input samples advanced per output sample (`in / out`).
    step: f64,
    /// Next output position, in input samples measured from `hist`'s front.
    pos: f64,
    /// Input history (front = oldest retained sample).
    hist: VecDeque<f32>,
}

impl SincResampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        // Anti-alias at the LOWER Nyquist: cutoff = ½ when upsampling, ½·(out/in)
        // when downsampling (in input-rate-normalized cycles/sample).
        let fc = 0.5 * (f64::from(out_rate) / f64::from(in_rate)).min(1.0);
        // Prime with RS_HALF zeros so the first output is centred at the stream
        // start (the filter's left half has no real samples yet).
        let hist: VecDeque<f32> = std::iter::repeat_n(0.0, RS_HALF as usize).collect();
        Self {
            bank: build_bank(fc),
            step: f64::from(in_rate) / f64::from(out_rate),
            pos: RS_HALF as f64,
            hist,
        }
    }

    fn push(&mut self, s: f32) {
        self.hist.push_back(s);
    }

    /// The next output sample, or `None` until `RS_HALF` look-ahead samples are
    /// buffered. Advances the read position and trims spent input on success.
    fn try_next(&mut self) -> Option<f32> {
        let i = self.pos.floor() as i64;
        if i + RS_HALF > self.hist.len() as i64 - 1 {
            return None; // need RS_HALF samples of look-ahead
        }
        let frac = self.pos - i as f64;
        let p = ((frac * RS_PHASES as f64).round() as usize).min(RS_PHASES - 1);
        let base = (i - RS_HALF + 1) as usize; // ≥ 0 by the priming + trim invariant
        let row = &self.bank[p * RS_TAPS..p * RS_TAPS + RS_TAPS];
        let mut acc = 0.0f32;
        for (j, &c) in row.iter().enumerate() {
            acc += c * self.hist[base + j];
        }
        self.pos += self.step;
        // Drop input no future output can reach, keeping `hist` bounded.
        let drop = self.pos.floor() as i64 - RS_HALF + 1;
        if drop > 0 {
            for _ in 0..drop {
                self.hist.pop_front();
            }
            self.pos -= drop as f64;
        }
        Some(acc)
    }
}

/// Capture-side resampler (`in_rate` → 48 kHz): feed arriving blocks, append
/// 48 kHz samples to `out`.
struct PushResampler(SincResampler);

impl PushResampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self(SincResampler::new(in_rate, out_rate))
    }

    fn process(&mut self, block: &[f32], out: &mut Vec<f32>) {
        for &s in block {
            self.0.push(s);
        }
        while let Some(y) = self.0.try_next() {
            out.push(y);
        }
    }
}

/// Playback-side resampler (48 kHz → `out_rate`): one output sample per `next`,
/// pulling 48 kHz samples on demand (`pull` returns 0.0 on underrun).
struct PullResampler(SincResampler);

impl PullResampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self(SincResampler::new(in_rate, out_rate))
    }

    fn next_sample(&mut self, pull: &mut impl FnMut() -> f32) -> f32 {
        loop {
            if let Some(y) = self.0.try_next() {
                return y;
            }
            self.0.push(pull());
        }
    }
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
    let in_rate = config.sample_rate.0;
    let max_backlog = SAMPLE_RATE as usize; // ~1 s at 48 kHz (post-resample)
    let mut resampler = (in_rate != SAMPLE_RATE).then(|| PushResampler::new(in_rate, SAMPLE_RATE));
    let mut mono: Vec<f32> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let Ok(mut b) = buf.lock() else { return };
            match &mut resampler {
                Some(r) => {
                    mono.clear();
                    for frame in data.chunks(channels) {
                        mono.push(
                            frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>()
                                / channels as f32,
                        );
                    }
                    resampled.clear();
                    r.process(&mono, &mut resampled);
                    b.extend(resampled.iter().copied());
                }
                None => {
                    for frame in data.chunks(channels) {
                        let m = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>()
                            / channels as f32;
                        b.push_back(m);
                    }
                }
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
    let out_rate = config.sample_rate.0;
    let mut resampler =
        (out_rate != SAMPLE_RATE).then(|| PullResampler::new(SAMPLE_RATE, out_rate));
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let mut guard = buf.lock().ok();
            for out_frame in data.chunks_mut(channels) {
                let mono = match &mut resampler {
                    Some(r) => r.next_sample(&mut || {
                        guard.as_mut().and_then(|b| b.pop_front()).unwrap_or(0.0)
                    }),
                    None => guard.as_mut().and_then(|b| b.pop_front()).unwrap_or(0.0),
                };
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
    fn resamplers_change_length_by_ratio_and_preserve_a_constant() {
        // Capture 24k → 48k roughly doubles the count (minus the FIR edge latency).
        let mut up = PushResampler::new(24_000, 48_000);
        let mut out = Vec::new();
        up.process(&[0.5f32; 2400], &mut out); // 100 ms @ 24k
        assert!((out.len() as i32 - 4800).abs() <= 40, "got {}", out.len());
        // A constant passes unit-DC-gain phases unchanged; skip the start-up ramp
        // where the window still overlaps the primed zeros.
        assert!(
            out[RS_TAPS..out.len() - 1]
                .iter()
                .all(|&s| (s - 0.5).abs() < 0.01),
            "constant preserved"
        );

        // Playback 48k → 24k roughly halves: pulling 2400 outputs eats ~4800 in.
        let mut down = PullResampler::new(48_000, 24_000);
        let mut src: VecDeque<f32> = std::iter::repeat_n(0.5f32, 4800).collect();
        let produced: Vec<f32> = (0..2400)
            .map(|_| down.next_sample(&mut || src.pop_front().unwrap_or(0.0)))
            .collect();
        assert!(
            produced[RS_TAPS..2000]
                .iter()
                .all(|&s| (s - 0.5).abs() < 0.01),
            "constant preserved through downsample"
        );
    }

    #[test]
    fn polyphase_downsampler_rejects_above_output_nyquist() {
        // 48k → 24k: output Nyquist 12 kHz. A 2 kHz tone (passband) survives near
        // unity; a 22 kHz tone (deep stopband — which a linear interpolator would
        // alias straight into the audible band) is strongly attenuated. This
        // band-limiting is the whole point of the windowed-sinc over linear.
        fn rms(xs: &[f32]) -> f32 {
            (xs.iter().map(|s| s * s).sum::<f32>() / xs.len() as f32).sqrt()
        }
        fn tone(freq: f64, n: usize) -> Vec<f32> {
            (0..n)
                .map(|i| ((i as f64 / 48_000.0) * freq * std::f64::consts::TAU).sin() as f32)
                .collect()
        }
        let run = |freq: f64| -> f32 {
            let mut r = PushResampler::new(48_000, 24_000);
            let mut out = Vec::new();
            r.process(&tone(freq, 9600), &mut out); // 200 ms
            // Measure the steady state, away from the filter's edge transients.
            rms(&out[RS_TAPS..out.len() - RS_TAPS])
        };
        let pass = run(2_000.0);
        let stop = run(22_000.0);
        assert!(pass > 0.5, "2 kHz passband tone survives (rms {pass})");
        assert!(stop < 0.05, "22 kHz stopband tone rejected (rms {stop})");
        assert!(stop < pass * 0.1, "stopband well below passband");
    }

    #[test]
    fn rms_distinguishes_silence_from_speech_level() {
        assert_eq!(rms_i16(&[]), 0.0);
        assert!(rms_i16(&[0i16; FRAME_SAMPLES]) < 1.0, "silence ≈ 0");
        // A constant-|amplitude| frame has RMS == that amplitude.
        let loud = [10_000i16; FRAME_SAMPLES];
        assert!((rms_i16(&loud) - 10_000.0).abs() < 1.0);
        // The VAD threshold sits above a quiet floor, below speech level.
        assert!(rms_i16(&[200i16; FRAME_SAMPLES]) < VAD_RMS_THRESHOLD);
        assert!(rms_i16(&loud) >= VAD_RMS_THRESHOLD);
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
