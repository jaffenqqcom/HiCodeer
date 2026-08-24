use anyhow::{anyhow, Result};
use collections::HashMap;
use gpui::{App, Global};

use crate::DeviceId;
use crate::Sound;

pub trait RodioExt: rodio::Source + Sized {
    fn constant_params(
        self,
        _channel_count: rodio::ChannelCount,
        _sample_rate: rodio::SampleRate,
    ) -> Self {
        self
    }

    fn constant_samplerate(self, _sample_rate: rodio::SampleRate) -> Self {
        self
    }

    fn possibly_disconnected_channels_to_mono(self) -> Self {
        self
    }
}

impl<S: rodio::Source> RodioExt for S {}

#[derive(Default)]
pub struct Audio {
    source_cache: HashMap<Sound, ()>,
}

impl Global for Audio {}

impl Audio {
    pub fn play_sound(_sound: Sound, _cx: &mut App) {}

    pub fn end_call(_cx: &mut App) {}
}

pub fn init(_cx: &mut App) {
    // OHOS has no audio device management yet, so no initialization is needed. Matches the empty implementation of the non-OHOS audio_pipeline::init.
}

pub fn ensure_devices_initialized(cx: &mut App) {
    if !cx.has_global::<AvailableAudioDevices>() {
        cx.set_global(AvailableAudioDevices(Vec::new()));
    }
}

pub fn resolve_device(_device_id: Option<&DeviceId>, _input: bool) -> Result<Device> {
    Err(anyhow!("audio devices are not available on OHOS yet"))
}

pub fn open_input_stream(_device_id: Option<DeviceId>) -> Result<InputStream> {
    Err(anyhow!("audio input is not available on OHOS yet"))
}

pub fn open_test_output(_device_id: Option<DeviceId>) -> Result<TestOutput> {
    Err(anyhow!("audio output is not available on OHOS yet"))
}

pub struct InputStream;

pub struct TestOutput;

impl TestOutput {
    pub fn mixer(&self) -> TestMixer {
        TestMixer
    }
}

pub struct TestMixer;

impl TestMixer {
    pub fn add<S>(&self, _source: S) {}
}

impl Iterator for InputStream {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

impl rodio::Source for InputStream {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        crate::CHANNEL_COUNT
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        crate::SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

pub struct Device;

#[derive(Clone, Debug)]
pub struct AudioDeviceInfo {
    pub id: DeviceId,
    pub desc: DeviceDescription,
}

impl AudioDeviceInfo {
    pub fn matches_input(&self, is_input: bool) -> bool {
        if is_input {
            self.desc.supports_input()
        } else {
            self.desc.supports_output()
        }
    }

    pub fn matches(&self, id: &DeviceId, is_input: bool) -> bool {
        &self.id == id && self.matches_input(is_input)
    }
}

impl std::fmt::Display for AudioDeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.desc.name.fmt(f)
    }
}

#[derive(Default)]
pub struct AvailableAudioDevices(pub Vec<AudioDeviceInfo>);

impl Global for AvailableAudioDevices {}

#[derive(Clone, Debug)]
pub struct DeviceDescription {
    pub name: String,
    pub input: bool,
    pub output: bool,
}

impl DeviceDescription {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn supports_input(&self) -> bool {
        self.input
    }

    pub fn supports_output(&self) -> bool {
        self.output
    }
}
