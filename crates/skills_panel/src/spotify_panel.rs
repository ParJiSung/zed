use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use credentials_provider::CredentialsProvider;
use futures::StreamExt as _;
use gpui::{
    AnyElement, AsyncWindowContext, Entity, FocusHandle, ImageSource, Resource, SharedUri, Task,
    WeakEntity, img,
};
use http_client::HttpClient;
use librespot_core::authentication::Credentials;
use ui::{
    Button, ButtonSize, ButtonStyle, IconButton, ListItem, ListItemSpacing, TintColor, Tooltip,
    prelude::*,
};
use util::ResultExt as _;
use workspace::{Workspace, dock::DockPosition, notifications::DetachAndPromptErr as _};

use crate::{
    ToggleSpotifyFocus, empty_message, poll_task,
    spotify_api::{Playback, Playlist, SpotifyClient, Track, Transport},
    spotify_player::{self, PlayerUpdate, SpotifyPlayer},
};

/// How long the Web API may take to reflect a change made from this panel.
const WEB_API_SETTLE_TIME: Duration = Duration::from_millis(2500);
/// How soon to re-read the playback state after a Web API command.
const QUICK_REFRESH_DELAY: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Account {
    Loading,
    SignedOut,
    SigningIn,
    SignedIn,
}

#[derive(Clone, Debug, PartialEq)]
enum PlayerStatus {
    Stopped,
    Starting,
    Ready,
    Failed(SharedString),
}

#[derive(Clone, Debug, PartialEq)]
enum LibraryView {
    Playlists,
    Playlist(Playlist),
    Queue,
}

pub struct SpotifyPanel {
    focus_handle: FocusHandle,
    position: DockPosition,
    active: bool,
    http: Arc<dyn HttpClient>,
    credentials: Arc<dyn CredentialsProvider>,
    spotify: Option<SpotifyClient>,
    account: Account,
    player: Option<SpotifyPlayer>,
    player_status: PlayerStatus,
    device_name: SharedString,
    device_id: Option<String>,
    playback: Option<Playback>,
    library_view: LibraryView,
    playlists: Vec<Playlist>,
    tracks: Vec<Track>,
    queue: Vec<Track>,
    library_error: Option<SharedString>,
    library_task: Task<()>,
    player_task: Task<()>,
    refresh_task: Task<()>,
    /// When the panel last changed playback state itself. The Web API lags
    /// behind such changes, so polled state is ignored for a short while.
    last_local_change: Option<Instant>,
    _player_updates: Task<()>,
    _poll_task: Task<()>,
}

