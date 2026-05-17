use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "rustysynth")]
use std::{
    fs::File,
    io::Cursor,
    path::{Path, PathBuf},
};

#[cfg(feature = "sonivox")]
use std::{
    ffi::c_void,
    os::raw::{c_char, c_int, c_long, c_uint},
    ptr,
};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    SampleFormat, Stream, StreamConfig,
};
#[cfg(feature = "rustysynth")]
use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};

#[cfg(feature = "rustysynth")]
use crate::paths;

const MR_SUCCESS: i32 = 0;
const MR_FAILED: i32 = -1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundType {
    Midi,
    Wav,
    Mp3,
    Amr,
    Pcm,
}

impl TryFrom<i32> for SoundType {
    type Error = AudioError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Midi),
            1 => Ok(Self::Wav),
            2 => Ok(Self::Mp3),
            3 => Ok(Self::Amr),
            4 => Ok(Self::Pcm),
            other => Err(AudioError::UnsupportedSoundType(other)),
        }
    }
}

#[derive(Debug)]
pub enum AudioError {
    UnsupportedSoundType(i32),
    UnsupportedPlayback(SoundType),
    OutputDeviceUnavailable,
    DefaultOutputConfig(String),
    BuildStream(String),
    PlayStream(String),
    SoundFontNotFound,
    SoundFontLoad(String),
    MidiLoad(String),
    Synthesizer(String),
    Sonivox(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSoundType(ty) => write!(f, "unsupported sound type: {ty}"),
            Self::UnsupportedPlayback(ty) => write!(f, "sound type is not implemented yet: {ty:?}"),
            Self::OutputDeviceUnavailable => write!(f, "no default output audio device"),
            Self::DefaultOutputConfig(err) => {
                write!(f, "failed to get default audio config: {err}")
            }
            Self::BuildStream(err) => write!(f, "failed to build audio stream: {err}"),
            Self::PlayStream(err) => write!(f, "failed to start audio stream: {err}"),
            Self::SoundFontNotFound => write!(
                f,
                "no SoundFont found; set SKYMRP_SOUNDFONT to an .sf2 file"
            ),
            Self::SoundFontLoad(err) => write!(f, "failed to load SoundFont: {err}"),
            Self::MidiLoad(err) => write!(f, "failed to load MIDI data: {err}"),
            Self::Synthesizer(err) => write!(f, "failed to create MIDI synthesizer: {err}"),
            Self::Sonivox(err) => write!(f, "Sonivox MIDI backend failed: {err}"),
        }
    }
}

impl std::error::Error for AudioError {}

pub trait SoundBackend {
    fn play_sound(&mut self, ty: SoundType, data: &[u8], looped: bool) -> Result<(), AudioError>;
    fn stop_sound(&mut self, ty: SoundType) -> Result<(), AudioError>;
}

#[cfg(feature = "rustysynth")]
struct PlaybackState {
    sequencer: Option<MidiFileSequencer>,
    left: Vec<f32>,
    right: Vec<f32>,
}

