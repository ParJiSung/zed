use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use credentials_provider::CredentialsProvider;
use futures::channel::mpsc;
use gpui::{AsyncApp, SharedString};
use gpui_tokio::Tokio;
use http_client::HttpClient;
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::{
    authentication::Credentials,
    config::{DeviceType, SessionConfig},
    session::Session,
};
use librespot_metadata::audio::UniqueFields;
use librespot_playback::{
    audio_backend::{Sink, SinkError, SinkResult},
    config::PlayerConfig,
    convert::Converter,
    decoder::AudioPacket,
    mixer::{self, MixerConfig},
    player::{Player, PlayerEvent},
};
use librespot_protocol::authentication::AuthenticationType;
use parking_lot::Mutex;
use rodio::{ChannelCount, MixerDeviceSink, SampleRate, Source, nz};
use util::ResultExt as _;

use crate::spotify_api::authorize;

const PLAYER_CREDENTIALS_KEY: &str = "https://accounts.spotify.com/zed-spotify-panel-player";
/// librespot's desktop client allows this loopback redirect path.
const STREAMING_CALLBACK_PATH: &str = "/login";
const CHANNELS: ChannelCount = nz!(2);
const SAMPLE_RATE: SampleRate = nz!(44_100);
/// About half a second of stereo audio. Keeping the buffer short makes pause
/// and skip take effect quickly, because queued audio would otherwise keep
/// playing after librespot has moved on.
const MAX_BUFFERED_SAMPLES: usize = 44_100;

/// A change reported by the built-in player itself, available immediately
/// rather than after the Web API catches up.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerUpdate {
    Playing(bool),
    Track {
        title: SharedString,
        artists: SharedString,
        album: SharedString,
        art_url: Option<SharedString>,
    },
}

/// A Spotify Connect device that plays inside Zed, so no Spotify app is needed.
pub struct SpotifyPlayer {
    spirc: Spirc,
}

impl SpotifyPlayer {
    /// Connects to Spotify and registers this Zed as a playback device.
    /// Returns the player along with credentials that can be stored and reused
    /// for later connections instead of logging in through the browser again.
    pub async fn start(
        credentials: Credentials,
        device_name: SharedString,
        cx: &AsyncApp,
    ) -> Result<(Self, Credentials, mpsc::UnboundedReceiver<PlayerUpdate>)> {
        let name = device_name.to_string();
        let (update_sender, update_receiver) = mpsc::unbounded();
        let (spirc, reusable_credentials) = cx
            .update(|cx| {
                Tokio::spawn_result(cx, async move {
                    let session = Session::new(SessionConfig::default(), None);
                    let mixer = mixer::find(None).context("no audio mixer is available")?(
                        MixerConfig::default(),
                    )?;
                    let player = Player::new(
                        PlayerConfig::default(),
                        session.clone(),
                        mixer.get_soft_volume(),
                        || Box::new(ZedAudioSink::new()) as Box<dyn Sink>,
                    );
                    let mut player_events = player.get_player_event_channel();
                    tokio::spawn(async move {
                        while let Some(event) = player_events.recv().await {
                            if let Some(update) = player_update(event)
                                && update_sender.unbounded_send(update).is_err()
                            {
                                break;
                            }
                        }
                    });
                    // Registering as a speaker makes Spotify offer to start a
                    // Jam whenever playback moves to Zed.
                    let connect_config = ConnectConfig {
                        name,
                        device_type: DeviceType::Computer,
                        ..ConnectConfig::default()
                    };
                    let (spirc, spirc_task) =
                        Spirc::new(connect_config, session.clone(), credentials, player, mixer)
                            .await
                            .map_err(|error| anyhow!("connecting to Spotify: {error}"))?;
                    tokio::spawn(spirc_task);
                    let reusable_credentials = Credentials {
                        username: Some(session.username()),
                        auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
                        auth_data: session.auth_data(),
                    };
                    anyhow::Ok((spirc, reusable_credentials))
                })
            })
            .await?;
        Ok((Self { spirc }, reusable_credentials, update_receiver))
    }

    pub fn play(&self) -> Result<()> {
        self.spirc
            .play()
            .map_err(|error| anyhow!("resuming playback: {error}"))
    }

    pub fn pause(&self) -> Result<()> {
        self.spirc
            .pause()
            .map_err(|error| anyhow!("pausing playback: {error}"))
    }

    pub fn next(&self) -> Result<()> {
        self.spirc
            .next()
            .map_err(|error| anyhow!("skipping to the next track: {error}"))
    }

    pub fn previous(&self) -> Result<()> {
        self.spirc
            .prev()
            .map_err(|error| anyhow!("going back a track: {error}"))
    }
}

impl Drop for SpotifyPlayer {
    fn drop(&mut self) {
        self.spirc
            .shutdown()
            .map_err(|error| anyhow!("stopping the Spotify player: {error}"))
            .log_err();
    }
}

