//! Usa el endpoint interno `color-lyrics/v2` de spclient (no documentado), con la sesión de librería.

use std::time::Duration;

use librespot_core::{session::Session, spotify_id::SpotifyId};
use serde::Deserialize;

pub struct LyricLine {
    pub start: Duration,
    pub text: String,
}

#[derive(Deserialize)]
struct LyricsResponse {
    lyrics: LyricsBody,
}

#[derive(Deserialize)]
struct LyricsBody {
    lines: Vec<ApiLine>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiLine {
    start_time_ms: String,
    words: String,
}

/// Spotify responde 404 cuando la canción no tiene letras.
pub async fn fetch_lyrics(session: &Session, track_uri: &str) -> Result<Vec<LyricLine>, String> {
    let id62 = track_uri
        .strip_prefix("spotify:track:")
        .ok_or_else(|| "uri de track inválida".to_string())?;
    let id = SpotifyId::from_base62(id62).map_err(|e| e.to_string())?;

    let bytes = session
        .spclient()
        .get_lyrics(&id)
        .await
        .map_err(|_| "sin letras para esta canción".to_string())?;

    let parsed: LyricsResponse = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;

    Ok(parsed
        .lyrics
        .lines
        .into_iter()
        .filter_map(|l| {
            let ms: u64 = l.start_time_ms.parse().ok()?;
            Some(LyricLine {
                start: Duration::from_millis(ms),
                text: l.words,
            })
        })
        .collect())
}
