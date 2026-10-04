//! A Subsonic-API client, aimed at Navidrome.
//!
//! # What this is and isn't responsible for
//!
//! Subsonic is a *library* API: it browses, searches, serves cover art, and
//! hands out a streamable URL per track. It has no notion of telling another
//! device to play something -- the client does the playing. Since this app is a
//! remote control and the Pi deliberately has no audio stack at all (no ALSA,
//! no decoder, no `/dev/snd`, and no headphone jack on a Zero 2 W), playback is
//! Home Assistant's job: hand [`Credentials::stream_url`] to
//! `media_player.play_media` and let a real player deal with it.
//!
//! So this module answers "what do I want to hear"; `main.rs` wires the answer
//! to "where does it come out".
//!
//! # Why the password is never stored
//!
//! Subsonic authenticates with `t = md5(password + salt)` for a caller-chosen
//! salt, and a fixed salt/token pair keeps working indefinitely. So setup
//! generates one random salt, derives the token, and persists only those --
//! functionally the same as storing the password for this API, but without
//! writing the user's actual password to disk, which matters when it's reused
//! elsewhere.
//!
//! The stream URL necessarily embeds `u`/`t`/`s`, so that token does travel to
//! HA and on to the player, in the clear over plain HTTP on the LAN. That's
//! inherent to Subsonic streaming rather than something this design adds, and
//! it is a LAN-scoped credential to a music library -- proportionate, in the
//! same spirit as the parental PIN's plain SHA-256.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CREDENTIALS_PATH: &str = "/etc/skylight/music.secret";

/// What this client reports itself as. Navidrome shows it in its activity log.
const CLIENT_NAME: &str = "skylight";
/// The API level we code against. 1.16.1 is what Navidrome implements and is
/// what `search3`/`getArtists`/`getAlbumList2` require.
const API_VERSION: &str = "1.16.1";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not reach the music server: {0}")]
    Unreachable(#[source] reqwest::Error),
    #[error("the music server rejected the request: {message} (code {code})")]
    Server { code: i64, message: String },
    #[error("unexpected reply from the music server: {0}")]
    Malformed(String),
    #[error("could not read or write the saved music settings: {0}")]
    Storage(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Everything needed to talk to the server, and where to play.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// Base URL with no trailing slash, e.g. `http://192.168.0.5:4533`.
    pub server_url: String,
    pub username: String,
    /// Random per-install salt; see the module header for why this and `token`
    /// are stored instead of the password.
    pub salt: String,
    /// `md5(password + salt)`, hex.
    pub token: String,
    /// The HA `media_player` entity playback is sent to. `None` until the user
    /// picks one -- browsing works without it.
    #[serde(default)]
    pub player_entity_id: Option<String>,
    /// Its friendly name, stored alongside the id so the transport bar can
    /// label itself at startup without waiting for Home Assistant to connect
    /// and without re-fetching the entity list. `serde(default)` so files
    /// written before this field existed still load.
    #[serde(default)]
    pub player_name: Option<String>,
}

impl Credentials {
    /// Derives a fresh salt/token pair from a password typed at setup.
    pub fn new(server_url: &str, username: &str, password: &str) -> Self {
        let salt = random_salt();
        let token = md5_hex(&format!("{password}{salt}"));
        Self {
            server_url: server_url.trim().trim_end_matches('/').to_string(),
            username: username.trim().to_string(),
            salt,
            token,
            player_entity_id: None,
            player_name: None,
        }
    }

    /// The query string every request needs.
    fn auth_query(&self) -> String {
        format!(
            "u={}&t={}&s={}&v={}&c={}&f=json",
            urlencode(&self.username),
            self.token,
            self.salt,
            API_VERSION,
            CLIENT_NAME,
        )
    }

    pub fn endpoint(&self, method: &str, params: &[(&str, &str)]) -> String {
        let mut url = format!("{}/rest/{}?{}", self.server_url, method, self.auth_query());
        for (key, value) in params {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&urlencode(value));
        }
        url
    }

    /// Album art, already scaled by the server.
    ///
    /// Asking Subsonic for the size we want means the Pi downloads and decodes
    /// a thumbnail rather than a full-resolution cover -- on a Zero 2 W that
    /// difference is the whole feature being usable or not.
    pub fn cover_art_url(&self, cover_art_id: &str, size: u32) -> String {
        self.endpoint("getCoverArt", &[("id", cover_art_id), ("size", &size.to_string())])
    }

    /// The URL handed to `media_player.play_media`.
    ///
    /// Deliberately a plain authenticated GET: every player that can fetch a
    /// URL can play this, which is the whole point of pushing playback out to
    /// Home Assistant.
    pub fn stream_url(&self, track_id: &str) -> String {
        self.endpoint("stream", &[("id", track_id)])
    }
}

