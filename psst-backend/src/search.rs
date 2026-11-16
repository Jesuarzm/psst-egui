//! Búsqueda global vía la Web API: el client_id interno de psst-core no sirve acá, hace falta uno propio con login OAuth aparte.

use std::{
    collections::HashMap,
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use librespot_core::{error::ErrorKind, session::Session};
use psst_core::oauth::{self, WebApiToken};
use tokio::runtime::Handle;

use crate::config;
use serde::Deserialize;

const REDIRECT_PORT: u16 = 8888;

#[derive(Clone)]
pub struct TrackHit {
    pub name: String,
    pub artist: String,
    pub artists: Vec<String>,
    pub artist_uris: Vec<String>,
    pub uri: String,
    pub duration: Duration,
    pub album: String,
    pub album_uri: String,
    pub year: Option<i32>,
    pub image: Option<String>,
    pub thumb: Option<String>,
}

#[derive(Clone)]
pub struct ArtistHit {
    pub name: String,
    pub uri: String,
    pub image: Option<String>,
}

#[derive(Clone)]
pub struct AlbumHit {
    pub name: String,
    pub artists: Vec<String>,
    pub year: Option<i32>,
    pub uri: String,
    pub image: Option<String>,
    pub thumb: Option<String>,
}

#[derive(Clone)]
pub struct PlaylistHit {
    pub name: String,
    pub uri: String,
    pub owner: String,
    pub image: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchKind {
    Track,
    Artist,
    Album,
    Playlist,
}

impl SearchKind {
    pub const ALL: [SearchKind; 4] = [SearchKind::Track, SearchKind::Artist, SearchKind::Album, SearchKind::Playlist];

    fn api_name(self) -> &'static str {
        match self {
            SearchKind::Track => "track",
            SearchKind::Artist => "artist",
            SearchKind::Album => "album",
            SearchKind::Playlist => "playlist",
        }
    }
}

#[derive(Clone)]
pub struct Section<T> {
    pub items: Vec<T>,
    pub total: usize,
}

impl<T> Default for Section<T> {
    fn default() -> Self {
        Self { items: Vec::new(), total: 0 }
    }
}

impl<T> Section<T> {
    /// Spotify no deja pasar de offset 1000 en /v1/search.
    pub fn has_more(&self) -> bool {
        self.items.len() < self.total.min(1000)
    }

    fn merge(&mut self, more: Section<T>) {
        if more.total > 0 || !more.items.is_empty() {
            self.total = more.total;
        }
        self.items.extend(more.items);
    }
}

#[derive(Default)]
pub struct SearchPage {
    pub tracks: Section<TrackHit>,
    pub artists: Section<ArtistHit>,
    pub albums: Section<AlbumHit>,
    pub playlists: Section<PlaylistHit>,
}

impl SearchPage {
    pub fn merge(&mut self, more: SearchPage) {
        self.tracks.merge(more.tracks);
        self.artists.merge(more.artists);
        self.albums.merge(more.albums);
        self.playlists.merge(more.playlists);
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.items.is_empty()
            && self.artists.items.is_empty()
            && self.albums.items.is_empty()
            && self.playlists.items.is_empty()
    }
}

pub fn webapi_client_id() -> Option<String> {
    config::get_string("webapi_client_id").filter(|k| !k.trim().is_empty())
}

pub fn set_webapi_client_id(client_id: &str) {
    config::set_value("webapi_client_id", Some(serde_json::Value::String(client_id.trim().to_string())));
}

fn stored_token() -> Option<WebApiToken> {
    config::read()
        .get("webapi_token")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
}

fn store_token(token: &WebApiToken) {
    if let Ok(value) = serde_json::to_value(token) {
        config::set_value("webapi_token", Some(value));
    }
}

pub fn webapi_configured() -> bool {
    webapi_client_id().is_some() && stored_token().is_some()
}

pub fn disconnect_webapi() {
    config::set_value("webapi_token", None);
    clear_user_name_cache();
}

pub fn webapi_token_if_available() -> Option<String> {
    let token = stored_token()?;
    if !token.is_expired() {
        return Some(token.access_token);
    }
    let client_id = webapi_client_id()?;
    let refresh_token = token.refresh_token.clone()?;
    let new_token = oauth::refresh_webapi_token(&client_id, &refresh_token).ok()?;
    let access_token = new_token.access_token.clone();
    store_token(&new_token);
    Some(access_token)
}

pub fn connect_webapi(auth_url_out: impl FnOnce(&str)) -> Result<(), String> {
    let client_id = webapi_client_id().ok_or("falta el Client ID")?;
    let (auth_url, pkce_verifier) = oauth::generate_webapi_auth_url(&client_id, REDIRECT_PORT);
    auth_url_out(&auth_url);
    let _ = open::that(&auth_url);

    let socket_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), REDIRECT_PORT);
    let code = oauth::get_authcode_listener(socket_addr, Duration::from_secs(180))
        .map_err(|e| e.to_string())?;
    let token = oauth::exchange_webapi_code_for_token(&client_id, REDIRECT_PORT, code, pkce_verifier)
        .map_err(|e| e.to_string())?;
    store_token(&token);
    clear_user_name_cache();
    Ok(())
}

