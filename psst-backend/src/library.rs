//! Sesión aparte (protocolo de librespot) solo para leer la librería: el OAuth normal sirve acá, el approval que falta es solo para audio.

use std::{
    collections::{HashMap, HashSet},
    env,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use futures::{stream, StreamExt};
use librespot_core::{
    authentication::Credentials, cache::Cache, config::SessionConfig, date::Date,
    session::Session, spotify_id::SpotifyId, FileId,
};
use librespot_metadata::{Metadata, Playlist as LsPlaylist, Track as LsTrack};
use librespot_protocol::{context_page::ContextPage, playlist4_external::SelectedListContent};
use protobuf::Message;
use serde::{Deserialize, Serialize};

const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";
const NAME_CONCURRENCY: usize = 24;

/// Playlist "DJ" de Spotify: `LsPlaylist::get()` siempre le da 0 tracks, por eso se filtra.
const DJ_PLAYLIST_ID: &str = "37i9dQZF1EYkqdzj48dyYq";

#[derive(Clone, Serialize, Deserialize)]
struct CachedTrackData {
    name: String,
    duration_ms: u64,
    /// se sigue escribiendo por compatibilidad con caches de versiones viejas.
    artist: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    artist_uris: Vec<String>,
    album: String,
    year: Option<i32>,
    cover: Option<[u8; 20]>,
    cover_large: Option<[u8; 20]>,
}

impl CachedTrackData {
    fn artists(&self) -> Vec<String> {
        if !self.artists.is_empty() {
            self.artists.clone()
        } else if !self.artist.is_empty() {
            vec![self.artist.clone()]
        } else {
            Vec::new()
        }
    }
}

pub fn join_artists(artists: &[String]) -> String {
    artists.join(", ")
}

fn track_cache_path() -> String {
    let dir = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    format!("{dir}/track-cache.json")
}

fn track_cache() -> &'static Mutex<HashMap<String, CachedTrackData>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedTrackData>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let data = std::fs::read_to_string(track_cache_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Mutex::new(data)
    })
}

fn persist_track_cache(cache: &HashMap<String, CachedTrackData>) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(track_cache_path(), json);
    }
}

fn cached_to_track_ref(uri: &str, added_at: Option<String>, data: &CachedTrackData) -> TrackRef {
    let artists = data.artists();
    TrackRef {
        name: data.name.clone(),
        uri: uri.to_string(),
        duration: Duration::from_millis(data.duration_ms),
        artist: join_artists(&artists),
        artists,
        artist_uris: data.artist_uris.clone(),
        album: data.album.clone(),
        year: data.year,
        cover: data.cover.map(FileId),
        cover_large: data.cover_large.map(FileId),
        added_at,
    }
}

#[derive(Clone)]
pub struct PlaylistRef {
    pub name: String,
    pub uri: String,
    pub track_count: i32,
    pub cover_url: Option<String>,
    pub fallback_cover: Option<FileId>,
    pub description: String,
    pub owner: String,
    pub revision: String,
}

pub struct TrackRef {
    pub name: String,
    pub uri: String,
    pub duration: Duration,
    pub artist: String,
    pub artists: Vec<String>,
    pub artist_uris: Vec<String>,
    pub album: String,
    pub year: Option<i32>,
    pub cover: Option<FileId>,
    pub cover_large: Option<FileId>,
    pub added_at: Option<String>,
}

pub fn has_cached_credentials() -> bool {
    let home = env::var("HOME").unwrap_or_default();
    std::path::Path::new(&format!("{home}/.cache/spotify-client/credentials.json")).exists()
}

