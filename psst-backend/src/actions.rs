//! En modo Development, `/me/tracks` y `/playlists/{id}/tracks` dan 403: se usan `/me/library` y `/playlists/{id}/items`.

const API: &str = "https://api.spotify.com/v1";

fn check(response: reqwest::blocking::Response) -> Result<(), String> {
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().unwrap_or_default();
    Err(format!("Spotify respondió {status}: {body}"))
}

pub fn set_liked(access_token: &str, track_uri: &str, liked: bool) -> Result<(), String> {
    let client = reqwest::blocking::Client::new();
    let request = if liked {
        client.put(format!("{API}/me/library"))
    } else {
        client.delete(format!("{API}/me/library"))
    };
    // cuerpo vacío explícito: sin `Content-Length: 0` Spotify contesta 411.
    let response = request
        .bearer_auth(access_token)
        .query(&[("uris", track_uri)])
        .body("")
        .send()
        .map_err(|e| e.to_string())?;
    check(response)
}

pub fn add_to_playlist(access_token: &str, playlist_uri: &str, track_uri: &str) -> Result<(), String> {
    let playlist_id = playlist_uri
        .strip_prefix("spotify:playlist:")
        .ok_or_else(|| "uri de playlist inválida".to_string())?;
    let response = reqwest::blocking::Client::new()
        .post(format!("{API}/playlists/{playlist_id}/items"))
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "uris": [track_uri] }))
        .send()
        .map_err(|e| e.to_string())?;
    check(response)
}