pub const REDIRECT_URI: &str = "http://127.0.0.1:8888/login";

#[derive(Debug)]
enum ApiError {
    Status(u16, String),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Status(code, body) => {
                let reason = reqwest::StatusCode::from_u16(*code)
                    .ok()
                    .and_then(|s| s.canonical_reason())
                    .unwrap_or("");
                write!(f, "Spotify respondió {code} {reason}: {body}")
            }
            ApiError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl ApiError {
    /// 400 por `limit` demasiado alto (apps en modo Development: máx. 10).
    fn is_invalid_limit(&self) -> bool {
        matches!(self, ApiError::Status(400, body) if body.contains("Invalid limit"))
    }
}

const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

fn api_get<T: serde::de::DeserializeOwned>(
    access_token: &str,
    url: &str,
    query: &[(&str, String)],
) -> Result<T, ApiError> {
    let client = reqwest::blocking::Client::new();
    for _ in 0..3 {
        let response = client
            .get(url)
            .bearer_auth(access_token)
            .query(query)
            .send()
            .map_err(|e| ApiError::Other(e.to_string()))?;
        let status = response.status();
        if status.as_u16() == 429 {
            let wait = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(1));
            if wait > MAX_RETRY_AFTER {
                return Err(ApiError::Other(format!(
                    "Spotify pidió esperar {} s (demasiados pedidos)",
                    wait.as_secs()
                )));
            }
            std::thread::sleep(wait);
            continue;
        }
        if !status.is_success() {
            return Err(ApiError::Status(status.as_u16(), response.text().unwrap_or_default()));
        }
        return response.json().map_err(|e| ApiError::Other(e.to_string()));
    }
    Err(ApiError::Other("Spotify sigue respondiendo 429 (demasiados pedidos)".to_string()))
}

/// Se intenta 20; en modo Development solo se acepta 10 (400 "Invalid limit").
static PAGE_LIMIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(20);
const FALLBACK_PAGE_LIMIT: usize = 10;

fn api_get_paged<T: serde::de::DeserializeOwned>(
    access_token: &str,
    url: &str,
    query: &[(&str, String)],
    wanted: usize,
) -> Result<T, ApiError> {
    use std::sync::atomic::Ordering;
    let limit = wanted.min(PAGE_LIMIT.load(Ordering::Relaxed)).max(1);
    let with_limit = |limit: usize| {
        let mut q = query.to_vec();
        q.push(("limit", limit.to_string()));
        q
    };
    match api_get(access_token, url, &with_limit(limit)) {
        Err(e) if e.is_invalid_limit() && limit > FALLBACK_PAGE_LIMIT => {
            PAGE_LIMIT.store(FALLBACK_PAGE_LIMIT, Ordering::Relaxed);
            api_get(access_token, url, &with_limit(FALLBACK_PAGE_LIMIT))
        }
        other => other,
    }
}

#[derive(Deserialize)]
struct ApiImage {
    url: String,
    width: Option<u32>,
}