fn md5_hex(input: &str) -> String {
    let digest = Md5::digest(input.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A salt only has to be unguessable-per-install, not cryptographically
/// precious -- it exists so the stored token isn't a bare password hash. Built
/// from the system clock plus the process id rather than pulling in an RNG
/// crate for one 16-character string.
fn random_salt() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    md5_hex(&format!("{nanos}-{}", std::process::id()))[..16].to_string()
}

/// Percent-encodes the characters that would otherwise break a query string.
///
/// Deliberately minimal rather than a dependency: usernames and search terms
/// are the only user-controlled values that reach here.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Where the credentials file lives. Overridable so the whole feature can be
/// exercised against a scratch path on the dev machine.
pub fn credentials_path() -> PathBuf {
    std::env::var("SKYLIGHT_MUSIC_CREDENTIALS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CREDENTIALS_PATH))
}

pub fn load_credentials() -> Option<Credentials> {
    load_credentials_at(&credentials_path())
}

/// Reads credentials from an explicit path.
///
/// Separate from [`load_credentials`] so tests can point at a scratch file by
/// argument. They must *not* do it by setting the env var: `set_var` is
/// unsound once the process is multithreaded, which a parallel test runner
/// very much is -- the same hazard that made the timezone feature restart
/// rather than call `setenv` (see `timezone.rs`).
pub fn load_credentials_at(path: &std::path::Path) -> Option<Credentials> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(credentials) => Some(credentials),
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "music settings file is unreadable; ignoring it");
            None
        }
    }
}

/// Writes credentials, replacing any previous ones.
///
/// Temp-then-rename so a crash mid-write can't leave a half-file that would
/// then be silently ignored on next boot.
pub fn save_credentials(credentials: &Credentials) -> io::Result<()> {
    save_credentials_at(&credentials_path(), credentials)
}

pub fn save_credentials_at(path: &std::path::Path, credentials: &Credentials) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension("new");
    let body = serde_json::to_string_pretty(credentials)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    std::fs::write(&staging, body)?;
    std::fs::rename(&staging, path)
}

pub fn forget_credentials() -> io::Result<()> {
    forget_credentials_at(&credentials_path())
}

pub fn forget_credentials_at(path: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

// --- Library model -------------------------------------------------------
//
// Only the fields the UI actually shows. Subsonic returns a great deal more,
// and `serde` ignores the rest.

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "albumCount")]
    pub album_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default, rename = "songCount")]
    pub song_count: u32,
    #[serde(default, rename = "coverArt")]
    pub cover_art: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    /// Seconds. Absent on some servers for some formats.
    #[serde(default)]
    pub duration: Option<u32>,
    #[serde(default)]
    pub track: Option<u32>,
    #[serde(default, rename = "coverArt")]
    pub cover_art: Option<String>,
}

impl Track {
    /// `3:07`, or empty when the server didn't say.
    pub fn duration_label(&self) -> String {
        match self.duration {
            Some(seconds) => format!("{}:{:02}", seconds / 60, seconds % 60),
            None => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "songCount")]
    pub song_count: u32,
}