#[cfg(feature = "rustysynth")]
impl PlaybackState {
    fn new() -> Self {
        Self {
            sequencer: None,
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    fn render(&mut self, frames: usize) -> (&[f32], &[f32]) {
        self.left.resize(frames, 0.0);
        self.right.resize(frames, 0.0);

        if let Some(sequencer) = self.sequencer.as_mut() {
            sequencer.render(&mut self.left, &mut self.right);
        } else {
            self.left.fill(0.0);
            self.right.fill(0.0);
        }

        (&self.left, &self.right)
    }

    fn stop(&mut self) {
        if let Some(sequencer) = self.sequencer.as_mut() {
            sequencer.stop();
        }
        self.sequencer = None;
        self.left.fill(0.0);
        self.right.fill(0.0);
    }
}

#[cfg(feature = "rustysynth")]
pub struct RustAudioBackend {
    sound_font: Arc<SoundFont>,
    sample_rate: i32,
    state: Arc<Mutex<PlaybackState>>,
    _stream: Stream,
}

#[cfg(feature = "rustysynth")]
impl RustAudioBackend {
    pub fn new() -> Result<Self, AudioError> {
        let sound_font_path = find_sound_font().ok_or(AudioError::SoundFontNotFound)?;
        let mut sound_font_file = File::open(&sound_font_path)
            .map_err(|err| AudioError::SoundFontLoad(err.to_string()))?;
        let sound_font = Arc::new(
            SoundFont::new(&mut sound_font_file)
                .map_err(|err| AudioError::SoundFontLoad(err.to_string()))?,
        );

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioError::OutputDeviceUnavailable)?;
        let supported = device
            .default_output_config()
            .map_err(|err| AudioError::DefaultOutputConfig(err.to_string()))?;
        let sample_rate = supported.sample_rate() as i32;
        let config = supported.config();
        let state = Arc::new(Mutex::new(PlaybackState::new()));
        let stream = build_stream(
            &device,
            &config,
            supported.sample_format(),
            Arc::clone(&state),
        )?;
        stream
            .play()
            .map_err(|err| AudioError::PlayStream(err.to_string()))?;

        log!(
            "Audio initialized: {:?}, {} Hz, SoundFont {}",
            supported.sample_format(),
            sample_rate,
            sound_font_path.display()
        );

        Ok(Self {
            sound_font,
            sample_rate,
            state,
            _stream: stream,
        })
    }

    fn play_midi(&mut self, data: &[u8], looped: bool) -> Result<(), AudioError> {
        let mut cursor = Cursor::new(data);
        let midi_file = Arc::new(
            MidiFile::new(&mut cursor).map_err(|err| AudioError::MidiLoad(err.to_string()))?,
        );
        let settings = SynthesizerSettings::new(self.sample_rate);
        let synthesizer = Synthesizer::new(&self.sound_font, &settings)
            .map_err(|err| AudioError::Synthesizer(err.to_string()))?;
        let mut sequencer = MidiFileSequencer::new(synthesizer);
        sequencer.play(&midi_file, looped);

        let mut state = self.state.lock().unwrap();
        state.sequencer = Some(sequencer);
        Ok(())
    }
}

#[cfg(feature = "sonivox")]
mod sonivox {
    use super::*;

    type EasResult = c_long;
    type EasHandle = *mut c_void;
    type EasDataHandle = *mut c_void;

    const EAS_SUCCESS: EasResult = 0;
    const EAS_STATE_STOPPED: c_long = 4;
    const EAS_STATE_ERROR: c_long = 7;

    #[repr(C)]
    struct EasFile {
        handle: *mut c_void,
        read_at: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, c_int) -> c_int>,
        size: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    }

    #[repr(C)]
    struct EasLibConfig {
        lib_version: u32,
        checked_version: c_uint,
        max_voices: i32,
        num_channels: i32,
        sample_rate: i32,
        mix_buffer_size: i32,
        filter_enabled: c_uint,
        build_timestamp: u32,
        build_guid: *mut c_char,
    }

    extern "C" {
        fn EAS_Init(data: *mut EasDataHandle) -> EasResult;
        fn EAS_Config() -> *const EasLibConfig;
        fn EAS_Shutdown(data: EasDataHandle) -> EasResult;
        fn EAS_OpenFile(
            data: EasDataHandle,
            locator: *mut EasFile,
            stream: *mut EasHandle,
        ) -> EasResult;
        fn EAS_Prepare(data: EasDataHandle, stream: EasHandle) -> EasResult;
        fn EAS_Render(
            data: EasDataHandle,
            output: *mut i16,
            requested_frames: i32,
            generated_frames: *mut i32,
        ) -> EasResult;
        fn EAS_CloseFile(data: EasDataHandle, stream: EasHandle) -> EasResult;
        fn EAS_State(data: EasDataHandle, stream: EasHandle, state: *mut c_long) -> EasResult;
        fn EAS_SetRepeat(data: EasDataHandle, stream: EasHandle, repeat_count: i32) -> EasResult;
    }

