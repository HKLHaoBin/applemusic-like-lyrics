#![allow(unused)]

use std::sync::{
    atomic::{AtomicBool, AtomicU8},
    Arc,
};

use super::resampler::SincFixedOutResampler;
use cpal::{traits::*, *};
use rb::*;
use symphonia::core::{
    audio::{AsAudioBufferRef, AudioBuffer, AudioBufferRef, Channels, RawSample, SignalSpec},
    conv::{ConvertibleSample, IntoSample},
};
use tokio::sync::mpsc::Sender;
use tracing::*;

pub trait AudioOutput {
    fn get_sample_name(&self) -> &'static str;
    fn stream_config(&self) -> &StreamConfig;
    fn sample_format(&self) -> SampleFormat;
    fn stream(&self) -> &Stream;
    fn is_dead(&self) -> bool;
    fn stream_mut(&mut self) -> &mut Stream;
    fn set_volume(&mut self, volume: f64);
    fn volume(&self) -> f64;
    fn write(&mut self, decoded: symphonia::core::audio::AudioBufferRef<'_>);
    fn flush(&mut self);
}

pub struct AudioStreamPlayer<T: AudioOutputSample> {
    config: StreamConfig,
    sample_format: SampleFormat,
    stream: Stream,
    is_dead: Arc<AtomicBool>,
    prod: rb::Producer<T>,
    volume: Arc<AtomicU8>,
    resampler: Option<SincFixedOutResampler<T>>,
    resampler_target_channels: usize,
    resampler_duration: usize,
    resampler_spec: SignalSpec,
}

pub trait AudioOutputSample:
    SizedSample
    + ConvertibleSample
    + IntoSample<f32>
    + RawSample
    + std::marker::Send
    + Default
    + 'static
{
}

impl AudioOutputSample for i8 {}
impl AudioOutputSample for i16 {}
impl AudioOutputSample for i32 {}
// impl AudioOutputSample for i64 {}
impl AudioOutputSample for u8 {}
impl AudioOutputSample for u16 {}
impl AudioOutputSample for u32 {}
// impl AudioOutputSample for u64 {}
impl AudioOutputSample for f32 {}
impl AudioOutputSample for f64 {}

impl<T: AudioOutputSample> AudioOutput for AudioStreamPlayer<T> {
    fn get_sample_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }

    fn stream_config(&self) -> &StreamConfig {
        &self.config
    }

    fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }

    fn stream(&self) -> &Stream {
        &self.stream
    }

    fn stream_mut(&mut self) -> &mut Stream {
        &mut self.stream
    }

    fn is_dead(&self) -> bool {
        self.is_dead.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set_volume(&mut self, volume: f64) {
        let volume = (volume * 255.).clamp(0., 255.) as u8;
        self.volume
            .store(volume, std::sync::atomic::Ordering::SeqCst);
    }

    fn volume(&self) -> f64 {
        self.volume.load(std::sync::atomic::Ordering::SeqCst) as f64 / 255.
    }

    fn write(&mut self, decoded: symphonia::core::audio::AudioBufferRef<'_>) {
        if decoded.frames() == 0 {
            return;
        }

        let should_replace_resampler = self.resampler.is_none()
            || self.resampler_duration != decoded.capacity()
            || &self.resampler_spec != decoded.spec()
            || self.resampler_target_channels != self.config.channels as usize;

        if should_replace_resampler {
            self.resampler = Some(SincFixedOutResampler::<T>::new_sinc_fixed(
                *decoded.spec(),
                self.config.sample_rate.0 as _,
                self.config.channels as _,
                decoded.capacity() as _,
            ));
            info!(
                "将会重采样 {}hz ({} channels) [{}] -> {}hz ({} channels) [{}]",
                decoded.spec().rate,
                decoded.spec().channels.count(),
                get_buffer_format(&decoded),
                self.config.sample_rate.0,
                self.config.channels,
                self.get_sample_name(),
            );
            self.resampler_duration = decoded.capacity();
            self.resampler_spec = *decoded.spec();
            self.resampler_target_channels = self.config.channels as _;
        }

        let rsp = self.resampler.as_mut().unwrap();

        rsp.resample(&decoded);

        while let Some(mut buf) = rsp.flush() {
            while let Some(written) = self.prod.write_blocking(buf) {
                buf = &buf[written..];
            }
        }
    }

    fn flush(&mut self) {}
}