pub async fn connect() -> Session {
    let cache_dir = format!("{}/.cache/spotify-client", env::var("HOME").unwrap());
    let cache = Cache::new(Some(&cache_dir), None, None, None).expect("cache");

    let credentials = if let Some(creds) = cache.credentials() {
        creds
    } else {
        println!("Login (librería) por OAuth...");
        librespot_oauth::OAuthClientBuilder::new(CLIENT_ID, REDIRECT_URI, vec!["streaming"])
            .open_in_browser()
            .build()
            .and_then(|c| c.get_access_token())
            .map(|t| Credentials::with_access_token(t.access_token))
            .expect("login OAuth de la libreria fallo")
    };

    let session = Session::new(SessionConfig::default(), Some(cache));
    session
        .connect(credentials, true)
        .await
        .expect("no se pudo conectar la sesion de libreria");
    session
}

pub fn liked_playlist(username: String) -> PlaylistRef {
    PlaylistRef {
        name: "♥ Me gusta".to_string(),
        uri: "collection".to_string(),
        track_count: 0,
        cover_url: None,
        fallback_cover: None,
        description: String::new(),
        owner: username,
        revision: String::new(),
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PlaylistCache {
    user: String,
    playlists: Vec<CachedPlaylist>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct CachedPlaylist {
    uri: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    revision: String,
    name: String,
    track_count: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fallback_cover: Option<[u8; 20]>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    description: String,
    owner: String,
}

impl CachedPlaylist {
    fn new(p: PlaylistRef, revision: String) -> Self {
        Self {
            uri: p.uri,
            revision,
            name: p.name,
            track_count: p.track_count,
            cover_url: p.cover_url,
            fallback_cover: p.fallback_cover.map(|f| f.0),
            description: p.description,
            owner: p.owner,
        }
    }

    fn to_ref(&self) -> PlaylistRef {
        PlaylistRef {
            name: self.name.clone(),
            uri: self.uri.clone(),
            track_count: self.track_count,
            cover_url: self.cover_url.clone(),
            fallback_cover: self.fallback_cover.map(FileId),
            description: self.description.clone(),
            owner: self.owner.clone(),
            revision: self.revision.clone(),
        }
    }
}

fn playlist_cache_path() -> String {
    let dir = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    format!("{dir}/playlist-cache.json")
}

fn load_playlist_cache(username: &str) -> Option<PlaylistCache> {
    let data = std::fs::read_to_string(playlist_cache_path()).ok()?;
    let cache: PlaylistCache = serde_json::from_str(&data).ok()?;
    (cache.user == username).then_some(cache)
}

fn persist_playlist_cache(cache: &PlaylistCache) {
    let Ok(json) = serde_json::to_string(cache) else { return };
    let path = playlist_cache_path();
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn cached_playlists(username: &str) -> Option<Vec<PlaylistRef>> {
    let cache = load_playlist_cache(username)?;
    let mut out = vec![liked_playlist(username.to_string())];
    out.extend(cache.playlists.iter().map(CachedPlaylist::to_ref));
    Some(out)
}

pub async fn refresh_playlists(session: &Session) -> Option<Vec<PlaylistRef>> {
    let username = session.username();
    let old_list = load_playlist_cache(&username).map(|c| c.playlists).unwrap_or_default();
    let old: HashMap<&str, &CachedPlaylist> = old_list.iter().map(|p| (p.uri.as_str(), p)).collect();

    // el rootlist es protobuf (SelectedListContent), no JSON como get_context.
    let bytes = match session.spclient().get_rootlist(0, Some(200)).await {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("playlists: no se pudo leer la lista de playlists (rootlist): {e:?}");
            return None;
        }
    };
    let content = match SelectedListContent::parse_from_bytes(&bytes) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("playlists: rootlist ilegible: {e}");
            return None;
        }
    };

    // `decorate=revision` manda un meta_item por item, en el mismo orden.
    let items = &content.contents.items;
    let metas = &content.contents.meta_items;
    let aligned = metas.len() == items.len();

    let (mut reused, mut fetched, mut kept) = (0, 0, 0);
    let mut entries = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Some(uri) = item.uri.as_deref() else {
            continue;
        };
        let Some(id_str) = uri.strip_prefix("spotify:playlist:") else {
            continue;
        };
        if id_str == DJ_PLAYLIST_ID {
            continue;
        }
        let revision = if aligned { hex(metas[i].revision()) } else { String::new() };
        let cached = old.get(uri).copied();
        if let Some(c) = cached.filter(|c| !revision.is_empty() && c.revision == revision) {
            reused += 1;
            entries.push(c.clone());
        } else if let Some(p) = playlist_ref(session, uri).await {
            fetched += 1;
            entries.push(CachedPlaylist::new(p, revision));
        } else if let Some(c) = cached {
            // falló: queda la de antes con su revisión vieja, para reintentarla en el próximo arranque.
            kept += 1;
            entries.push(c.clone());
        }
    }
    eprintln!("playlists: {reused} sin cambios, {fetched} pedidas a Spotify, {kept} del cache por error");

    let mut out = vec![liked_playlist(username.clone())];
    out.extend(entries.iter().map(CachedPlaylist::to_ref));
    if entries != old_list {
        persist_playlist_cache(&PlaylistCache { user: username, playlists: entries });
    }
    Some(out)
}