    struct MemoryMidi {
        data: Vec<u8>,
    }

    unsafe extern "C" fn read_at(
        handle: *mut c_void,
        buf: *mut c_void,
        offset: c_int,
        size: c_int,
    ) -> c_int {
        if handle.is_null() || buf.is_null() || offset < 0 || size <= 0 {
            return 0;
        }

        let midi = &*(handle as *const MemoryMidi);
        let start = offset as usize;
        if start >= midi.data.len() {
            return 0;
        }

        let end = start.saturating_add(size as usize).min(midi.data.len());
        let len = end - start;
        ptr::copy_nonoverlapping(midi.data[start..end].as_ptr(), buf as *mut u8, len);
        len as c_int
    }

    unsafe extern "C" fn size(handle: *mut c_void) -> c_int {
        if handle.is_null() {
            return 0;
        }
        let midi = &*(handle as *const MemoryMidi);
        midi.data.len().min(c_int::MAX as usize) as c_int
    }

    fn check(result: EasResult, operation: &str) -> Result<(), AudioError> {
        if result == EAS_SUCCESS {
            Ok(())
        } else {
            Err(AudioError::Sonivox(format!(
                "{operation} returned {result}"
            )))
        }
    }

    pub struct SonivoxSequence {
        data: EasDataHandle,
        stream: EasHandle,
        _source: Box<MemoryMidi>,
    }

    unsafe impl Send for SonivoxSequence {}

    impl SonivoxSequence {
        fn new(midi_data: &[u8], looped: bool) -> Result<Self, AudioError> {
            let mut data = ptr::null_mut();
            check(unsafe { EAS_Init(&mut data) }, "EAS_Init")?;

            let mut source = Box::new(MemoryMidi {
                data: midi_data.to_vec(),
            });
            let mut locator = EasFile {
                handle: source.as_mut() as *mut MemoryMidi as *mut c_void,
                read_at: Some(read_at),
                size: Some(size),
            };
            let mut stream = ptr::null_mut();

            let open_result = unsafe { EAS_OpenFile(data, &mut locator, &mut stream) };
            if let Err(err) = check(open_result, "EAS_OpenFile") {
                unsafe {
                    EAS_Shutdown(data);
                }
                return Err(err);
            }

            let repeat_count = if looped { -1 } else { 0 };
            if let Err(err) = check(
                unsafe { EAS_SetRepeat(data, stream, repeat_count) },
                "EAS_SetRepeat",
            ) {
                unsafe {
                    EAS_CloseFile(data, stream);
                    EAS_Shutdown(data);
                }
                return Err(err);
            }

            if let Err(err) = check(unsafe { EAS_Prepare(data, stream) }, "EAS_Prepare") {
                unsafe {
                    EAS_CloseFile(data, stream);
                    EAS_Shutdown(data);
                }
                return Err(err);
            }

            Ok(Self {
                data,
                stream,
                _source: source,
            })
        }

        fn render(&mut self, frames: usize, channels: usize, output: &mut [i16]) -> usize {
            if self.is_stopped() {
                return 0;
            }

            let requested = frames.min(i32::MAX as usize) as i32;
            let mut generated = 0;
            let result =
                unsafe { EAS_Render(self.data, output.as_mut_ptr(), requested, &mut generated) };
            if result != EAS_SUCCESS || generated <= 0 {
                return 0;
            }

            (generated as usize).min(output.len() / channels.max(1))
        }

        fn is_stopped(&self) -> bool {
            let mut state = 0;
            let result = unsafe { EAS_State(self.data, self.stream, &mut state) };
            result != EAS_SUCCESS || state == EAS_STATE_STOPPED || state == EAS_STATE_ERROR
        }
    }

    impl Drop for SonivoxSequence {
        fn drop(&mut self) {
            unsafe {
                if !self.stream.is_null() {
                    EAS_CloseFile(self.data, self.stream);
                }
                if !self.data.is_null() {
                    EAS_Shutdown(self.data);
                }
            }
        }
    }