fn pick_image(images: &[ApiImage], target: u32) -> Option<String> {
    images
        .iter()
        .filter(|i| i.width.unwrap_or(0) >= target)
        .min_by_key(|i| i.width.unwrap_or(0))
        .or_else(|| images.iter().max_by_key(|i| i.width.unwrap_or(0)))
        .map(|i| i.url.clone())
}

const IMAGE_TARGET: u32 = 300;
const THUMB_TARGET: u32 = 64;

fn year_of(release_date: Option<&str>) -> Option<i32> {
    release_date.and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())
}

#[derive(Deserialize)]
struct ApiPage<T> {
    // Spotify devuelve `null` para ítems bloqueados/borrados por región.
    items: Vec<Option<T>>,
    #[serde(default)]
    total: usize,
}

impl<T> ApiPage<T> {
    fn into_section<U>(self, f: impl FnMut(T) -> U) -> Section<U> {
        Section {
            items: self.items.into_iter().flatten().map(f).collect(),
            total: self.total,
        }
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    tracks: Option<ApiPage<ApiTrack>>,
    artists: Option<ApiPage<ApiArtistFull>>,
    albums: Option<ApiPage<ApiAlbum>>,
    playlists: Option<ApiPage<ApiPlaylist>>,
}

#[derive(Deserialize)]
struct ApiTrack {
    name: String,
    uri: String,
    duration_ms: u64,
    artists: Vec<ApiArtist>,
    album: ApiAlbum,
}

#[derive(Deserialize)]
struct ApiArtist {
    name: String,
    #[serde(default)]
    uri: String,
}

#[derive(Deserialize)]
struct ApiArtistFull {
    name: String,
    uri: String,
    #[serde(default)]
    images: Vec<ApiImage>,
}

#[derive(Deserialize)]
struct ApiAlbum {
    name: String,
    #[serde(default)]
    uri: String,
    release_date: Option<String>,
    #[serde(default)]
    artists: Vec<ApiArtist>,
    #[serde(default)]
    images: Vec<ApiImage>,
}

#[derive(Deserialize)]
struct ApiPlaylist {
    name: String,
    uri: String,
    owner: ApiOwner,
    #[serde(default)]
    images: Option<Vec<ApiImage>>,
}

#[derive(Deserialize)]
struct ApiOwner {
    display_name: Option<String>,
}

fn track_hit(t: ApiTrack) -> TrackHit {
    let artists: Vec<String> = t.artists.iter().map(|a| a.name.clone()).collect();
    TrackHit {
        artist: crate::library::join_artists(&artists),
        artists,
        artist_uris: t.artists.into_iter().map(|a| a.uri).collect(),
        name: t.name,
        uri: t.uri,
        duration: Duration::from_millis(t.duration_ms),
        year: year_of(t.album.release_date.as_deref()),
        image: pick_image(&t.album.images, IMAGE_TARGET),
        thumb: pick_image(&t.album.images, THUMB_TARGET),
        album_uri: t.album.uri,
        album: t.album.name,
    }
}

fn album_hit(a: ApiAlbum) -> AlbumHit {
    AlbumHit {
        artists: a.artists.into_iter().map(|x| x.name).collect(),
        year: year_of(a.release_date.as_deref()),
        image: pick_image(&a.images, IMAGE_TARGET),
        thumb: pick_image(&a.images, THUMB_TARGET),
        name: a.name,
        uri: a.uri,
    }
}

pub fn search(
    access_token: &str,
    query: &str,
    kinds: &[SearchKind],
    limit: usize,
    offset: usize,
) -> Result<SearchPage, String> {
    let types: Vec<&str> = kinds.iter().map(|k| k.api_name()).collect();
    let parsed: SearchResponse = api_get_paged(
        access_token,
        "https://api.spotify.com/v1/search",
        &[
            ("q", query.to_string()),
            ("type", types.join(",")),
            ("offset", offset.to_string()),
            ("market", "from_token".to_string()),
        ],
        limit.min(50),
    )
    .map_err(|e| e.to_string())?;

    Ok(SearchPage {
        tracks: parsed.tracks.map(|p| p.into_section(track_hit)).unwrap_or_default(),
        artists: parsed
            .artists
            .map(|p| {
                p.into_section(|a| ArtistHit {
                    image: pick_image(&a.images, IMAGE_TARGET),
                    name: a.name,
                    uri: a.uri,
                })
            })
            .unwrap_or_default(),
        albums: parsed.albums.map(|p| p.into_section(album_hit)).unwrap_or_default(),
        playlists: parsed
            .playlists
            .map(|p| {
                p.into_section(|p| PlaylistHit {
                    image: pick_image(p.images.as_deref().unwrap_or_default(), IMAGE_TARGET),
                    name: p.name,
                    uri: p.uri,
                    owner: p.owner.display_name.unwrap_or_default(),
                })
            })
            .unwrap_or_default(),
    })
}

pub fn normalize_name(s: &str) -> String {
    let folded: String = s
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ç' => 'c',
            other => other,
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn spotify_id(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or(uri)
}

#[derive(Deserialize)]
struct TopTracksResponse {
    tracks: Vec<ApiTrack>,
}

/// `top-tracks` da 403 en modo Development: no se reintenta en la sesión.
static TOP_TRACKS_FORBIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn top_tracks_cache() -> &'static Mutex<HashMap<String, Vec<TrackHit>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<TrackHit>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Si `top-tracks` da 403, se busca `artist:"Nombre"` y se filtran sus canciones.
pub fn artist_top_tracks(access_token: &str, artist_uri: &str, artist_name: &str) -> Result<Vec<TrackHit>, String> {
    use std::sync::atomic::Ordering;
    if let Some(cached) = top_tracks_cache().lock().unwrap().get(artist_uri) {
        return Ok(cached.clone());
    }

    let mut tracks = None;
    if !TOP_TRACKS_FORBIDDEN.load(Ordering::Relaxed) {
        let url = format!("https://api.spotify.com/v1/artists/{}/top-tracks", spotify_id(artist_uri));
        match api_get::<TopTracksResponse>(access_token, &url, &[("market", "from_token".to_string())]) {
            Ok(r) => tracks = Some(r.tracks.into_iter().map(track_hit).take(5).collect::<Vec<_>>()),
            Err(ApiError::Status(403 | 404, _)) => TOP_TRACKS_FORBIDDEN.store(true, Ordering::Relaxed),
            Err(e) => return Err(e.to_string()),
        }
    }
    let tracks = match tracks {
        Some(t) => t,
        None => {
            let query = format!("artist:\"{}\"", artist_name.replace('"', ""));
            let mut found = Vec::new();
            let mut offset = 0;
            for _ in 0..3 {
                let page = search(access_token, &query, &[SearchKind::Track], 20, offset)?.tracks;
                offset += page.items.len();
                let more = page.has_more() && !page.items.is_empty();
                found.extend(page.items.into_iter().filter(|t| t.artist_uris.iter().any(|u| u == artist_uri)));
                if found.len() >= 5 || !more {
                    break;
                }
            }
            found.truncate(5);
            found
        }
    };
    top_tracks_cache().lock().unwrap().insert(artist_uri.to_string(), tracks.clone());
    Ok(tracks)
}

pub fn artist_by_uri(access_token: &str, artist_uri: &str) -> Result<ArtistHit, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, ArtistHit>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(artist_uri) {
        return Ok(hit.clone());
    }
    let url = format!("https://api.spotify.com/v1/artists/{}", spotify_id(artist_uri));
    let a: ApiArtistFull = api_get(access_token, &url, &[]).map_err(|e| e.to_string())?;
    let hit = ArtistHit {
        image: pick_image(&a.images, IMAGE_TARGET),
        name: a.name,
        uri: a.uri,
    };
    cache.lock().unwrap().insert(artist_uri.to_string(), hit.clone());
    Ok(hit)
}

pub fn find_artist(access_token: &str, name: &str) -> Result<ArtistHit, String> {
    let page = search(access_token, &format!("artist:\"{}\"", name.replace('"', "")), &[SearchKind::Artist], 10, 0)?;
    let wanted = normalize_name(name);
    let mut artists = page.artists.items;
    let exact = artists.iter().position(|a| normalize_name(&a.name) == wanted);
    match exact {
        Some(i) => Ok(artists.swap_remove(i)),
        None => artists.into_iter().next().ok_or_else(|| format!("no se encontró al artista \"{name}\"")),
    }
}

pub fn artist_albums(access_token: &str, artist_uri: &str, offset: usize) -> Result<Section<AlbumHit>, String> {
    static CACHE: OnceLock<Mutex<HashMap<(String, usize), Section<AlbumHit>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (artist_uri.to_string(), offset);
    if let Some(section) = cache.lock().unwrap().get(&key) {
        return Ok(section.clone());
    }
    let url = format!("https://api.spotify.com/v1/artists/{}/albums", spotify_id(artist_uri));
    let page: ApiPage<ApiAlbum> = api_get_paged(
        access_token,
        &url,
        &[
            ("include_groups", "album,single".to_string()),
            ("market", "from_token".to_string()),
            ("offset", offset.to_string()),
        ],
        50,
    )
    .map_err(|e| e.to_string())?;
    let section = page.into_section(album_hit);
    cache.lock().unwrap().insert(key, section.clone());
    Ok(section)
}

#[derive(Deserialize)]
struct ApiAlbumTrack {
    name: String,
    uri: String,
    duration_ms: u64,
    artists: Vec<ApiArtist>,
}

const MAX_ALBUM_TRACKS: usize = 200;

pub fn album_tracks(access_token: &str, album: &AlbumHit) -> Result<Vec<TrackHit>, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<TrackHit>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(tracks) = cache.lock().unwrap().get(&album.uri) {
        return Ok(tracks.clone());
    }
    let url = format!("https://api.spotify.com/v1/albums/{}/tracks", spotify_id(&album.uri));
    let mut tracks = Vec::new();
    loop {
        let page: ApiPage<ApiAlbumTrack> = api_get_paged(
            access_token,
            &url,
            &[("market", "from_token".to_string()), ("offset", tracks.len().to_string())],
            50,
        )
        .map_err(|e| e.to_string())?;
        let total = page.total;
        let before = tracks.len();
        tracks.extend(page.items.into_iter().flatten().map(|t| {
            let artists: Vec<String> = t.artists.iter().map(|a| a.name.clone()).collect();
            TrackHit {
                artist: crate::library::join_artists(&artists),
                artists,
                artist_uris: t.artists.into_iter().map(|a| a.uri).collect(),
                name: t.name,
                uri: t.uri,
                duration: Duration::from_millis(t.duration_ms),
                album: album.name.clone(),
                album_uri: album.uri.clone(),
                year: album.year,
                image: album.image.clone(),
                thumb: album.thumb.clone(),
            }
        }));
        if tracks.len() == before || tracks.len() >= total.min(MAX_ALBUM_TRACKS) {
            break;
        }
    }
    cache.lock().unwrap().insert(album.uri.clone(), tracks.clone());
    Ok(tracks)
}