/// How the album list is ordered -- the orders `getAlbumList2` defines that
/// are worth offering on a wall display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumSort {
    /// Recently added. The default: it's what "anything new?" means.
    Newest,
    Alphabetical,
    Frequent,
    Recent,
    Random,
}

impl AlbumSort {
    pub const ALL: [AlbumSort; 5] = [
        AlbumSort::Newest,
        AlbumSort::Alphabetical,
        AlbumSort::Frequent,
        AlbumSort::Recent,
        AlbumSort::Random,
    ];

    /// Subsonic's `getAlbumList2` type parameter.
    pub fn as_type(self) -> &'static str {
        match self {
            AlbumSort::Newest => "newest",
            AlbumSort::Alphabetical => "alphabeticalByName",
            AlbumSort::Frequent => "frequent",
            AlbumSort::Recent => "recent",
            AlbumSort::Random => "random",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AlbumSort::Newest => "Newest",
            AlbumSort::Alphabetical => "A-Z",
            AlbumSort::Frequent => "Most played",
            AlbumSort::Recent => "Recently played",
            AlbumSort::Random => "Random",
        }
    }

    pub fn from_index(index: usize) -> AlbumSort {
        Self::ALL.get(index).copied().unwrap_or(AlbumSort::Newest)
    }

    pub fn index(self) -> i32 {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0) as i32
    }

    /// `random` returns a fresh shuffle per request, so paging it would just
    /// show overlapping random picks rather than "the next page".
    pub fn is_pageable(self) -> bool {
        !matches!(self, AlbumSort::Random)
    }
}

/// How many albums a page holds.
///
/// Subsonic caps `getAlbumList2` at 500, but smaller pages keep each request
/// quick and the list responsive on a Pi Zero 2 W.
pub const ALBUM_PAGE_SIZE: u32 = 100;

// --- Response envelope ---------------------------------------------------

/// Pulls the `subsonic-response` object out, turning a server-reported failure
/// into an [`Error::Server`] carrying the server's own wording -- which is far
/// more useful at setup time than "login failed" ("Wrong username or
/// password", "Incompatible Subsonic REST protocol version").
pub fn unwrap_response(body: &str) -> Result<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|err| Error::Malformed(format!("not JSON: {err}")))?;
    let response = parsed
        .get("subsonic-response")
        .ok_or_else(|| Error::Malformed("no subsonic-response object".into()))?;

    if response.get("status").and_then(|s| s.as_str()) == Some("failed") {
        let error = response.get("error");
        return Err(Error::Server {
            code: error.and_then(|e| e.get("code")).and_then(|c| c.as_i64()).unwrap_or(0),
            message: error
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string(),
        });
    }
    Ok(response.clone())
}

/// Reads a list out of a response, tolerating the two shapes Subsonic uses.
///
/// Servers omit the inner array entirely when a collection is empty (rather
/// than sending `[]`), so a missing key is an empty list, not an error. Getting
/// this wrong would turn "no results" into a parse failure.
fn list_at<T: for<'de> Deserialize<'de>>(
    response: &serde_json::Value,
    outer: &str,
    inner: &str,
) -> Result<Vec<T>> {
    let Some(container) = response.get(outer) else {
        return Ok(Vec::new());
    };
    let Some(items) = container.get(inner) else {
        return Ok(Vec::new());
    };
    serde_json::from_value(items.clone())
        .map_err(|err| Error::Malformed(format!("could not read {outer}.{inner}: {err}")))
}

pub fn parse_albums(response: &serde_json::Value) -> Result<Vec<Album>> {
    list_at(response, "albumList2", "album")
}

pub fn parse_album_tracks(response: &serde_json::Value) -> Result<Vec<Track>> {
    list_at(response, "album", "song")
}

pub fn parse_playlists(response: &serde_json::Value) -> Result<Vec<Playlist>> {
    list_at(response, "playlists", "playlist")
}

pub fn parse_playlist_tracks(response: &serde_json::Value) -> Result<Vec<Track>> {
    list_at(response, "playlist", "entry")
}