    struct SonivoxPlaybackState {
        sequence: Option<SonivoxSequence>,
        channels: usize,
        mix_buffer_size: usize,
        pcm: Vec<i16>,
        pending: Vec<i16>,
        pending_offset_frames: usize,
        left: Vec<f32>,
        right: Vec<f32>,
    }

    impl SonivoxPlaybackState {
        fn new(channels: usize, mix_buffer_size: usize) -> Self {
            Self {
                sequence: None,
                channels,
                mix_buffer_size,
                pcm: Vec::new(),
                pending: Vec::new(),
                pending_offset_frames: 0,
                left: Vec::new(),
                right: Vec::new(),
            }
        }

        fn render(&mut self, frames: usize) -> (&[f32], &[f32]) {
            self.left.resize(frames, 0.0);
            self.right.resize(frames, 0.0);

            if self.sequence.is_none() {
                self.left.fill(0.0);
                self.right.fill(0.0);
                return (&self.left, &self.right);
            }

            let mut rendered = 0;
            while rendered < frames {
                let copied = self.copy_pending(rendered, frames);
                if copied > 0 {
                    rendered += copied;
                    continue;
                }

                if self.sequence.is_none() {
                    break;
                }

                let samples = self.mix_buffer_size * self.channels;
                self.pcm.resize(samples, 0);

                let generated = self
                    .sequence
                    .as_mut()
                    .expect("sequence checked above")
                    .render(self.mix_buffer_size, self.channels, &mut self.pcm);
                if generated == 0 {
                    self.sequence = None;
                    break;
                }

                self.pending.clear();
                self.pending
                    .extend_from_slice(&self.pcm[..generated * self.channels]);
                self.pending_offset_frames = 0;

                if generated < self.mix_buffer_size {
                    self.sequence = None;
                }
            }

            if rendered < frames {
                self.left[rendered..].fill(0.0);
                self.right[rendered..].fill(0.0);
            }

            (&self.left, &self.right)
        }

        fn copy_pending(&mut self, output_offset: usize, requested_frames: usize) -> usize {
            let pending_frames = self.pending.len() / self.channels;
            if self.pending_offset_frames >= pending_frames {
                self.pending.clear();
                self.pending_offset_frames = 0;
                return 0;
            }

            let available = pending_frames - self.pending_offset_frames;
            let count = available.min(requested_frames - output_offset);
            for frame in 0..count {
                let src = (self.pending_offset_frames + frame) * self.channels;
                let left = self.pending[src] as f32 / i16::MAX as f32;
                let right = if self.channels > 1 {
                    self.pending[src + 1] as f32 / i16::MAX as f32
                } else {
                    left
                };
                self.left[output_offset + frame] = left;
                self.right[output_offset + frame] = right;
            }

            self.pending_offset_frames += count;
            if self.pending_offset_frames >= pending_frames {
                self.pending.clear();
                self.pending_offset_frames = 0;
            }

            count
        }

        fn stop(&mut self) {
            self.sequence = None;
            self.left.fill(0.0);
            self.right.fill(0.0);
            self.pcm.fill(0);
            self.pending.clear();
            self.pending_offset_frames = 0;
        }
    }

    pub struct SonivoxAudioBackend {
        state: Arc<Mutex<SonivoxPlaybackState>>,
        _stream: Stream,
    }

    impl SonivoxAudioBackend {
        pub fn new() -> Result<Self, AudioError> {
            let config = unsafe {
                EAS_Config()
                    .as_ref()
                    .ok_or_else(|| AudioError::Sonivox("EAS_Config returned null".to_string()))?
            };
            let sample_rate = config.sample_rate as u32;
            let channels = config.num_channels.max(1) as usize;
            let mix_buffer_size = config.mix_buffer_size.max(1) as usize;

            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or(AudioError::OutputDeviceUnavailable)?;
            let supported = device
                .default_output_config()
                .map_err(|err| AudioError::DefaultOutputConfig(err.to_string()))?;
            let mut stream_config = supported.config();
            stream_config.sample_rate = sample_rate;
            stream_config.channels = channels as u16;

            let state = Arc::new(Mutex::new(SonivoxPlaybackState::new(
                channels,
                mix_buffer_size,
            )));
            let stream = build_sonivox_stream(
                &device,
                &stream_config,
                supported.sample_format(),
                Arc::clone(&state),
            )?;
            stream
                .play()
                .map_err(|err| AudioError::PlayStream(err.to_string()))?;

            log!(
                "Audio initialized: {:?}, {} Hz, Sonivox embedded wavetable",
                supported.sample_format(),
                sample_rate
            );

            Ok(Self {
                state,
                _stream: stream,
            })
        }