impl SpotifyPanel {
    pub(crate) async fn load(
        _workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        cx.update(|_window, cx| cx.new(Self::new))
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let http = cx.http_client();
        let credentials = zed_credentials_provider::global(cx);
        let mut panel = Self {
            focus_handle: cx.focus_handle(),
            position: DockPosition::Left,
            active: false,
            http: http.clone(),
            credentials: credentials.clone(),
            spotify: None,
            account: Account::Loading,
            player: None,
            player_status: PlayerStatus::Stopped,
            device_name: spotify_player::device_name(),
            device_id: None,
            playback: None,
            library_view: LibraryView::Playlists,
            playlists: Vec::new(),
            tracks: Vec::new(),
            queue: Vec::new(),
            library_error: None,
            library_task: Task::ready(()),
            player_task: Task::ready(()),
            refresh_task: Task::ready(()),
            last_local_change: None,
            _player_updates: Task::ready(()),
            _poll_task: poll_task(cx, |panel: &mut Self, cx| {
                if panel.active {
                    panel.refresh(cx);
                }
            }),
        };
        panel.library_task = cx.spawn(async move |this, cx| {
            let spotify = SpotifyClient::load(http, credentials.clone(), cx).await;
            let player_credentials = spotify_player::load_credentials(&credentials, cx).await;
            this.update(cx, |panel, cx| {
                match (spotify, player_credentials) {
                    (Ok(Some(spotify)), Ok(Some(player_credentials))) => {
                        panel.spotify = Some(spotify);
                        panel.account = Account::SignedIn;
                        panel.load_playlists(cx);
                        panel.start_player(player_credentials, cx);
                    }
                    // Either login alone is not enough to play inside Zed.
                    (Ok(_), Ok(_)) => panel.account = Account::SignedOut,
                    (Err(error), _) | (_, Err(error)) => {
                        panel.account = Account::SignedOut;
                        panel.library_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        panel
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(spotify) = self.spotify.clone() else {
            return;
        };
        self.refresh_task = cx.spawn(async move |this, cx| {
            let Some(playback) = spotify.playback(cx).await.log_err() else {
                return;
            };
            this.update(cx, |panel, cx| {
                let recently_changed = panel
                    .last_local_change
                    .is_some_and(|changed| changed.elapsed() < WEB_API_SETTLE_TIME);
                if !recently_changed && panel.playback != playback {
                    panel.playback = playback;
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    /// Re-reads the playback state soon, once Spotify has had a moment to
    /// apply a command sent through the Web API.
    fn refresh_soon(&mut self, cx: &mut Context<Self>) {
        self.refresh_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(QUICK_REFRESH_DELAY).await;
            this.update(cx, |panel, cx| panel.refresh(cx)).log_err();
        });
    }

    fn mark_local_change(&mut self, cx: &mut Context<Self>) {
        self.last_local_change = Some(Instant::now());
        cx.notify();
    }

    fn set_playing(&mut self, playing: bool, cx: &mut Context<Self>) {
        if let Some(playback) = &mut self.playback {
            playback.is_playing = playing;
        }
        self.mark_local_change(cx);
    }

    fn apply_player_update(&mut self, update: PlayerUpdate, cx: &mut Context<Self>) {
        match update {
            PlayerUpdate::Playing(playing) => {
                if let Some(playback) = &mut self.playback {
                    playback.is_playing = playing;
                    if playing {
                        playback.device_name = self.device_name.clone();
                    }
                }
            }
            PlayerUpdate::Track {
                title,
                artists,
                album,
                art_url,
            } => {
                let is_playing = self
                    .playback
                    .as_ref()
                    .is_none_or(|playback| playback.is_playing);
                self.playback = Some(Playback {
                    title,
                    artists,
                    album,
                    art_url,
                    is_playing,
                    device_name: self.device_name.clone(),
                });
            }
        }
        self.mark_local_change(cx);
    }

    /// The built-in player, if it is the device that is currently playing.
    fn local_player(&self) -> Option<&SpotifyPlayer> {
        let playback = self.playback.as_ref()?;
        if playback.device_name != self.device_name {
            return None;
        }
        self.player.as_ref()
    }

    /// Starts the built-in Spotify player. After a browser login the player
    /// returns reusable credentials, which are kept so later starts are silent.
    fn start_player(&mut self, credentials: Credentials, cx: &mut Context<Self>) {
        self.player = None;
        self.device_id = None;
        self.player_status = PlayerStatus::Starting;
        let device_name = self.device_name.clone();
        let credentials_provider = self.credentials.clone();
        self.player_task = cx.spawn(async move |this, cx| {
            let result = SpotifyPlayer::start(credentials, device_name, cx).await;
            let result = match result {
                Ok((player, reusable_credentials, updates)) => {
                    spotify_player::store_credentials(
                        &credentials_provider,
                        &reusable_credentials,
                        cx,
                    )
                    .await
                    .log_err();
                    Ok((player, updates))
                }
                Err(error) => Err(error),
            };
            this.update(cx, |panel, cx| {
                match result {
                    Ok((player, mut updates)) => {
                        panel.player = Some(player);
                        panel.player_status = PlayerStatus::Ready;
                        panel._player_updates = cx.spawn(async move |this, cx| {
                            while let Some(update) = updates.next().await {
                                if this
                                    .update(cx, |panel, cx| panel.apply_player_update(update, cx))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                    }
                    Err(error) => {
                        panel.player_status = PlayerStatus::Failed(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    /// The Web API id of this Zed's player, looked up once it has registered.
    async fn zed_device_id(
        this: &WeakEntity<Self>,
        spotify: &SpotifyClient,
        cx: &mut gpui::AsyncApp,
    ) -> Result<String> {
        let (cached, device_name) = this.read_with(cx, |panel, _| {
            (panel.device_id.clone(), panel.device_name.clone())
        })?;
        if let Some(device_id) = cached {
            return Ok(device_id);
        }
        let device_id = spotify
            .device_id(&device_name, cx)
            .await?
            .with_context(|| format!("{device_name} is not connected to Spotify yet"))?;
        this.update(cx, |panel, _| panel.device_id = Some(device_id.clone()))?;
        Ok(device_id)
    }

    fn transport(&mut self, transport: Transport, window: &mut Window, cx: &mut Context<Self>) {
        // Commands for the built-in player skip the round trip through
        // Spotify's servers.
        if let Some(player) = self.local_player() {
            let result = match transport {
                Transport::Pause => player.pause(),
                Transport::Next => player.next(),
                Transport::Previous => player.previous(),
            };
            match result {
                Ok(()) => {
                    if transport == Transport::Pause {
                        self.set_playing(false, cx);
                    }
                }
                Err(error) => {
                    Task::ready(Err::<(), _>(error)).detach_and_prompt_err(
                        "Spotify did not respond",
                        window,
                        cx,
                        |error, _, _| Some(format!("{error:#}")),
                    );
                }
            }
            return;
        }

        let Some(spotify) = self.spotify.clone() else {
            return;
        };
        if transport == Transport::Pause {
            self.set_playing(false, cx);
        }
        cx.spawn(async move |this, cx| {
            spotify.transport(transport, cx).await?;
            this.update(cx, |panel, cx| panel.refresh_soon(cx))?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Spotify did not respond", window, cx, |error, _, _| {
            Some(format!("{error:#}"))
        });
    }

    /// Plays in Zed. With neither a context nor a track this resumes what
    /// the account was last playing, moving it to Zed if it was elsewhere.
    fn play(
        &mut self,
        context_uri: Option<SharedString>,
        track_uri: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if context_uri.is_none()
            && track_uri.is_none()
            && let Some(player) = self.local_player()
        {
            match player.play() {
                Ok(()) => self.set_playing(true, cx),
                Err(error) => {
                    Task::ready(Err::<(), _>(error)).detach_and_prompt_err(
                        "Spotify could not play that",
                        window,
                        cx,
                        |error, _, _| Some(format!("{error:#}")),
                    );
                }
            }
            return;
        }

        let Some(spotify) = self.spotify.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let device_id = Self::zed_device_id(&this, &spotify, cx).await?;
            spotify
                .play_on_device(&device_id, context_uri.as_deref(), track_uri.as_deref(), cx)
                .await?;
            this.update(cx, |panel, cx| {
                panel.refresh_soon(cx);
                if panel.library_view == LibraryView::Queue {
                    panel.show_queue(cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err(
            "Spotify could not play that",
            window,
            cx,
            |error, _, _| Some(format!("{error:#}")),
        );
    }

    fn sign_in(&mut self, cx: &mut Context<Self>) {
        self.account = Account::SigningIn;
        self.library_error = None;
        let http = self.http.clone();
        let credentials = self.credentials.clone();
        self.library_task = cx.spawn(async move |this, cx| {
            let result = async {
                let spotify = SpotifyClient::sign_in(http.clone(), credentials, cx).await?;
                let player_credentials = spotify_player::streaming_login(&http, cx)
                    .await
                    .context("logging in the built-in player")?;
                anyhow::Ok((spotify, player_credentials))
            }
            .await;
            this.update(cx, |panel, cx| {
                match result {
                    Ok((spotify, player_credentials)) => {
                        panel.spotify = Some(spotify);
                        panel.account = Account::SignedIn;
                        panel.load_playlists(cx);
                        panel.start_player(player_credentials, cx);
                    }
                    Err(error) => {
                        panel.account = Account::SignedOut;
                        panel.library_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spotify) = self.spotify.take() else {
            return;
        };
        self.player = None;
        self.player_status = PlayerStatus::Stopped;
        self.device_id = None;
        self.playback = None;
        self.account = Account::SignedOut;
        self.playlists.clear();
        self.tracks.clear();
        self.queue.clear();
        self.library_view = LibraryView::Playlists;
        cx.notify();
        let credentials = self.credentials.clone();
        cx.spawn(async move |_, cx| {
            spotify.sign_out(cx).await?;
            spotify_player::delete_credentials(&credentials, cx).await
        })
        .detach_and_prompt_err(
            "Failed to sign out of Spotify",
            window,
            cx,
            |error, _, _| Some(format!("{error:#}")),
        );
    }

    fn load_library<T: 'static>(
        &mut self,
        cx: &mut Context<Self>,
        fetch: impl AsyncFnOnce(SpotifyClient, &mut gpui::AsyncApp) -> Result<T> + 'static,
        store: impl FnOnce(&mut Self, T) + 'static,
    ) {
        let Some(spotify) = self.spotify.clone() else {
            return;
        };
        self.library_error = None;
        self.library_task = cx.spawn(async move |this, cx| {
            let result = fetch(spotify, cx).await;
            this.update(cx, |panel, cx| {
                match result {
                    Ok(value) => store(panel, value),
                    Err(error) => panel.library_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn load_playlists(&mut self, cx: &mut Context<Self>) {
        self.load_library(
            cx,
            async |spotify, cx| spotify.playlists(cx).await,
            |panel, playlists| panel.playlists = playlists,
        );
    }

    fn show_playlists(&mut self, cx: &mut Context<Self>) {
        self.library_view = LibraryView::Playlists;
        if self.playlists.is_empty() {
            self.load_playlists(cx);
        }
        cx.notify();
    }

    fn open_playlist(&mut self, playlist: Playlist, cx: &mut Context<Self>) {
        let playlist_id = playlist.id.clone();
        self.library_view = LibraryView::Playlist(playlist);
        self.tracks.clear();
        self.load_library(
            cx,
            async move |spotify, cx| spotify.playlist_tracks(&playlist_id, cx).await,
            |panel, tracks| panel.tracks = tracks,
        );
        cx.notify();
    }

    fn show_queue(&mut self, cx: &mut Context<Self>) {
        self.library_view = LibraryView::Queue;
        self.load_library(
            cx,
            async |spotify, cx| spotify.queue(cx).await,
            |panel, queue| panel.queue = queue,
        );
        cx.notify();
    }

    fn reload_library(&mut self, cx: &mut Context<Self>) {
        match self.library_view.clone() {
            LibraryView::Playlists => self.load_playlists(cx),
            LibraryView::Playlist(playlist) => self.open_playlist(playlist, cx),
            LibraryView::Queue => self.show_queue(cx),
        }
    }

    fn render_controls(&self, playing: bool, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .justify_center()
            .gap_4()
            .child(
                IconButton::new("spotify-previous", IconName::ChevronLeft)
                    .icon_size(IconSize::Medium)
                    .tooltip(Tooltip::text("Previous"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.transport(Transport::Previous, window, cx)
                    })),
            )
            .child(
                IconButton::new(
                    "spotify-play-pause",
                    if playing {
                        IconName::DebugPause
                    } else {
                        IconName::PlayFilled
                    },
                )
                .icon_size(IconSize::Medium)
                .style(ButtonStyle::Filled)
                .tooltip(Tooltip::text(if playing { "Pause" } else { "Play in Zed" }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if playing {
                        this.transport(Transport::Pause, window, cx)
                    } else {
                        this.play(None, None, window, cx)
                    }
                })),
            )
            .child(
                IconButton::new("spotify-next", IconName::ChevronRight)
                    .icon_size(IconSize::Medium)
                    .tooltip(Tooltip::text("Next"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.transport(Transport::Next, window, cx)
                    })),
            )
    }

    fn render_player(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = match &self.player_status {
            PlayerStatus::Stopped => None,
            PlayerStatus::Starting => Some(
                Label::new(format!("Starting {}…", self.device_name))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            PlayerStatus::Ready => None,
            PlayerStatus::Failed(error) => Some(
                Label::new(format!("The built-in player could not start: {error}"))
                    .size(LabelSize::Small)
                    .color(Color::Error),
            ),
        };

        let now_playing: AnyElement = match self.playback.clone() {
            None => empty_message("Nothing is playing. Pick a song below.").into_any_element(),
            Some(playback) => {
                let elsewhere = playback.device_name != self.device_name;
                h_flex()
                    .gap_3()
                    .when_some(playback.art_url.clone(), |row, art_url| {
                        row.child(
                            img(ImageSource::Resource(Resource::Uri(SharedUri::from(
                                art_url.to_string(),
                            ))))
                            .size(px(72.))
                            .flex_none()
                            .rounded_md(),
                        )
                    })
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0p5()
                            .child(Label::new(playback.title).truncate())
                            .child(Label::new(playback.artists).color(Color::Muted).truncate())
                            .child(
                                Label::new(if elsewhere {
                                    format!("Playing on {}", playback.device_name)
                                } else {
                                    playback.album.to_string()
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                            ),
                    )
                    .into_any_element()
            }
        };
        let playing = self
            .playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing);

        v_flex()
            .gap_2()
            .children(status)
            .child(now_playing)
            .child(self.render_controls(playing, cx))
            .into_any_element()
    }

    fn render_library(&self, cx: &mut Context<Self>) -> AnyElement {
        let error = self.library_error.clone().map(|error| {
            Label::new(error)
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element()
        });

        match self.account {
            Account::Loading => empty_message("Checking your Spotify login…").into_any_element(),
            Account::SigningIn => {
                empty_message("Finish logging in to Spotify in your browser…").into_any_element()
            }
            Account::SignedOut => v_flex()
                .w_full()
                .p_3()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border_focused)
                .bg(cx.theme().colors().element_background)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(IconName::AudioOn).color(Color::Accent))
                        .child(Label::new("Connect your Spotify account").size(LabelSize::Large)),
                )
                .child(
                    Label::new(
                        "See all your playlists, Liked Songs and queue, and pick what plays.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .children(error)
                .child(
                    Button::new("spotify-connect", "Connect Spotify")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .size(ButtonSize::Large)
                        .full_width()
                        .start_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::Small))
                        .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx))),
                )
                .into_any_element(),
            Account::SignedIn => {
                let showing_queue = self.library_view == LibraryView::Queue;
                let rows: Vec<AnyElement> = match &self.library_view {
                    LibraryView::Playlists => self
                        .playlists
                        .iter()
                        .enumerate()
                        .map(|(index, playlist)| {
                            let playlist = playlist.clone();
                            library_row(
                                ("spotify-playlist", index),
                                playlist.name.clone(),
                                format!("{} songs", playlist.track_count).into(),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_playlist(playlist.clone(), cx)
                            }))
                            .into_any_element()
                        })
                        .collect(),
                    LibraryView::Playlist(playlist) => self
                        .tracks
                        .iter()
                        .enumerate()
                        .map(|(index, track)| {
                            let context_uri = playlist.uri.clone();
                            let track_uri = track.uri.clone();
                            library_row(
                                ("spotify-track", index),
                                track.name.clone(),
                                track.artists.clone(),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.play(
                                    Some(context_uri.clone()),
                                    Some(track_uri.clone()),
                                    window,
                                    cx,
                                )
                            }))
                            .into_any_element()
                        })
                        .collect(),
                    LibraryView::Queue => self
                        .queue
                        .iter()
                        .enumerate()
                        .map(|(index, track)| {
                            let track_uri = track.uri.clone();
                            library_row(
                                ("spotify-queue", index),
                                track.name.clone(),
                                track.artists.clone(),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.play(None, Some(track_uri.clone()), window, cx)
                            }))
                            .into_any_element()
                        })
                        .collect(),
                };

                let playlist_header = match &self.library_view {
                    LibraryView::Playlist(playlist) => {
                        let context_uri = playlist.uri.clone();
                        Some(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    IconButton::new("spotify-back", IconName::ChevronLeft)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Back to playlists"))
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.show_playlists(cx)),
                                        ),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Label::new(playlist.name.clone())
                                            .size(LabelSize::Small)
                                            .truncate(),
                                    ),
                                )
                                .child(
                                    Button::new("spotify-play-playlist", "Play")
                                        .style(ButtonStyle::Filled)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.play(Some(context_uri.clone()), None, window, cx)
                                        })),
                                ),
                        )
                    }
                    _ => None,
                };

                let is_empty = rows.is_empty();
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                Button::new("spotify-playlists-tab", "Playlists")
                                    .toggle_state(!showing_queue)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.show_playlists(cx)),
                                    ),
                            )
                            .child(
                                Button::new("spotify-queue-tab", "Queue")
                                    .toggle_state(showing_queue)
                                    .on_click(cx.listener(|this, _, _, cx| this.show_queue(cx))),
                            )
                            .child(div().flex_1())
                            .child(
                                IconButton::new("spotify-reload", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Reload"))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.reload_library(cx)),
                                    ),
                            )
                            .child(
                                IconButton::new("spotify-sign-out", IconName::Exit)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Disconnect Spotify account"))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.sign_out(window, cx)
                                        }),
                                    ),
                            ),
                    )
                    .children(playlist_header)
                    .children(error)
                    .child(
                        v_flex()
                            .id("spotify-library")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .when(is_empty, |list| {
                                list.child(empty_message(if showing_queue {
                                    "The queue is empty."
                                } else {
                                    "Loading…"
                                }))
                            })
                            .children(rows),
                    )
                    .into_any_element()
            }
        }
    }
}

fn library_row(id: impl Into<ElementId>, title: SharedString, detail: SharedString) -> ListItem {
    ListItem::new(id).spacing(ListItemSpacing::Sparse).child(
        v_flex()
            .min_w_0()
            .child(Label::new(title).truncate())
            .child(
                Label::new(detail)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            ),
    )
}

impl Render for SpotifyPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("SpotifyPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_3()
            .gap_3()
            .when(self.account == Account::SignedIn, |panel| {
                panel.child(self.render_player(cx))
            })
            .child(self.render_library(cx))
    }
}

impl_dock_panel!(
    SpotifyPanel,
    name: "Spotify Panel",
    key: "SpotifyPanel",
    tooltip: "Spotify",
    icon: IconName::AudioOn,
    toggle: ToggleSpotifyFocus,
    priority: 101,
    group: crate::MEDIA_BUTTON_GROUP
);