pub fn parse_artist_albums(response: &serde_json::Value) -> Result<Vec<Album>> {
    list_at(response, "artist", "album")
}

/// `getArtists` nests artists under index buckets (`A`, `B`, ...), so this
/// flattens them rather than exposing the alphabet to the UI.
pub fn parse_artists(response: &serde_json::Value) -> Result<Vec<Artist>> {
    let Some(index) = response.get("artists").and_then(|a| a.get("index")) else {
        return Ok(Vec::new());
    };
    let Some(buckets) = index.as_array() else {
        return Ok(Vec::new());
    };
    let mut artists = Vec::new();
    for bucket in buckets {
        if let Some(items) = bucket.get("artist") {
            let parsed: Vec<Artist> = serde_json::from_value(items.clone())
                .map_err(|err| Error::Malformed(format!("could not read artists: {err}")))?;
            artists.extend(parsed);
        }
    }
    Ok(artists)
}

/// `search3` returns whichever of the three kinds matched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
}

pub fn parse_search(response: &serde_json::Value) -> Result<SearchResults> {
    Ok(SearchResults {
        artists: list_at(response, "searchResult3", "artist")?,
        albums: list_at(response, "searchResult3", "album")?,
        tracks: list_at(response, "searchResult3", "song")?,
    })
}

/// Decoded album art, in a form that can cross a thread boundary.
///
/// Raw RGBA rather than a `slint::Image` because that type isn't `Send`: the
/// download and decode happen on a worker, and only the final cheap wrap into
/// an image happens on the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverArt {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decodes fetched art bytes.
///
/// Split from the fetch so the awkward part -- "is this actually an image?" --
/// is testable without a server. Servers have been known to answer with an
/// HTML error page and a 200, which must not be mistaken for a cover.
pub fn decode_cover(bytes: &[u8]) -> Result<CoverArt> {
    let decoded = image::load_from_memory(bytes)
        .map_err(|err| Error::Malformed(format!("cover art is not a readable image: {err}")))?
        .to_rgba8();
    Ok(CoverArt {
        width: decoded.width(),
        height: decoded.height(),
        rgba: decoded.into_raw(),
    })
}

// --- The client ----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    credentials: Credentials,
}

impl Client {
    pub fn new(credentials: Credentials) -> Self {
        let http = reqwest::Client::builder()
            // Bounded for the same reason every other network call in this app
            // is: a wall display must never wedge on a server that stopped
            // answering. See the connection-resilience notes in CLAUDE.md.
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { http, credentials }
    }

    async fn get(&self, method: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        let url = self.credentials.endpoint(method, params);
        let body = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(Error::Unreachable)?
            .text()
            .await
            .map_err(Error::Unreachable)?;
        unwrap_response(&body)
    }

    /// Validates credentials. Run at setup so a typo surfaces immediately,
    /// with the server's own message, instead of on every later request.
    pub async fn ping(&self) -> Result<()> {
        self.get("ping", &[]).await.map(|_| ())
    }

    /// One page of albums. `offset` is in albums, not pages.
    pub async fn albums(&self, sort: AlbumSort, offset: u32) -> Result<Vec<Album>> {
        let response = self
            .get(
                "getAlbumList2",
                &[
                    ("type", sort.as_type()),
                    ("size", &ALBUM_PAGE_SIZE.to_string()),
                    ("offset", &offset.to_string()),
                ],
            )
            .await?;
        parse_albums(&response)
    }