pub async fn playlist_ref(session: &Session, uri: &str) -> Option<PlaylistRef> {
    let id = SpotifyId::from_base62(uri.strip_prefix("spotify:playlist:")?).ok()?;
    let p = match LsPlaylist::get(session, &id).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("playlists: no se pudo leer {uri}: {e:?}");
            return None;
        }
    };
    let cover_url = p
        .attributes
        .picture_sizes
        .iter()
        .find(|pic| pic.target_name == "small")
        .or_else(|| p.attributes.picture_sizes.first())
        .map(|pic| pic.url.clone());

    let fallback_cover = if cover_url.is_none() {
        first_track_cover(session, &p).await
    } else {
        None
    };

    Some(PlaylistRef {
        name: p.name().to_string(),
        uri: uri.to_string(),
        track_count: p.length,
        cover_url,
        fallback_cover,
        description: strip_html(&p.attributes.description),
        owner: p.id.username.clone(),
        revision: String::new(),
    })
}

fn artist_uris_of(artists: &[librespot_metadata::artist::Artist]) -> Vec<String> {
    artists
        .iter()
        .filter_map(|a| a.id.to_base62().ok())
        .map(|id| format!("spotify:artist:{id}"))
        .collect()
}

fn strip_html(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode_entities(&text).trim().to_string()
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let decoded = after[1..].find(';').filter(|&end| end <= 10).and_then(|end| {
            let name = &after[1..=end];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((ch, end + 2))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &after[len..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::strip_html;

    #[test]
    fn limpia_html_de_descripciones() {
        assert_eq!(
            strip_html("<a href=spotify:playlist:x>Bad Omens</a>, <a href=spotify:playlist:y>Spiritbox</a> &amp; más"),
            "Bad Omens, Spiritbox & más"
        );
        assert_eq!(strip_html("It&#x27;s &#39;ok&#39; &lt;3"), "It's 'ok' <3");
        assert_eq!(strip_html("AT&T sin cerrar &"), "AT&T sin cerrar &");
        assert_eq!(strip_html(""), "");
    }
}

async fn first_track_cover(session: &Session, playlist: &LsPlaylist) -> Option<FileId> {
    let first_uri = playlist.tracks().next()?.to_uri().ok()?;

    if let Some(cached) = track_cache().lock().unwrap().get(&first_uri) {
        return cached.cover.map(FileId);
    }

    let id62 = first_uri.strip_prefix("spotify:track:")?;
    let id = SpotifyId::from_base62(id62).ok()?;
    let track = LsTrack::get(session, &id).await.ok()?;
    track.album.covers.iter().min_by_key(|c| c.width).map(|c| c.id)
}

pub async fn tracks(session: &Session, uri: &str) -> Vec<TrackRef> {
    let items: Vec<(String, Option<i64>)> = if uri == "collection" {
        liked_track_uris(session).await
    } else {
        let Some(id_str) = uri.strip_prefix("spotify:playlist:") else {
            return vec![];
        };
        let Ok(id) = SpotifyId::from_base62(id_str) else {
            return vec![];
        };
        let Ok(playlist) = LsPlaylist::get(session, &id).await else {
            return vec![];
        };
        playlist
            .contents
            .items
            .iter()
            .filter_map(|item| {
                let uri = item.id.to_uri().ok()?;
                Some((uri, Some(item.attributes.timestamp.as_timestamp_ms())))
            })
            .collect()
    };
    resolve_tracks(session, items).await
}

fn format_added_at(ms: i64) -> Option<String> {
    let d = Date::from_timestamp_ms(ms).ok()?.as_utc();
    Some(format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day()))
}

async fn resolve_tracks(session: &Session, items: Vec<(String, Option<i64>)>) -> Vec<TrackRef> {
    let mut resolved: Vec<(usize, TrackRef)> = Vec::with_capacity(items.len());
    let mut to_fetch: Vec<(usize, String, Option<i64>)> = Vec::new();

    {
        let cache = track_cache().lock().unwrap();
        for (idx, (uri, added_at_ms)) in items.into_iter().enumerate() {
            match cache.get(&uri) {
                Some(data) => {
                    let added_at = added_at_ms.and_then(format_added_at);
                    resolved.push((idx, cached_to_track_ref(&uri, added_at, data)));
                }
                None => to_fetch.push((idx, uri, added_at_ms)),
            }
        }
    }

    if !to_fetch.is_empty() {
        let fetched: Vec<(usize, TrackRef, Option<(String, CachedTrackData)>)> = stream::iter(to_fetch)
            .map(|(idx, uri, added_at_ms)| async move {
                let added_at = added_at_ms.and_then(format_added_at);
                let id = uri
                    .strip_prefix("spotify:track:")
                    .and_then(|id| SpotifyId::from_base62(id).ok());
                match id {
                    Some(id) => match LsTrack::get(session, &id).await {
                        Ok(t) => {
                            let cover = t.album.covers.iter().min_by_key(|c| c.width).map(|c| c.id);
                            let cover_large = t.album.covers.iter().max_by_key(|c| c.width).map(|c| c.id);
                            let name = t.name;
                            let duration_ms = t.duration.max(0) as u64;
                            let artists: Vec<String> = t.artists.iter().map(|a| a.name.clone()).collect();
                            let artist_uris = artist_uris_of(&t.artists);
                            let artist = join_artists(&artists);
                            let album = t.album.name;
                            let year = (t.album.date.year() != 0).then(|| t.album.date.year());

                            let cached = CachedTrackData {
                                name: name.clone(),
                                duration_ms,
                                artist: artists.first().cloned().unwrap_or_default(),
                                artists: artists.clone(),
                                artist_uris: artist_uris.clone(),
                                album: album.clone(),
                                year,
                                cover: cover.map(|f| f.0),
                                cover_large: cover_large.map(|f| f.0),
                            };
                            let track_ref = TrackRef {
                                name,
                                uri: uri.clone(),
                                duration: Duration::from_millis(duration_ms),
                                artist,
                                artists,
                                artist_uris,
                                album,
                                year,
                                cover,
                                cover_large,
                                added_at,
                            };
                            (idx, track_ref, Some((uri, cached)))
                        }
                        Err(_) => (idx, empty_track_ref(uri, added_at), None),
                    },
                    None => (idx, empty_track_ref(uri, added_at), None),
                }
            })
            .buffer_unordered(NAME_CONCURRENCY)
            .collect()
            .await;

        let mut cache = track_cache().lock().unwrap();
        let mut any_new = false;
        for (idx, track_ref, cache_entry) in fetched {
            if let Some((uri, data)) = cache_entry {
                cache.insert(uri, data);
                any_new = true;
            }
            resolved.push((idx, track_ref));
        }
        if any_new {
            persist_track_cache(&cache);
        }
    }

    resolved.sort_by_key(|(idx, _)| *idx);
    resolved.into_iter().map(|(_, t)| t).collect()
}

/// Una petición por canción reproducida, nunca en lote: en lote vuelven los 429.
pub async fn backfill_artists_loop(session: Session, playback: crate::playback::Playback) {
    let mut tried: HashSet<String> = HashSet::new();
    let mut track = playback.watch_track();
    while track.changed().await.is_ok() {
        let Some(uri) = track.borrow_and_update().clone() else { continue };
        let stale = track_cache()
            .lock()
            .unwrap()
            .get(&uri)
            .is_some_and(|d| d.artists.is_empty() || d.artist_uris.is_empty());
        if !stale || !tried.insert(uri.clone()) {
            continue;
        }
        if let Some((artists, uris)) = refresh_cached_artists(&session, &uri).await {
            playback.update_track_artists(&uri, artists, uris);
        }
    }
}

async fn refresh_cached_artists(session: &Session, uri: &str) -> Option<(Vec<String>, Vec<String>)> {
    let id = SpotifyId::from_base62(uri.strip_prefix("spotify:track:")?).ok()?;
    let track = LsTrack::get(session, &id).await.ok()?;
    let artists: Vec<String> = track.artists.iter().map(|a| a.name.clone()).collect();
    let uris = artist_uris_of(&track.artists);
    if artists.is_empty() {
        return None;
    }
    let mut cache = track_cache().lock().unwrap();
    if let Some(entry) = cache.get_mut(uri) {
        entry.artists = artists.clone();
        entry.artist_uris = uris.clone();
        persist_track_cache(&cache);
    }
    Some((artists, uris))
}

fn empty_track_ref(uri: String, added_at: Option<String>) -> TrackRef {
    TrackRef {
        name: uri.clone(),
        uri,
        duration: Duration::ZERO,
        artist: String::new(),
        artists: Vec::new(),
        artist_uris: Vec::new(),
        album: String::new(),
        year: None,
        cover: None,
        cover_large: None,
        added_at,
    }
}

pub async fn liked_uri_set(session: &Session) -> HashSet<String> {
    liked_track_uris(session).await.into_iter().map(|(uri, _)| uri).collect()
}

async fn liked_track_uris(session: &Session) -> Vec<(String, Option<i64>)> {
    let username = session.username();
    let collection_uri = format!("spotify:user:{username}:collection");
    let ctx = match session.spclient().get_context(&collection_uri).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("me gusta: no se pudo leer la colección: {e:?}");
            return vec![];
        }
    };

    let mut out = Vec::new();
    let mut next_page_url = None;
    for page in &ctx.pages {
        collect_uris(page, &mut out);
        if page.next_page_url.is_some() {
            next_page_url = page.next_page_url.clone();
        }
    }
    // tope de páginas por si `next_page_url` formara un ciclo.
    let mut guard = 0;
    while let Some(url) = next_page_url.take() {
        guard += 1;
        if guard > 500 {
            break;
        }
        let Ok(bytes) = session.spclient().get_next_page(&url).await else {
            break;
        };
        let json = String::from_utf8_lossy(&bytes);
        let Ok(page) = protobuf_json_mapping::parse_from_str::<ContextPage>(&json) else {
            break;
        };
        collect_uris(&page, &mut out);
        next_page_url = page.next_page_url.clone();
    }
    out
}

fn collect_uris(page: &ContextPage, out: &mut Vec<(String, Option<i64>)>) {
    for t in &page.tracks {
        if let Some(uri) = &t.uri {
            let added_at = t.metadata.get("added_at").and_then(|s| s.parse::<i64>().ok());
            out.push((uri.clone(), added_at));
        }
    }
}