#[derive(Deserialize)]
struct ApiUser {
    display_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NameError {
    Empty,
    /// 403 — la Web API en modo Development no deja ver perfiles ajenos.
    Forbidden,
    NotFound,
    NoToken,
    /// fallo de red u otro error transitorio — no se cachea.
    Other(String),
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameError::Empty => write!(f, "el perfil no tiene nombre"),
            NameError::Forbidden => write!(f, "Spotify no deja ver este perfil (403)"),
            NameError::NotFound => write!(f, "el usuario no existe (404)"),
            NameError::NoToken => write!(f, "búsqueda (Web API) sin conectar"),
            NameError::Other(e) => write!(f, "{e}"),
        }
    }
}

type NameFetch = Result<Option<String>, NameError>;

fn webapi_get_display_name(access_token: &str, url: &str) -> NameFetch {
    let response = reqwest::blocking::Client::new()
        .get(url)
        .bearer_auth(access_token)
        .send()
        .map_err(|e| NameError::Other(e.to_string()))?;

    match response.status().as_u16() {
        403 => return Err(NameError::Forbidden),
        404 => return Err(NameError::NotFound),
        _ if !response.status().is_success() => {
            return Err(NameError::Other(format!("Spotify respondió {}", response.status())));
        }
        _ => {}
    }

    let parsed: ApiUser = response.json().map_err(|e| NameError::Other(e.to_string()))?;
    Ok(parsed.display_name)
}