    pub async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let response = self.get("getAlbum", &[("id", album_id)]).await?;
        parse_album_tracks(&response)
    }

    pub async fn artists(&self) -> Result<Vec<Artist>> {
        let response = self.get("getArtists", &[]).await?;
        parse_artists(&response)
    }

    pub async fn artist_albums(&self, artist_id: &str) -> Result<Vec<Album>> {
        let response = self.get("getArtist", &[("id", artist_id)]).await?;
        parse_artist_albums(&response)
    }

    pub async fn playlists(&self) -> Result<Vec<Playlist>> {
        let response = self.get("getPlaylists", &[]).await?;
        parse_playlists(&response)
    }

    pub async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        let response = self.get("getPlaylist", &[("id", playlist_id)]).await?;
        parse_playlist_tracks(&response)
    }

    /// Fetches and decodes one cover at the given pixel size.
    pub async fn cover_art(&self, cover_art_id: &str, size: u32) -> Result<CoverArt> {
        let url = self.credentials.cover_art_url(cover_art_id, size);
        let bytes = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(Error::Unreachable)?
            .bytes()
            .await
            .map_err(Error::Unreachable)?;
        decode_cover(&bytes)
    }

    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        let response = self
            .get(
                "search3",
                &[
                    ("query", query),
                    ("artistCount", "20"),
                    ("albumCount", "20"),
                    ("songCount", "50"),
                ],
            )
            .await?;
        parse_search(&response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            server_url: "http://music.local:4533".into(),
            username: "me".into(),
            salt: "abcdef0123456789".into(),
            token: "0123456789abcdef0123456789abcdef".into(),
            player_entity_id: None,
            player_name: None,
        }
    }

    /// Interop, not taste: Subsonic specifies this digest, so getting it wrong
    /// means every request is rejected.
    ///
    /// Both expectations were computed independently (`md5sum` and Python's
    /// hashlib) rather than by running this code and pasting what it produced
    /// -- a self-blessed "known answer" would pass no matter how wrong the
    /// implementation was.
    #[test]
    fn the_auth_token_is_md5_of_password_then_salt() {
        assert_eq!(md5_hex("sesameabc"), "ab3c35d2e29e863dbdd0df5bf3ec081f");
        // The shape the API actually uses: md5(password + salt).
        assert_eq!(md5_hex("hunter2c19b2d"), "1b41ecef65ff7799cf7a84cf2d505e08");
        let credentials = Credentials {
            salt: "c19b2d".into(),
            ..Credentials::new("http://x", "me", "hunter2")
        };
        assert_eq!(
            md5_hex(&format!("hunter2{}", credentials.salt)),
            "1b41ecef65ff7799cf7a84cf2d505e08"
        );
    }

    #[test]
    fn setup_derives_a_token_and_never_keeps_the_password() {
        let created = Credentials::new("http://music.local:4533/", "me", "hunter2");
        assert_eq!(created.server_url, "http://music.local:4533", "trailing slash trimmed");
        assert_eq!(created.token, md5_hex(&format!("hunter2{}", created.salt)));
        let stored = serde_json::to_string(&created).unwrap();
        assert!(!stored.contains("hunter2"), "the password must not reach disk: {stored}");
    }

    #[test]
    fn two_installs_do_not_share_a_salt() {
        let a = Credentials::new("http://x", "me", "pw");
        std::thread::sleep(Duration::from_millis(2));
        let b = Credentials::new("http://x", "me", "pw");
        assert_ne!(a.salt, b.salt);
    }

    #[test]
    fn urls_carry_auth_and_escape_their_parameters() {
        let url = credentials().endpoint("search3", &[("query", "the beatles & co")]);
        assert!(url.starts_with("http://music.local:4533/rest/search3?"));
        assert!(url.contains("u=me") && url.contains("s=abcdef0123456789") && url.contains("f=json"));
        assert!(url.contains("query=the%20beatles%20%26%20co"), "got {url}");
    }

    #[test]
    fn a_stream_url_is_a_plain_authenticated_get_any_player_can_fetch() {
        let url = credentials().stream_url("tr-1");
        assert!(url.contains("/rest/stream?"));
        assert!(url.contains("id=tr-1"));
        assert!(url.contains("u=me"));
    }

    /// The server's own wording is far more useful than "login failed".
    #[test]
    fn a_failed_response_surfaces_the_servers_message() {
        let body = r#"{"subsonic-response":{"status":"failed","version":"1.16.1",
            "error":{"code":40,"message":"Wrong username or password"}}}"#;
        match unwrap_response(body) {
            Err(Error::Server { code, message }) => {
                assert_eq!(code, 40);
                assert_eq!(message, "Wrong username or password");
            }
            other => panic!("expected a server error, got {other:?}"),
        }
    }

    #[test]
    fn a_non_json_reply_is_reported_rather_than_panicking() {
        assert!(matches!(unwrap_response("<html>nope</html>"), Err(Error::Malformed(_))));
        assert!(matches!(unwrap_response("{}"), Err(Error::Malformed(_))));
    }

    #[test]
    fn albums_and_their_tracks_parse() {
        let body = r#"{"subsonic-response":{"status":"ok","albumList2":{"album":[
            {"id":"al-1","name":"Kind of Blue","artist":"Miles Davis","year":1959,
             "songCount":5,"coverArt":"al-1"}]}}}"#;
        let albums = parse_albums(&unwrap_response(body).unwrap()).unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "Kind of Blue");
        assert_eq!(albums[0].artist.as_deref(), Some("Miles Davis"));

        let body = r#"{"subsonic-response":{"status":"ok","album":{"id":"al-1","song":[
            {"id":"tr-1","title":"So What","artist":"Miles Davis","duration":545,"track":1}]}}}"#;
        let tracks = parse_album_tracks(&unwrap_response(body).unwrap()).unwrap();
        assert_eq!(tracks[0].title, "So What");
        assert_eq!(tracks[0].duration_label(), "9:05");
    }

    /// Subsonic buckets artists under index letters; the UI wants a flat list.
    #[test]
    fn artists_are_flattened_out_of_their_alphabet_buckets() {
        let body = r#"{"subsonic-response":{"status":"ok","artists":{"index":[
            {"name":"A","artist":[{"id":"ar-1","name":"Aphex Twin","albumCount":9}]},
            {"name":"M","artist":[{"id":"ar-2","name":"Miles Davis","albumCount":40},
                                  {"id":"ar-3","name":"Mogwai","albumCount":11}]}]}}}"#;
        let artists = parse_artists(&unwrap_response(body).unwrap()).unwrap();
        let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Aphex Twin", "Miles Davis", "Mogwai"]);
    }

    /// Subsonic omits the array entirely when a collection is empty rather than
    /// sending `[]`, so "no results" must not look like a parse failure.
    #[test]
    fn an_empty_collection_is_an_empty_list_not_an_error() {
        let ok = unwrap_response(r#"{"subsonic-response":{"status":"ok"}}"#).unwrap();
        assert!(parse_albums(&ok).unwrap().is_empty());
        assert!(parse_artists(&ok).unwrap().is_empty());
        assert!(parse_playlists(&ok).unwrap().is_empty());
        let empty = parse_search(&ok).unwrap();
        assert!(empty.artists.is_empty() && empty.albums.is_empty() && empty.tracks.is_empty());

        let empty_search = unwrap_response(
            r#"{"subsonic-response":{"status":"ok","searchResult3":{}}}"#,
        )
        .unwrap();
        let empty = parse_search(&empty_search).unwrap();
        assert!(empty.artists.is_empty() && empty.albums.is_empty() && empty.tracks.is_empty());
    }

    #[test]
    fn search_splits_its_three_kinds_of_hit() {
        let body = r#"{"subsonic-response":{"status":"ok","searchResult3":{
            "artist":[{"id":"ar-1","name":"Portishead","albumCount":3}],
            "album":[{"id":"al-9","name":"Dummy","artist":"Portishead","songCount":11}],
            "song":[{"id":"tr-9","title":"Roads","artist":"Portishead","duration":302}]}}}"#;
        let results = parse_search(&unwrap_response(body).unwrap()).unwrap();
        assert_eq!(results.artists[0].name, "Portishead");
        assert_eq!(results.albums[0].name, "Dummy");
        assert_eq!(results.tracks[0].duration_label(), "5:02");
    }

    /// Files written before the player name was stored must still load, or an
    /// update would silently drop the speaker someone had already chosen.
    #[test]
    fn credentials_without_a_player_name_still_load() {
        let older = r#"{"server_url":"http://music.local:4533","username":"me",
            "salt":"abc","token":"def","player_entity_id":"media_player.kitchen"}"#;
        let parsed: Credentials = serde_json::from_str(older).expect("older file should load");
        assert_eq!(parsed.player_entity_id.as_deref(), Some("media_player.kitchen"));
        assert_eq!(parsed.player_name, None, "absent, not a parse failure");
    }

    #[test]
    fn cover_art_is_requested_at_the_size_we_intend_to_draw() {
        let url = credentials().cover_art_url("al-1", 128);
        assert!(url.contains("/rest/getCoverArt?"));
        assert!(url.contains("id=al-1") && url.contains("size=128"));
    }

    /// A server answering an error page with a 200 must not be mistaken for a
    /// cover -- that would put garbage pixels on a wall display.
    #[test]
    fn cover_art_that_is_not_an_image_is_rejected() {
        assert!(matches!(decode_cover(b"<html>not found</html>"), Err(Error::Malformed(_))));
        assert!(matches!(decode_cover(&[]), Err(Error::Malformed(_))));
    }

    #[test]
    fn a_real_png_decodes_to_rgba() {
        // A 1x1 opaque red PNG, byte-for-byte.
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d,
            0xb0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let cover = decode_cover(png).expect("a valid PNG should decode");
        assert_eq!((cover.width, cover.height), (1, 1));
        assert_eq!(cover.rgba.len(), 4, "one pixel, four channels");
        assert_eq!(cover.rgba[3], 255, "opaque");
    }

    /// Uses the `_at` entry points rather than pointing the env var at a
    /// scratch file: `set_var` mutates process-global state and is unsound once
    /// other threads exist, which under a parallel test runner they always do.
    /// An earlier version of this test did exactly that and made an unrelated
    /// updater test fail intermittently.
    #[test]
    fn credentials_round_trip_through_disk() {
        let path = std::env::temp_dir()
            .join(format!("skylight-music-test-{}.secret", std::process::id()));
        let _ = forget_credentials_at(&path);

        assert!(load_credentials_at(&path).is_none(), "nothing saved yet");
        let mut credentials = credentials();
        credentials.player_entity_id = Some("media_player.kitchen".into());
        credentials.player_name = Some("Kitchen".into());
        save_credentials_at(&path, &credentials).unwrap();
        assert_eq!(load_credentials_at(&path).as_ref(), Some(&credentials));

        forget_credentials_at(&path).unwrap();
        assert!(load_credentials_at(&path).is_none());
        // Forgetting something already gone is not an error -- the UI calls this
        // to reset and shouldn't have to care.
        forget_credentials_at(&path).unwrap();
    }

    #[test]
    fn album_sorts_map_to_the_types_subsonic_expects() {
        assert_eq!(AlbumSort::Newest.as_type(), "newest");
        assert_eq!(AlbumSort::Alphabetical.as_type(), "alphabeticalByName");
        assert_eq!(AlbumSort::Frequent.as_type(), "frequent");
    }

    #[test]
    fn sorts_round_trip_through_their_ui_index() {
        for sort in AlbumSort::ALL {
            assert_eq!(AlbumSort::from_index(sort.index() as usize), sort);
            assert!(!sort.label().is_empty());
        }
        // A stray index from the UI must not panic.
        assert_eq!(AlbumSort::from_index(99), AlbumSort::Newest);
    }

    /// `random` reshuffles per request, so "page 2" would just be more random
    /// albums overlapping page 1 -- offering Load more there would be a lie.
    #[test]
    fn random_is_not_pageable_but_the_rest_are() {
        assert!(!AlbumSort::Random.is_pageable());
        assert!(AlbumSort::Newest.is_pageable());
        assert!(AlbumSort::Alphabetical.is_pageable());
    }
}