fn get_buffer_format(decoded: &symphonia::core::audio::AudioBufferRef<'_>) -> &'static str {
    match decoded {
        symphonia::core::audio::AudioBufferRef::U8(_) => "u8",
        symphonia::core::audio::AudioBufferRef::U16(_) => "u16",
        symphonia::core::audio::AudioBufferRef::U24(_) => "u24",
        symphonia::core::audio::AudioBufferRef::U32(_) => "u32",
        symphonia::core::audio::AudioBufferRef::S8(_) => "i8",
        symphonia::core::audio::AudioBufferRef::S16(_) => "i16",
        symphonia::core::audio::AudioBufferRef::S24(_) => "i24",
        symphonia::core::audio::AudioBufferRef::S32(_) => "i32",
        symphonia::core::audio::AudioBufferRef::F32(_) => "f32",
        symphonia::core::audio::AudioBufferRef::F64(_) => "f64",
    }
}

#[instrument(skip(output))]
fn init_audio_stream_inner<T: AudioOutputSample + Into<f64>>(
    output: Device,
    selected_config: StreamConfig,
) -> Box<dyn AudioOutput> {
    let channels = selected_config.channels;
    const RING_BUF_SIZE_MS: usize = 128;
    let ring_len =
        ((RING_BUF_SIZE_MS * selected_config.sample_rate.0 as usize) / 1000) * channels as usize;
    info!(
        "音频输出流环缓冲区大小为 {} 个样本（约为 {}ms 的缓冲）",
        ring_len, RING_BUF_SIZE_MS
    );
    let ring = rb::SpscRb::<T>::new(ring_len);
    let prod = ring.producer();
    let cons = ring.consumer();
    let is_dead = Arc::new(AtomicBool::new(false));
    let is_dead_c = is_dead.clone();
    let volume: Arc<_> = Arc::new(AtomicU8::new(u8::MAX >> 1));
    let volume_c = volume.clone();
    let stream = output
        .build_output_stream::<T, _, _>(
            &selected_config,
            move |data, _info| {
                let written = cons.read(data).unwrap_or(0);
                data[written..].fill(T::MID);
                let volume = volume_c.load(std::sync::atomic::Ordering::SeqCst) as f32 / 255.;
                data.iter_mut().for_each(|x| {
                    let s: f32 = (*x).into_sample();
                    *x = (s * volume).into_sample();
                });
            },
            move |err| {
                warn!("[WARN][AT] {err}");
                is_dead_c.store(true, std::sync::atomic::Ordering::SeqCst);
            },
            None,
        )
        .unwrap();
    info!("音频输出流准备完毕！");
    Box::new(AudioStreamPlayer {
        config: selected_config,
        sample_format: <T as SizedSample>::FORMAT,
        stream,
        prod,
        is_dead,
        volume,
        resampler: None,
        resampler_duration: 0,
        resampler_target_channels: 0,
        resampler_spec: SignalSpec {
            rate: 0,
            channels: Channels::empty(),
        },
    })
}

fn get_sample_format_quality_level(sample_format: SampleFormat) -> u8 {
    match sample_format {
        SampleFormat::I8 | SampleFormat::U8 => 0,
        SampleFormat::I16 | SampleFormat::U16 => 1,
        SampleFormat::I32 | SampleFormat::U32 => 2,
        SampleFormat::I64 | SampleFormat::U64 => 3,
        SampleFormat::F32 => 4,
        SampleFormat::F64 => 5,
        _ => unreachable!(),
    }
}

#[instrument]
pub fn init_audio_player(output_device_name: &str) -> Box<dyn AudioOutput> {
    let host = cpal::default_host();
    let output = if output_device_name.is_empty() {
        host.default_output_device().unwrap()
    } else {
        host.output_devices()
            .unwrap()
            .find(|d| d.name().unwrap_or_default() == output_device_name)
            .unwrap_or_else(|| host.default_output_device().unwrap())
    };
    info!(
        "已初始化输出音频设备为 {}",
        output.name().unwrap_or_default()
    );
    let configs = output
        .supported_output_configs()
        .unwrap()
        .collect::<Vec<_>>();
    let mut selected_config = StreamConfig {
        channels: 2,
        sample_rate: SampleRate(0),
        buffer_size: cpal::BufferSize::Default,
    };
    let mut selected_sample_format = SampleFormat::F32;
    for config in configs {
        info!(
            "已找到配置 {}hz-{}hz {} 通道 {}",
            config.min_sample_rate().0,
            config.max_sample_rate().0,
            config.channels(),
            config.sample_format()
        );
        if config.channels() > selected_config.channels
            || config.min_sample_rate().0 > selected_config.sample_rate.0
            || get_sample_format_quality_level(config.sample_format())
                > get_sample_format_quality_level(selected_sample_format)
        {
            selected_config.channels = config.channels();
            selected_config.sample_rate.0 = config.min_sample_rate().0;
            selected_sample_format = config.sample_format();
        }
    }
    info!(
        "尝试通过配置 {}hz {} 通道 {} 格式创建输出流",
        selected_config.sample_rate.0, selected_config.channels, selected_sample_format,
    );

    (match selected_sample_format {
        SampleFormat::I8 => init_audio_stream_inner::<i8>(output, selected_config),
        SampleFormat::I16 => init_audio_stream_inner::<i16>(output, selected_config),
        SampleFormat::I32 => init_audio_stream_inner::<i32>(output, selected_config),
        // SampleFormat::I64 => init_audio_stream_inner::<i64>(output, selected_config),
        SampleFormat::U8 => init_audio_stream_inner::<u8>(output, selected_config),
        SampleFormat::U16 => init_audio_stream_inner::<u16>(output, selected_config),
        SampleFormat::U32 => init_audio_stream_inner::<u32>(output, selected_config),
        // SampleFormat::U64 => init_audio_stream_inner::<u64>(output, selected_config),
        SampleFormat::F32 => init_audio_stream_inner::<f32>(output, selected_config),
        SampleFormat::F64 => init_audio_stream_inner::<f64>(output, selected_config),
        _ => unreachable!(),
    }) as _
}

pub enum OwnedAudioBuffer {
    U8(AudioBuffer<u8>),
    U16(AudioBuffer<u16>),
    U24(AudioBuffer<symphonia::core::sample::u24>),
    U32(AudioBuffer<u32>),
    S8(AudioBuffer<i8>),
    S16(AudioBuffer<i16>),
    S24(AudioBuffer<symphonia::core::sample::i24>),
    S32(AudioBuffer<i32>),
    F32(AudioBuffer<f32>),
    F64(AudioBuffer<f64>),
}

impl AsAudioBufferRef for OwnedAudioBuffer {
    fn as_audio_buffer_ref(&self) -> AudioBufferRef<'_> {
        match self {
            OwnedAudioBuffer::U8(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::U16(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::U24(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::U32(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::S8(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::S16(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::S24(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::S32(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::F32(x) => x.as_audio_buffer_ref(),
            OwnedAudioBuffer::F64(x) => x.as_audio_buffer_ref(),
        }
    }
}

enum AudioOutputMessage {
    Write(usize, OwnedAudioBuffer),
    SetVolume(f64),
}

#[derive(Debug, Clone)]
pub struct AudioOutputSender {
    sender: Sender<AudioOutputMessage>,
}

impl AudioOutputSender {
    pub async fn write_ref(&self, id: usize, decoded: AudioBufferRef<'_>) -> anyhow::Result<()> {
        let buf = match decoded {
            AudioBufferRef::U8(x) => OwnedAudioBuffer::U8(x.into_owned()),
            AudioBufferRef::U16(x) => OwnedAudioBuffer::U16(x.into_owned()),
            AudioBufferRef::U24(x) => OwnedAudioBuffer::U24(x.into_owned()),
            AudioBufferRef::U32(x) => OwnedAudioBuffer::U32(x.into_owned()),
            AudioBufferRef::S8(x) => OwnedAudioBuffer::S8(x.into_owned()),
            AudioBufferRef::S16(x) => OwnedAudioBuffer::S16(x.into_owned()),
            AudioBufferRef::S24(x) => OwnedAudioBuffer::S24(x.into_owned()),
            AudioBufferRef::S32(x) => OwnedAudioBuffer::S32(x.into_owned()),
            AudioBufferRef::F32(x) => OwnedAudioBuffer::F32(x.into_owned()),
            AudioBufferRef::F64(x) => OwnedAudioBuffer::F64(x.into_owned()),
        };
        self.sender.send(AudioOutputMessage::Write(id, buf)).await?;
        Ok(())
    }

    pub async fn write(&self, id: usize, decoded: OwnedAudioBuffer) -> anyhow::Result<()> {
        self.sender
            .send(AudioOutputMessage::Write(id, decoded))
            .await?;
        Ok(())
    }

    pub async fn set_volume(&self, volume: f64) -> anyhow::Result<()> {
        self.sender
            .send(AudioOutputMessage::SetVolume(volume))
            .await?;
        Ok(())
    }
}

pub fn create_audio_output_thread() -> AudioOutputSender {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AudioOutputMessage>(1);
    tokio::runtime::Handle::current().spawn_blocking(move || {
        let mut output = init_audio_player("");
        output.set_volume(0.5);
        output.stream().play().unwrap();
        let mut current_id = 0;
        info!("音频线程正在开始工作！");
        while let Some(msg) = rx.blocking_recv() {
            match msg {
                AudioOutputMessage::Write(id, decoded) => {
                    if id >= current_id || id == 0 {
                        current_id = id;
                        if output.is_dead() {
                            info!("现有输出设备已断开，正在重新初始化播放器");
                            output = init_audio_player("");
                            continue;
                        }
                        output.write(decoded.as_audio_buffer_ref());
                    }
                }
                AudioOutputMessage::SetVolume(volume) => {
                    output.set_volume(volume);
                }
            }
        }
        info!("所有接收者已关闭，音频线程即将结束！");
    });
    AudioOutputSender { sender: tx }
}