/// `/v1/users/{id}` da 403 en modo Development aunque el token sea válido.
fn fetch_user_display_name(access_token: &str, user_id: &str) -> NameFetch {
    webapi_get_display_name(access_token, &format!("https://api.spotify.com/v1/users/{user_id}"))
}

fn fetch_own_display_name(access_token: &str) -> NameFetch {
    webapi_get_display_name(access_token, "https://api.spotify.com/v1/me")
}

#[derive(Deserialize)]
struct SpProfile {
    name: Option<String>,
}

/// Endpoint interno `/user-profile-view/v3/profile/{id}`: sin token ni el 403 de modo Development, por eso va primero.
fn fetch_profile_via_session(session: &Session, rt: &Handle, user_id: &str) -> NameFetch {
    let bytes = rt
        .block_on(session.spclient().get_user_profile(user_id, Some(0), Some(0)))
        .map_err(|e| match e.kind {
            ErrorKind::NotFound => NameError::NotFound,
            ErrorKind::PermissionDenied => NameError::Forbidden,
            _ => NameError::Other(e.to_string()),
        })?;
    let parsed: SpProfile = serde_json::from_slice(&bytes).map_err(|e| NameError::Other(e.to_string()))?;
    Ok(parsed.name)
}

fn user_name_cache_path() -> String {
    let dir = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    format!("{dir}/user-names.json")
}