        fn play_midi(&mut self, data: &[u8], looped: bool) -> Result<(), AudioError> {
            let sequence = SonivoxSequence::new(data, looped)?;
            let mut state = self.state.lock().unwrap();
            state.sequence = Some(sequence);
            Ok(())
        }
    }

    impl SoundBackend for SonivoxAudioBackend {
        fn play_sound(
            &mut self,
            ty: SoundType,
            data: &[u8],
            looped: bool,
        ) -> Result<(), AudioError> {
            match ty {
                SoundType::Midi => self.play_midi(data, looped),
                other => Err(AudioError::UnsupportedPlayback(other)),
            }
        }

        fn stop_sound(&mut self, ty: SoundType) -> Result<(), AudioError> {
            match ty {
                SoundType::Midi => {
                    self.state.lock().unwrap().stop();
                    Ok(())
                }
                other => Err(AudioError::UnsupportedPlayback(other)),
            }
        }
    }

    fn build_sonivox_stream(
        device: &cpal::Device,
        config: &StreamConfig,
        sample_format: SampleFormat,
        state: Arc<Mutex<SonivoxPlaybackState>>,
    ) -> Result<Stream, AudioError> {
        let err_fn = |err| {
            log!("audio stream error: {err}");
        };
        let channels = config.channels as usize;
        match sample_format {
            SampleFormat::F32 => device
                .build_output_stream(
                    config,
                    move |output: &mut [f32], _| write_sonivox_output_f32(output, channels, &state),
                    err_fn,
                    None,
                )
                .map_err(|err| AudioError::BuildStream(err.to_string())),
            SampleFormat::I16 => device
                .build_output_stream(
                    config,
                    move |output: &mut [i16], _| write_sonivox_output_i16(output, channels, &state),
                    err_fn,
                    None,
                )
                .map_err(|err| AudioError::BuildStream(err.to_string())),
            SampleFormat::U16 => device
                .build_output_stream(
                    config,
                    move |output: &mut [u16], _| write_sonivox_output_u16(output, channels, &state),
                    err_fn,
                    None,
                )
                .map_err(|err| AudioError::BuildStream(err.to_string())),
            other => Err(AudioError::BuildStream(format!(
                "unsupported output sample format: {other:?}"
            ))),
        }
    }

    fn write_sonivox_output_f32(
        output: &mut [f32],
        channels: usize,
        state: &Mutex<SonivoxPlaybackState>,
    ) {
        if channels == 0 {
            return;
        }

        let frames = output.len() / channels;
        let Ok(mut state) = state.try_lock() else {
            output.fill(0.0);
            return;
        };
        let (left, right) = state.render(frames);

        for frame in 0..frames {
            for channel in 0..channels {
                output[frame * channels + channel] = if channel == 0 {
                    left[frame]
                } else {
                    right[frame]
                };
            }
        }
    }

    fn write_sonivox_output_i16(
        output: &mut [i16],
        channels: usize,
        state: &Mutex<SonivoxPlaybackState>,
    ) {
        let mut buffer = vec![0.0; output.len()];
        write_sonivox_output_f32(&mut buffer, channels, state);
        for (dst, sample) in output.iter_mut().zip(buffer) {
            *dst = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        }
    }