fn player_update(event: PlayerEvent) -> Option<PlayerUpdate> {
    match event {
        PlayerEvent::Playing { .. } => Some(PlayerUpdate::Playing(true)),
        PlayerEvent::Paused { .. } | PlayerEvent::Stopped { .. } => {
            Some(PlayerUpdate::Playing(false))
        }
        PlayerEvent::TrackChanged { audio_item } => {
            let (artists, album) = match &audio_item.unique_fields {
                UniqueFields::Track { artists, album, .. } => (
                    artists
                        .iter()
                        .map(|artist| artist.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    album.clone(),
                ),
                UniqueFields::Local { artists, album, .. } => (
                    artists.clone().unwrap_or_default(),
                    album.clone().unwrap_or_default(),
                ),
                UniqueFields::Episode { show_name, .. } => (show_name.clone(), String::new()),
            };
            // Covers are listed largest first. The panel shows the art at
            // 72px, so take the smallest one that is still at least that big.
            let art_url = audio_item
                .covers
                .iter()
                .rev()
                .find(|cover| cover.width >= 72)
                .or_else(|| audio_item.covers.first())
                .map(|cover| cover.url.clone().into());
            Some(PlayerUpdate::Track {
                title: audio_item.name.clone().into(),
                artists: artists.into(),
                album: album.into(),
                art_url,
            })
        }
        _ => None,
    }
}

/// The name this Zed shows in Spotify's device list. Including the host name
/// keeps several computers running Zed apart.
pub fn device_name() -> SharedString {
    let host = sysinfo::System::host_name().unwrap_or_else(|| "this computer".to_string());
    format!("Zed ({host})").into()
}

/// librespot only accepts logins issued to Spotify's own desktop client, so
/// playback needs its own browser login, separate from the Web API one.
pub async fn streaming_login(http: &Arc<dyn HttpClient>, cx: &AsyncApp) -> Result<Credentials> {
    let client_id = SessionConfig::default().client_id;
    let response = authorize(http, &client_id, "streaming", STREAMING_CALLBACK_PATH, cx).await?;
    Ok(Credentials::with_access_token(response.access_token))
}

pub async fn load_credentials(
    provider: &Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> Result<Option<Credentials>> {
    let Some((_, bytes)) = provider
        .read_credentials(PLAYER_CREDENTIALS_KEY, cx)
        .await?
    else {
        return Ok(None);
    };
    let credentials =
        serde_json::from_slice(&bytes).context("reading the stored Spotify player login")?;
    Ok(Some(credentials))
}

pub async fn store_credentials(
    provider: &Arc<dyn CredentialsProvider>,
    credentials: &Credentials,
    cx: &AsyncApp,
) -> Result<()> {
    let bytes = serde_json::to_vec(credentials).context("serializing the Spotify player login")?;
    provider
        .write_credentials(
            PLAYER_CREDENTIALS_KEY,
            credentials.username.as_deref().unwrap_or("spotify"),
            &bytes,
            cx,
        )
        .await
        .context("saving the Spotify player login to the keychain")
}

pub async fn delete_credentials(
    provider: &Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> Result<()> {
    provider
        .delete_credentials(PLAYER_CREDENTIALS_KEY, cx)
        .await
}

/// Feeds librespot's decoded audio into Zed's own audio output. librespot's
/// bundled backends link a different version of the platform audio libraries
/// than Zed does, so they cannot be built into the same binary.
struct ZedAudioSink {
    buffer: Arc<Mutex<VecDeque<f32>>>,
    output: Option<MixerDeviceSink>,
}

impl ZedAudioSink {
    fn new() -> Self {
        Self {
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            output: None,
        }
    }
}

impl Sink for ZedAudioSink {
    fn start(&mut self) -> SinkResult<()> {
        if self.output.is_none() {
            let mut output = rodio::DeviceSinkBuilder::open_default_sink()
                .map_err(|error| SinkError::ConnectionRefused(error.to_string()))?;
            output.log_on_drop(false);
            output.mixer().add(BufferedSamples {
                buffer: self.buffer.clone(),
            });
            self.output = Some(output);
        }
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.buffer.lock().clear();
        self.output = None;
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        if self.output.is_none() {
            self.start()?;
        }
        let samples = packet
            .samples()
            .map_err(|error| SinkError::OnWrite(error.to_string()))?;
        let samples = converter.f64_to_f32(samples);
        // librespot decodes faster than real time; blocking here paces it to
        // the speed the output device consumes samples.
        while self.buffer.lock().len() > MAX_BUFFERED_SAMPLES {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.buffer.lock().extend(samples);
        Ok(())
    }
}

struct BufferedSamples {
    buffer: Arc<Mutex<VecDeque<f32>>>,
}

impl Iterator for BufferedSamples {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        // Silence while waiting for more audio keeps the output stream open.
        Some(self.buffer.lock().pop_front().unwrap_or(0.0))
    }
}

impl Source for BufferedSamples {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        CHANNELS
    }

    fn sample_rate(&self) -> SampleRate {
        SAMPLE_RATE
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}
