use std::fmt::Debug;

use concat_string::concat_string;

use self::audio_quality::AudioQuality;
use serde::*;

mod audio_quality;
mod fft_player;
mod output;
mod player;
mod resampler;

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum SongData {
    #[serde(rename_all = "camelCase")]
    Local {
        file_path: String,
        orig_order: usize,
    },
    /// 自定义的歌曲数据，可以交由宿主程序注册的歌曲元数据处理器处理
    #[serde(rename_all = "camelCase")]
    Custom {
        id: String,
        song_json_data: String,
        orig_order: usize,
    },
}

trait SongSource {
    fn get_id(&self) -> String;
    async fn fetch_source(&self) -> std::result::Result<Vec<u8>, String> {
        Err("未实现".to_string())
    }
}

impl SongSource for () {
    fn get_id(&self) -> String {
        "".to_string()
    }
}

impl SongSource for SongData {
    fn get_id(&self) -> String {
        match self {
            SongData::Local { file_path, .. } => format!("local:{:x}", md5::compute(file_path)),
            SongData::Custom { id, .. } => concat_string!("custom:", id),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
pub enum AudioThreadMessage {
    #[serde(rename_all = "camelCase")]
    ResumeAudio,
    #[serde(rename_all = "camelCase")]
    PauseAudio,
    #[serde(rename_all = "camelCase")]
    ResumeOrPauseAudio,
    #[serde(rename_all = "camelCase")]
    SeekAudio {
        position: f64,
    },
    #[serde(rename_all = "camelCase")]
    JumpToSong {
        song_index: usize,
    },
    #[serde(rename_all = "camelCase")]
    PrevSong,
    #[serde(rename_all = "camelCase")]
    NextSong,
    #[serde(rename_all = "camelCase")]
    SetPlaylist {
        songs: Vec<SongData>,
    },
    #[serde(rename_all = "camelCase")]
    SetVolume {
        volume: f64,
    },
    #[serde(rename_all = "camelCase")]
    SetVolumeRelative {
        volume: f64,
    },
    #[serde(rename_all = "camelCase")]
    SetAudioOutput {
        name: String,
    },
    SyncStatus,
}

pub type AudioPlayerEventSender =
    tokio::sync::mpsc::Sender<AudioThreadEventMessage<AudioThreadEvent>>;
pub type AudioPlayerMessageSender =
    tokio::sync::mpsc::Sender<AudioThreadEventMessage<AudioThreadMessage>>;
pub type AudioPlayerEventReceiver =
    tokio::sync::mpsc::Receiver<AudioThreadEventMessage<AudioThreadEvent>>;
pub type AudioPlayerMessageReceiver =
    tokio::sync::mpsc::Receiver<AudioThreadEventMessage<AudioThreadMessage>>;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type", content = "data")]
pub enum AudioThreadEvent {
    #[serde(rename_all = "camelCase")]
    PlayPosition { position: f64 },
    #[serde(rename_all = "camelCase")]
    LoadProgress { position: f64 },
    #[serde(rename_all = "camelCase")]
    LoadAudio {
        music_id: String,
        duration: f64,
        quality: AudioQuality,
    },
    #[serde(rename_all = "camelCase")]
    LoadingAudio { music_id: String },
    #[serde(rename_all = "camelCase")]
    SyncStatus {
        music_id: String,
        is_playing: bool,
        duration: f64,
        position: f64,
        volume: f64,
        load_position: f64,
        playlist: Vec<SongData>,
        playlist_inited: bool,
        quality: AudioQuality,
    },
    #[serde(rename_all = "camelCase")]
    PlayStatus { is_playing: bool },
    #[serde(rename_all = "camelCase")]
    SetDuration { duration: f64 },
    #[serde(rename_all = "camelCase")]
    LoadError { error: String },
    #[serde(rename_all = "camelCase")]
    VolumeChanged { volume: f64 },
    #[serde(rename = "fftData")]
    #[serde(rename_all = "camelCase")]
    FFTData { data: Vec<f32> },
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AudioThreadEventMessage<T> {
    callback_id: String,
    data: Option<T>,
}

impl<T> AudioThreadEventMessage<T> {
    pub fn new(callback_id: String, data: Option<T>) -> Self {
        Self { callback_id, data }
    }

    pub fn to<D>(self, new_data: D) -> AudioThreadEventMessage<D> {
        AudioThreadEventMessage {
            callback_id: self.callback_id,
            data: Some(new_data),
        }
    }

    pub fn to_none<D>(self) -> AudioThreadEventMessage<D> {
        AudioThreadEventMessage {
            callback_id: self.callback_id,
            data: None,
        }
    }
}