    fn write_sonivox_output_u16(
        output: &mut [u16],
        channels: usize,
        state: &Mutex<SonivoxPlaybackState>,
    ) {
        let mut buffer = vec![0.0; output.len()];
        write_sonivox_output_f32(&mut buffer, channels, state);
        for (dst, sample) in output.iter_mut().zip(buffer) {
            *dst = ((sample.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16;
        }
    }
}

#[cfg(feature = "rustysynth")]
impl SoundBackend for RustAudioBackend {
    fn play_sound(&mut self, ty: SoundType, data: &[u8], looped: bool) -> Result<(), AudioError> {
        match ty {
            SoundType::Midi => self.play_midi(data, looped),
            other => Err(AudioError::UnsupportedPlayback(other)),
        }
    }

    fn stop_sound(&mut self, ty: SoundType) -> Result<(), AudioError> {
        match ty {
            SoundType::Midi => {
                self.state.lock().unwrap().stop();
                Ok(())
            }
            other => Err(AudioError::UnsupportedPlayback(other)),
        }
    }
}

#[cfg(feature = "rustysynth")]
fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    state: Arc<Mutex<PlaybackState>>,
) -> Result<Stream, AudioError> {
    let err_fn = |err| {
        log!("audio stream error: {err}");
    };
    let channels = config.channels as usize;
    match sample_format {
        SampleFormat::F32 => device
            .build_output_stream(
                config,
                move |output: &mut [f32], _| write_output_f32(output, channels, &state),
                err_fn,
                None,
            )
            .map_err(|err| AudioError::BuildStream(err.to_string())),
        SampleFormat::I16 => device
            .build_output_stream(
                config,
                move |output: &mut [i16], _| write_output_i16(output, channels, &state),
                err_fn,
                None,
            )
            .map_err(|err| AudioError::BuildStream(err.to_string())),
        SampleFormat::U16 => device
            .build_output_stream(
                config,
                move |output: &mut [u16], _| write_output_u16(output, channels, &state),
                err_fn,
                None,
            )
            .map_err(|err| AudioError::BuildStream(err.to_string())),
        other => Err(AudioError::BuildStream(format!(
            "unsupported output sample format: {other:?}"
        ))),
    }
}

#[cfg(feature = "rustysynth")]
fn write_output_f32(output: &mut [f32], channels: usize, state: &Mutex<PlaybackState>) {
    if channels == 0 {
        return;
    }

    let frames = output.len() / channels;
    let Ok(mut state) = state.try_lock() else {
        output.fill(0.0);
        return;
    };
    let (left, right) = state.render(frames);

    for frame in 0..frames {
        for channel in 0..channels {
            output[frame * channels + channel] = if channel == 0 {
                left[frame]
            } else {
                right[frame]
            };
        }
    }
}

#[cfg(feature = "rustysynth")]
fn write_output_i16(output: &mut [i16], channels: usize, state: &Mutex<PlaybackState>) {
    let mut buffer = vec![0.0; output.len()];
    write_output_f32(&mut buffer, channels, state);
    for (dst, sample) in output.iter_mut().zip(buffer) {
        *dst = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    }
}

#[cfg(feature = "rustysynth")]
fn write_output_u16(output: &mut [u16], channels: usize, state: &Mutex<PlaybackState>) {
    let mut buffer = vec![0.0; output.len()];
    write_output_f32(&mut buffer, channels, state);
    for (dst, sample) in output.iter_mut().zip(buffer) {
        *dst = ((sample.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16;
    }
}

#[cfg(feature = "rustysynth")]
fn find_sound_font() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SKYMRP_SOUNDFONT") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    if let Ok(dir) = std::env::var("SKYMRP_SOUNDFONT_DIR") {
        if let Some(path) = find_sound_font_in_dir(Path::new(&dir)) {
            return Some(path);
        }
    }

    [
        PathBuf::from("soundfonts"),
        paths::mythroad_dir().join("soundfonts"),
    ]
    .into_iter()
    .find_map(|path| find_sound_font_in_dir(&path))
}

#[cfg(feature = "rustysynth")]
fn find_sound_font_in_dir(dir: &Path) -> Option<PathBuf> {
    ["soundfont.sf2", "default.sf2"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

enum AudioBackend {
    #[cfg(feature = "rustysynth")]
    SoundFont(RustAudioBackend),
    #[cfg(feature = "sonivox")]
    Sonivox(sonivox::SonivoxAudioBackend),
}

impl AudioBackend {
    fn new() -> Result<Self, AudioError> {
        let preferred = std::env::var("SKYMRP_MIDI_BACKEND").unwrap_or_default();

        #[cfg(feature = "rustysynth")]
        if preferred == "rustysynth" {
            match RustAudioBackend::new() {
                Ok(backend) => return Ok(Self::SoundFont(backend)),
                Err(err) => {
                    log!("SoundFont backend unavailable, falling back: {err}");
                }
            }
        }

        #[cfg(feature = "sonivox")]
        {
            match sonivox::SonivoxAudioBackend::new() {
                Ok(backend) => return Ok(Self::Sonivox(backend)),
                Err(err) if preferred == "sonivox" => return Err(err),
                Err(err) => {
                    log!("Sonivox backend unavailable, falling back: {err}");
                }
            }
        }

        #[cfg(feature = "rustysynth")]
        {
            RustAudioBackend::new().map(Self::SoundFont)
        }

        #[cfg(not(feature = "rustysynth"))]
        {
            Err(AudioError::UnsupportedPlayback(SoundType::Midi))
        }
    }
}

impl SoundBackend for AudioBackend {
    fn play_sound(&mut self, ty: SoundType, data: &[u8], looped: bool) -> Result<(), AudioError> {
        match self {
            #[cfg(feature = "rustysynth")]
            Self::SoundFont(backend) => backend.play_sound(ty, data, looped),
            #[cfg(feature = "sonivox")]
            Self::Sonivox(backend) => backend.play_sound(ty, data, looped),
            #[allow(unreachable_patterns)]
            _ => Err(AudioError::UnsupportedPlayback(ty)),
        }
    }

    fn stop_sound(&mut self, ty: SoundType) -> Result<(), AudioError> {
        match self {
            #[cfg(feature = "rustysynth")]
            Self::SoundFont(backend) => backend.stop_sound(ty),
            #[cfg(feature = "sonivox")]
            Self::Sonivox(backend) => backend.stop_sound(ty),
            #[allow(unreachable_patterns)]
            _ => Err(AudioError::UnsupportedPlayback(ty)),
        }
    }
}

static AUDIO_BACKEND: OnceLock<Mutex<Option<AudioBackend>>> = OnceLock::new();

fn backend() -> &'static Mutex<Option<AudioBackend>> {
    AUDIO_BACKEND.get_or_init(|| Mutex::new(None))
}

fn with_backend<F>(f: F) -> Result<(), AudioError>
where
    F: FnOnce(&mut AudioBackend) -> Result<(), AudioError>,
{
    let mut guard = backend().lock().unwrap();
    if guard.is_none() {
        *guard = Some(AudioBackend::new()?);
    }
    f(guard.as_mut().unwrap())
}

pub fn play_sound_from_guest(ty: i32, data: &[u8], looped: bool) -> i32 {
    let result = SoundType::try_from(ty).and_then(|sound_type| {
        with_backend(|backend| backend.play_sound(sound_type, data, looped))
    });

    match result {
        Ok(()) => MR_SUCCESS,
        Err(err) => {
            log!("mr_playSound failed: {err}");
            MR_FAILED
        }
    }
}

pub fn stop_sound_from_guest(ty: i32) -> i32 {
    let result = SoundType::try_from(ty).and_then(|sound_type| {
        let mut guard = backend().lock().unwrap();
        match guard.as_mut() {
            Some(backend) => backend.stop_sound(sound_type),
            None if sound_type == SoundType::Midi => Ok(()),
            None => Err(AudioError::UnsupportedPlayback(sound_type)),
        }
    });

    match result {
        Ok(()) => MR_SUCCESS,
        Err(err) => {
            log!("mr_stopSound failed: {err}");
            MR_FAILED
        }
    }
}
