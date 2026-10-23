//! Microphone and speaker at the agent's format: 16 kHz mono 16-bit PCM. Works on CoreAudio (the
//! simulator on a Mac) and ALSA (the NAO).

use std::{
    collections::VecDeque,
    sync::{
        mpsc::{sync_channel, Receiver},
        Arc, Mutex,
    },
};

use color_eyre::{
    eyre::{eyre, WrapErr},
    Result,
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream,
};

pub const RATE: f32 = 16_000.0;

pub struct Audio {
    _input: Stream,
    _output: Stream,
    /// 16 kHz mono samples from the microphone.
    pub captured: Receiver<Vec<i16>>,
    /// 16 kHz mono samples waiting to be played.
    pub playback: Arc<Mutex<VecDeque<f32>>>,
}

impl Audio {
    pub fn open() -> Result<Self> {
        let mut audio = Self::open_devices()?;
        // Testing without a person: NAO_EPH_VOICE_TEST_WAV (16 kHz mono 16-bit) replaces the
        // microphone, played in real time and followed by silence.
        if let Ok(path) = std::env::var("NAO_EPH_VOICE_TEST_WAV") {
            let bytes = std::fs::read(&path).wrap_err_with(|| format!("reading {path}"))?;
            let samples: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            let (sender, captured) = sync_channel(64);
            std::thread::spawn(move || {
                let mut chunks = samples.chunks(1600).map(<[i16]>::to_vec).chain(std::iter::repeat(vec![0; 1600]));
                while let Some(chunk) = chunks.next() {
                    if sender.send(chunk).is_err() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            });
            audio.captured = captured;
        }
        Ok(audio)
    }

    fn open_devices() -> Result<Self> {
        let host = cpal::default_host();
        let input_device = host.default_input_device().ok_or_else(|| eyre!("no microphone"))?;
        let output_device = host.default_output_device().ok_or_else(|| eyre!("no speaker"))?;

        let input_config = input_device.default_input_config().wrap_err("microphone config")?;
        let channels = input_config.channels() as usize;
        let step = RATE / input_config.sample_rate().0 as f32;
        let (sender, captured) = sync_channel(64);
        let mut phase = 0.0f32;
        let mut convert = move |mono: &mut dyn Iterator<Item = f32>| {
            // Drops or repeats samples to reach 16 kHz; speech recognition does not mind.
            let mut out = Vec::new();
            for sample in mono {
                phase += step;
                while phase >= 1.0 {
                    phase -= 1.0;
                    out.push((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                }
            }
            let _ = sender.try_send(out);
        };
        let error = |e| eprintln!("nao-eph-voice: audio: {e}");
        let config = input_config.config();
        let input = match input_config.sample_format() {
            SampleFormat::F32 => input_device.build_input_stream(
                &config,
                move |data: &[f32], _: &_| {
                    convert(&mut data.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32))
                },
                error,
                None,
            ),
            SampleFormat::I16 => input_device.build_input_stream(
                &config,
                move |data: &[i16], _: &_| {
                    convert(&mut data.chunks(channels).map(|f| {
                        f.iter().map(|&s| s as f32 / i16::MAX as f32).sum::<f32>() / channels as f32
                    }))
                },
                error,
                None,
            ),
            format => return Err(eyre!("unsupported microphone format {format}")),
        }
        .wrap_err("opening the microphone")?;

        let output_config = output_device.default_output_config().wrap_err("speaker config")?;
        let out_channels = output_config.channels() as usize;
        let out_step = RATE / output_config.sample_rate().0 as f32;
        let playback = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let queue = playback.clone();
        let mut out_phase = 0.0f32;
        let mut current = 0.0f32;
        let mut next_sample = move || {
            out_phase += out_step;
            while out_phase >= 1.0 {
                out_phase -= 1.0;
                current = queue.lock().unwrap().pop_front().unwrap_or(0.0);
            }
            current
        };
        let config = output_config.config();
        let output = match output_config.sample_format() {
            SampleFormat::F32 => output_device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &_| {
                    for frame in data.chunks_mut(out_channels) {
                        frame.fill(next_sample());
                    }
                },
                error,
                None,
            ),
            SampleFormat::I16 => output_device.build_output_stream(
                &config,
                move |data: &mut [i16], _: &_| {
                    for frame in data.chunks_mut(out_channels) {
                        frame.fill((next_sample() * i16::MAX as f32) as i16);
                    }
                },
                error,
                None,
            ),
            format => return Err(eyre!("unsupported speaker format {format}")),
        }
        .wrap_err("opening the speaker")?;

        input.play()?;
        output.play()?;
        Ok(Self { _input: input, _output: output, captured, playback })
    }
}