fn user_name_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut data: HashMap<String, String> = std::fs::read_to_string(user_name_cache_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // las versiones viejas guardaban el ID como nombre: esas entradas se descartan.
        data.retain(|id, name| !name.is_empty() && name != id);
        Mutex::new(data)
    })
}

const NEGATIVE_TTL: Duration = Duration::from_secs(10 * 60);

/// Fallos (vacío/403/404) solo en memoria y con expiración: cachearlos en disco los dejaba pegados para siempre.
fn negative_cache() -> &'static Mutex<HashMap<String, (Instant, NameError)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, NameError)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn persist_user_names(cache: &HashMap<String, String>) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(user_name_cache_path(), json);
    }
}

pub fn clear_user_name_cache() {
    user_name_cache().lock().unwrap().clear();
    negative_cache().lock().unwrap().clear();
    let _ = std::fs::remove_file(user_name_cache_path());
}

pub fn display_name_for(session: &Session, rt: &Handle, user_id: &str, is_self: bool) -> Result<String, NameError> {
    if let Some(cached) = user_name_cache().lock().unwrap().get(user_id) {
        return Ok(cached.clone());
    }
    if let Some((at, err)) = negative_cache().lock().unwrap().get(user_id) {
        if at.elapsed() < NEGATIVE_TTL {
            return Err(err.clone());
        }
    }

    let usable = |r: NameFetch| match r {
        Ok(Some(name)) if !name.trim().is_empty() && name != user_id => Ok(name),
        Ok(_) => Err(NameError::Empty),
        Err(e) => Err(e),
    };

    let mut result = usable(fetch_profile_via_session(session, rt, user_id));
    if matches!(result, Err(ref e) if *e != NameError::NotFound) {
        result = match webapi_token_if_available() {
            Some(token) if is_self => usable(fetch_own_display_name(&token)),
            Some(token) => usable(fetch_user_display_name(&token, user_id)),
            None => result.map_err(|e| if matches!(e, NameError::Other(_)) { NameError::NoToken } else { e }),
        };
    }

    match &result {
        Ok(name) => {
            let mut cache = user_name_cache().lock().unwrap();
            cache.insert(user_id.to_string(), name.clone());
            persist_user_names(&cache);
        }
        Err(NameError::Other(_)) => {}
        Err(err) => {
            negative_cache()
                .lock()
                .unwrap()
                .insert(user_id.to_string(), (Instant::now(), err.clone()));
        }
    }
    result
}
