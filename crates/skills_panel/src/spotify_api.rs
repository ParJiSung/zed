use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use credentials_provider::CredentialsProvider;
use gpui::{AsyncApp, SharedString};
use http_client::{AsyncBody, HttpClient, Method, Request, StatusCode};
use parking_lot::Mutex;
use rand::RngCore as _;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use url::form_urlencoded;

/// Spotify client IDs are public by design; the PKCE flow needs no secret.
const CLIENT_ID: &str = "28e128b2582e403b8f1c7dfbb3774b91";
const CREDENTIALS_KEY: &str = "https://accounts.spotify.com/zed-spotify-panel";
const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const API_URL: &str = "https://api.spotify.com/v1";
const SCOPES: &str = "playlist-read-private playlist-read-collaborative user-library-read \
    user-read-playback-state user-read-currently-playing user-modify-playback-state";
/// Stored alongside the token so that a login made with fewer scopes is
/// treated as signed out, prompting the user to connect again.
const CREDENTIALS_USERNAME: &str = "scopes-v2";
const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 8898;
const CALLBACK_PATH: &str = "/callback";
/// Guards against a malformed `next` link looping forever.
pub(crate) const MAX_PAGES: usize = 500;
const LIKED_SONGS_ID: &str = "liked-songs";

#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    pub id: SharedString,
    pub name: SharedString,
    pub uri: SharedString,
    pub track_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Playback {
    pub title: SharedString,
    pub artists: SharedString,
    pub album: SharedString,
    pub art_url: Option<SharedString>,
    pub is_playing: bool,
    pub device_name: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transport {
    Pause,
    Next,
    Previous,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub name: SharedString,
    pub artists: SharedString,
    pub uri: SharedString,
}

struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct SpotifyClient {
    http: Arc<dyn HttpClient>,
    credentials: Arc<dyn CredentialsProvider>,
    tokens: Arc<Mutex<Tokens>>,
}

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

impl SpotifyClient {
    /// Restores a previous sign-in from the keychain, if there is one.
    pub async fn load(
        http: Arc<dyn HttpClient>,
        credentials: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> Result<Option<Self>> {
        let Some((username, refresh_token)) =
            credentials.read_credentials(CREDENTIALS_KEY, cx).await?
        else {
            return Ok(None);
        };
        if username != CREDENTIALS_USERNAME {
            return Ok(None);
        }
        let refresh_token =
            String::from_utf8(refresh_token).context("stored Spotify token is not UTF-8")?;
        Ok(Some(Self {
            http,
            credentials,
            tokens: Arc::new(Mutex::new(Tokens {
                access_token: String::new(),
                refresh_token,
                expires_at: Instant::now(),
            })),
        }))
    }

    pub async fn sign_in(
        http: Arc<dyn HttpClient>,
        credentials: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> Result<Self> {
        let response = authorize(&http, CLIENT_ID, SCOPES, CALLBACK_PATH, cx).await?;
        let refresh_token = response
            .refresh_token
            .context("Spotify did not return a refresh token")?;
        credentials
            .write_credentials(
                CREDENTIALS_KEY,
                CREDENTIALS_USERNAME,
                refresh_token.as_bytes(),
                cx,
            )
            .await
            .context("saving the Spotify login to the keychain")?;

        Ok(Self {
            http,
            credentials,
            tokens: Arc::new(Mutex::new(Tokens {
                access_token: response.access_token,
                refresh_token,
                expires_at: expires_at(response.expires_in),
            })),
        })
    }

    pub async fn sign_out(&self, cx: &AsyncApp) -> Result<()> {
        self.credentials
            .delete_credentials(CREDENTIALS_KEY, cx)
            .await
    }

    async fn access_token(&self, cx: &AsyncApp) -> Result<String> {
        let refresh_token = {
            let tokens = self.tokens.lock();
            if Instant::now() < tokens.expires_at {
                return Ok(tokens.access_token.clone());
            }
            tokens.refresh_token.clone()
        };

        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("refresh_token", &refresh_token)
            .finish();
        let response = request_token(&self.http, body).await?;

        // Spotify may rotate the refresh token; the old one stops working.
        if let Some(rotated) = &response.refresh_token {
            self.credentials
                .write_credentials(
                    CREDENTIALS_KEY,
                    CREDENTIALS_USERNAME,
                    rotated.as_bytes(),
                    cx,
                )
                .await
                .context("saving the refreshed Spotify login to the keychain")?;
        }
        let mut tokens = self.tokens.lock();
        tokens.access_token = response.access_token.clone();
        tokens.expires_at = expires_at(response.expires_in);
        if let Some(rotated) = response.refresh_token {
            tokens.refresh_token = rotated;
        }
        Ok(response.access_token)
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
        cx: &AsyncApp,
    ) -> Result<(StatusCode, String)> {
        let access_token = self.access_token(cx).await?;
        let mut builder = Request::builder()
            .method(method.clone())
            .uri(url)
            .header("Authorization", format!("Bearer {access_token}"));
        let body = match body {
            Some(json) => {
                builder = builder.header("Content-Type", "application/json");
                AsyncBody::from(json.to_string())
            }
            None => {
                // Spotify rejects body-less PUT and POST requests that do not
                // state their length, and the HTTP client omits it for an
                // empty body.
                if method != Method::GET {
                    builder = builder.header("Content-Length", "0");
                }
                AsyncBody::empty()
            }
        };
        let mut response = self.http.send(builder.body(body)?).await?;
        let mut text = String::new();
        smol::io::AsyncReadExt::read_to_string(response.body_mut(), &mut text).await?;
        Ok((response.status(), text))
    }

    async fn get_json(&self, url: &str, cx: &AsyncApp) -> Result<Value> {
        let (status, text) = self.send(Method::GET, url, None, cx).await?;
        if !status.is_success() {
            return Err(api_error(status, &text));
        }
        serde_json::from_str(&text).context("parsing the Spotify response")
    }

    /// Follows Spotify's `next` links and collects every item on every page.
    async fn all_pages<T>(
        &self,
        first_url: String,
        cx: &AsyncApp,
        mut parse: impl FnMut(&Value) -> Option<T>,
    ) -> Result<Vec<T>> {
        let mut items = Vec::new();
        let mut next = Some(first_url);
        for _ in 0..MAX_PAGES {
            let Some(url) = next.take() else {
                break;
            };
            let page = self.get_json(&url, cx).await?;
            items.extend(
                page["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(&mut parse),
            );
            next = page["next"].as_str().map(str::to_string);
        }
        Ok(items)
    }

    /// Lists every playlist in the library, led by the user's Liked Songs.
    pub async fn playlists(&self, cx: &AsyncApp) -> Result<Vec<Playlist>> {
        let user = self.get_json(&format!("{API_URL}/me"), cx).await?;
        let user_id = user["id"]
            .as_str()
            .context("Spotify did not return a user id")?;
        let liked_songs = self
            .get_json(&format!("{API_URL}/me/tracks?limit=1"), cx)
            .await?;

        let mut playlists = vec![Playlist {
            id: LIKED_SONGS_ID.into(),
            name: "Liked Songs".into(),
            uri: format!("spotify:user:{user_id}:collection").into(),
            track_count: liked_songs["total"].as_u64().unwrap_or(0),
        }];
        playlists.extend(
            self.all_pages(
                format!("{API_URL}/me/playlists?limit=50"),
                cx,
                playlist_from_json,
            )
            .await?,
        );
        Ok(playlists)
    }

    /// Fetches one page of a playlist's songs, returning them with the URL of
    /// the next page. Callers show each page as it arrives instead of waiting
    /// for a long playlist to load completely.
    pub async fn playlist_tracks_page(
        &self,
        playlist_id: &str,
        next_page: Option<&str>,
        cx: &AsyncApp,
    ) -> Result<(Vec<Track>, Option<String>)> {
        let page = match next_page {
            Some(url) => self.get_json(url, cx).await?,
            None if playlist_id == LIKED_SONGS_ID => {
                self.get_json(&format!("{API_URL}/me/tracks?limit=50"), cx)
                    .await?
            }
            // Spotify renamed this endpoint from `/tracks` to `/items`; try both.
            None => match self
                .get_json(
                    &format!("{API_URL}/playlists/{playlist_id}/items?limit=100"),
                    cx,
                )
                .await
            {
                Ok(page) => page,
                Err(_) => {
                    self.get_json(
                        &format!("{API_URL}/playlists/{playlist_id}/tracks?limit=100"),
                        cx,
                    )
                    .await?
                }
            },
        };
        let tracks = page["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                if entry["item"].is_object() {
                    track_from_json(&entry["item"])
                } else {
                    track_from_json(&entry["track"])
                }
            })
            .collect();
        Ok((tracks, page["next"].as_str().map(str::to_string)))
    }

    pub async fn queue(&self, cx: &AsyncApp) -> Result<Vec<Track>> {
        let response = self
            .get_json(&format!("{API_URL}/me/player/queue"), cx)
            .await?;
        Ok(response["queue"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(track_from_json)
            .collect())
    }

    async fn expect_success(
        &self,
        method: Method,
        url: &str,
        body: Option<Value>,
        cx: &AsyncApp,
    ) -> Result<()> {
        let (status, text) = self.send(method, url, body, cx).await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(api_error(status, &text))
        }
    }

    /// What is playing on the account right now, on any device.
    pub async fn playback(&self, cx: &AsyncApp) -> Result<Option<Playback>> {
        let (status, text) = self
            .send(Method::GET, &format!("{API_URL}/me/player"), None, cx)
            .await?;
        if status == StatusCode::NO_CONTENT || text.trim().is_empty() {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(api_error(status, &text));
        }
        let json: Value =
            serde_json::from_str(&text).context("parsing the Spotify player state")?;
        Ok(playback_from_json(&json))
    }

    pub async fn transport(&self, transport: Transport, cx: &AsyncApp) -> Result<()> {
        let (method, path) = match transport {
            Transport::Pause => (Method::PUT, "pause"),
            Transport::Next => (Method::POST, "next"),
            Transport::Previous => (Method::POST, "previous"),
        };
        self.expect_success(method, &format!("{API_URL}/me/player/{path}"), None, cx)
            .await
    }

    /// Finds the Web API id of the Spotify Connect device with this name.
    pub async fn device_id(&self, device_name: &str, cx: &AsyncApp) -> Result<Option<String>> {
        let devices = self
            .get_json(&format!("{API_URL}/me/player/devices"), cx)
            .await?;
        Ok(devices["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|device| device["name"].as_str() == Some(device_name))
            .and_then(|device| device["id"].as_str())
            .map(str::to_string))
    }

    /// Plays on the given device. With neither a context nor a track, this
    /// resumes whatever the account was last playing.
    pub async fn play_on_device(
        &self,
        device_id: &str,
        context_uri: Option<&str>,
        track_uri: Option<&str>,
        cx: &AsyncApp,
    ) -> Result<()> {
        let body = match (context_uri, track_uri) {
            (Some(context_uri), Some(track_uri)) => Some(serde_json::json!({
                "context_uri": context_uri,
                "offset": { "uri": track_uri },
            })),
            (Some(context_uri), None) => Some(serde_json::json!({ "context_uri": context_uri })),
            (None, Some(track_uri)) => Some(serde_json::json!({ "uris": [track_uri] })),
            (None, None) => None,
        };
        let device_id: String = form_urlencoded::byte_serialize(device_id.as_bytes()).collect();
        self.expect_success(
            Method::PUT,
            &format!("{API_URL}/me/player/play?device_id={device_id}"),
            body,
            cx,
        )
        .await
    }
}

/// Runs Spotify's browser login (authorization code with PKCE) for the given
/// client and returns the token response.
pub(crate) async fn authorize(
    http: &Arc<dyn HttpClient>,
    client_id: &str,
    scopes: &str,
    callback_path: &'static str,
    cx: &AsyncApp,
) -> Result<TokenResponse> {
    let (redirect_uri, callback) = oauth_callback_server::start_oauth_callback_server_with_config(
        oauth_callback_server::OAuthCallbackServerConfig {
            host: CALLBACK_HOST,
            preferred_port: CALLBACK_PORT,
            fallback_port: None,
            path: callback_path,
        },
    )
    .context("starting the Spotify login callback server")?;

    let mut verifier_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let mut state_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut state_bytes);
    let state = URL_SAFE_NO_PAD.encode(state_bytes);

    let mut authorize_url = url::Url::parse(AUTHORIZE_URL)?;
    authorize_url
        .query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", scopes)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("state", &state);
    cx.update(|cx| cx.open_url(authorize_url.as_str()));

    let callback = callback
        .await
        .map_err(|_| anyhow!("the Spotify login was cancelled"))?
        .context("the Spotify login failed")?;
    if callback.state != state {
        return Err(anyhow!("the Spotify login returned a mismatched state"));
    }

    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", client_id)
        .append_pair("code", &callback.code)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_verifier", &verifier)
        .finish();
    request_token(http, body).await
}

async fn request_token(http: &Arc<dyn HttpClient>, body: String) -> Result<TokenResponse> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(AsyncBody::from(body))?;
    let mut response = http.send(request).await?;
    let mut text = String::new();
    smol::io::AsyncReadExt::read_to_string(response.body_mut(), &mut text).await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "Spotify login failed (HTTP {}): {}",
            response.status(),
            truncate(&text)
        ));
    }
    serde_json::from_str(&text).context("parsing the Spotify token response")
}

