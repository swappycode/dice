//! Step 1 of the voice audio layer: a mic → speaker loopback to confirm `cpal`
//! capture + playback actually work on THIS machine, isolated from the codec,
//! network, and UI. If you hear yourself, the audio stack is good and we build
//! the real pipeline on top of it.
//!
//! Run from `apps/desktop-client/src-tauri`:
//!     cargo run --example voice_loopback
//!
//! Put on HEADPHONES (there is no echo cancellation yet — speakers will howl),
//! speak, and you should hear yourself with a short delay. Ctrl-C to stop.
//! It prints the chosen devices + their configs; if the input and output
//! sample rates differ, the pitch will be off — tell me the printed configs.

#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

/// Shared mono ring buffer between the capture and playback callbacks.
type Buf = Arc<Mutex<VecDeque<f32>>>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let input = host
        .default_input_device()
        .ok_or("no default input device")?;
    let output = host
        .default_output_device()
        .ok_or("no default output device")?;
    println!("input device:  {}", input.name()?);
    println!("output device: {}", output.name()?);

    let in_cfg = input.default_input_config()?;
    let out_cfg = output.default_output_config()?;
    println!("input config:  {in_cfg:?}");
    println!("output config: {out_cfg:?}");
    if in_cfg.sample_rate() != out_cfg.sample_rate() {
        println!(
            "WARNING: input rate {} != output rate {} — pitch will be wrong; \
             tell me these numbers and I'll add resampling.",
            in_cfg.sample_rate().0,
            out_cfg.sample_rate().0
        );
    }

    let buf: Buf = Arc::new(Mutex::new(VecDeque::new()));

    let in_stream = match in_cfg.sample_format() {
        cpal::SampleFormat::F32 => build_input::<f32>(&input, &in_cfg.config(), buf.clone())?,
        cpal::SampleFormat::I16 => build_input::<i16>(&input, &in_cfg.config(), buf.clone())?,
        cpal::SampleFormat::U16 => build_input::<u16>(&input, &in_cfg.config(), buf.clone())?,
        other => return Err(format!("unsupported input sample format: {other:?}").into()),
    };
    let out_stream = match out_cfg.sample_format() {
        cpal::SampleFormat::F32 => build_output::<f32>(&output, &out_cfg.config(), buf.clone())?,
        cpal::SampleFormat::I16 => build_output::<i16>(&output, &out_cfg.config(), buf.clone())?,
        cpal::SampleFormat::U16 => build_output::<u16>(&output, &out_cfg.config(), buf.clone())?,
        other => return Err(format!("unsupported output sample format: {other:?}").into()),
    };

    in_stream.play()?;
    out_stream.play()?;
    println!("\nLoopback running. HEADPHONES on — speak; you should hear yourself.");
    println!("Ctrl-C to stop (auto-stops after 2 minutes).");
    std::thread::sleep(Duration::from_secs(120));
    Ok(())
}

/// Capture stream: downmix every frame to mono and push it into `buf`, bounding
/// the backlog so latency can't grow without limit.
fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buf: Buf,
) -> Result<cpal::Stream, Box<dyn std::error::Error>>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let max_backlog = config.sample_rate.0 as usize / 2; // ~0.5 s
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut b = buf.lock().unwrap();
            for frame in data.chunks(channels) {
                let mono = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>()
                    / channels as f32;
                b.push_back(mono);
            }
            while b.len() > max_backlog {
                b.pop_front();
            }
        },
        |e| eprintln!("input stream error: {e}"),
        None,
    )?;
    Ok(stream)
}

/// Playback stream: pull mono samples from `buf` (silence on underrun) and fan
/// them across the output channels.
fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    buf: Buf,
) -> Result<cpal::Stream, Box<dyn std::error::Error>>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let mut b = buf.lock().unwrap();
            for frame in data.chunks_mut(channels) {
                let mono = b.pop_front().unwrap_or(0.0);
                let sample = T::from_sample(mono);
                for x in frame.iter_mut() {
                    *x = sample;
                }
            }
        },
        |e| eprintln!("output stream error: {e}"),
        None,
    )?;
    Ok(stream)
}