fn playback_from_json(json: &Value) -> Option<Playback> {
    let item = &json["item"];
    let title = item["name"].as_str()?;
    let artists = item["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|artist| artist["name"].as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(Playback {
        title: title.to_string().into(),
        artists: artists.into(),
        album: item["album"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string()
            .into(),
        art_url: item["album"]["images"][0]["url"]
            .as_str()
            .map(|url| url.to_string().into()),
        is_playing: json["is_playing"].as_bool().unwrap_or(false),
        device_name: json["device"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string()
            .into(),
    })
}

fn playlist_from_json(item: &Value) -> Option<Playlist> {
    // Newer API responses name the track count `items`, older ones `tracks`.
    let track_count = item["items"]["total"]
        .as_u64()
        .or_else(|| item["tracks"]["total"].as_u64())
        .unwrap_or(0);
    Some(Playlist {
        id: item["id"].as_str()?.to_string().into(),
        name: item["name"].as_str()?.to_string().into(),
        uri: item["uri"].as_str()?.to_string().into(),
        track_count,
    })
}

fn track_from_json(track: &Value) -> Option<Track> {
    let name = track["name"].as_str()?;
    let uri = track["uri"].as_str()?;
    let artists = track["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|artist| artist["name"].as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(Track {
        name: name.to_string().into(),
        artists: artists.into(),
        uri: uri.to_string().into(),
    })
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn expires_at(expires_in_seconds: u64) -> Instant {
    // Refresh a little early so a request never races the expiry.
    Instant::now() + Duration::from_secs(expires_in_seconds.saturating_sub(60))
}

fn api_error(status: StatusCode, body: &str) -> anyhow::Error {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|json| json["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| truncate(body));
    anyhow!("Spotify returned HTTP {status}: {message}")
}

fn truncate(text: &str) -> String {
    const MAX_LENGTH: usize = 240;
    match text.char_indices().nth(MAX_LENGTH) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn track_from_json_joins_artists() {
        let track = serde_json::json!({
            "name": "Song",
            "uri": "spotify:track:1",
            "artists": [{ "name": "A" }, { "name": "B" }],
        });
        assert_eq!(
            track_from_json(&track),
            Some(Track {
                name: "Song".into(),
                artists: "A, B".into(),
                uri: "spotify:track:1".into(),
            })
        );
        assert_eq!(
            track_from_json(&serde_json::json!({ "name": "No uri" })),
            None
        );
    }

    #[test]
    fn playlist_from_json_reads_either_track_count_field() {
        let newer = serde_json::json!({
            "id": "1", "name": "New", "uri": "spotify:playlist:1", "items": { "total": 7 },
        });
        let older = serde_json::json!({
            "id": "2", "name": "Old", "uri": "spotify:playlist:2", "tracks": { "total": 9 },
        });
        assert_eq!(
            playlist_from_json(&newer).map(|playlist| playlist.track_count),
            Some(7)
        );
        assert_eq!(
            playlist_from_json(&older).map(|playlist| playlist.track_count),
            Some(9)
        );
        assert_eq!(
            playlist_from_json(&serde_json::json!({ "name": "No id" })),
            None
        );
    }

    #[test]
    fn playback_from_json_reads_track_and_device() {
        let json = serde_json::json!({
            "is_playing": true,
            "device": { "name": "Zed (laptop)" },
            "item": {
                "name": "Song",
                "artists": [{ "name": "A" }],
                "album": { "name": "Album", "images": [{ "url": "https://i.scdn.co/image/1" }] },
            },
        });
        assert_eq!(
            playback_from_json(&json),
            Some(Playback {
                title: "Song".into(),
                artists: "A".into(),
                album: "Album".into(),
                art_url: Some("https://i.scdn.co/image/1".into()),
                is_playing: true,
                device_name: "Zed (laptop)".into(),
            })
        );
        assert_eq!(
            playback_from_json(&serde_json::json!({ "is_playing": false })),
            None
        );
    }

    #[test]
    fn truncate_respects_character_boundaries() {
        let long = "é".repeat(300);
        let truncated = truncate(&long);
        assert_eq!(truncated.chars().count(), 241);
        assert!(truncated.ends_with('…'));
        assert_eq!(truncate("short"), "short");
    }
}
