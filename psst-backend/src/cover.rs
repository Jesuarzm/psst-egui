use librespot_core::{session::Session, FileId};

use crate::library::PlaylistRef;

pub async fn fetch_cover_bytes(session: &Session, file_id: FileId) -> Result<Vec<u8>, String> {
    session
        .spclient()
        .get_image(&file_id)
        .await
        .map_err(|e| e.to_string())
        .map(|b| b.to_vec())
}

pub async fn fetch_cover_rgba(session: &Session, file_id: FileId) -> Result<(Vec<u8>, u32, u32), String> {
    let bytes = fetch_cover_bytes(session, file_id).await?;
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}

pub fn fetch_url_rgba(url: &str) -> Result<(Vec<u8>, u32, u32), String> {
    let bytes = reqwest::blocking::get(url)
        .map_err(|e| e.to_string())?
        .bytes()
        .map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}

fn playlist_cover_dir() -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(format!(
        "{}/.cache/psst-tui/playlist-covers",
        std::env::var("HOME").unwrap_or_default()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// FNV-1a y no `DefaultHasher`: el nombre de archivo tiene que ser estable entre versiones de Rust.
pub fn playlist_cover_key(p: &PlaylistRef) -> Option<String> {
    if let Some(url) = &p.cover_url {
        let hash = url.bytes().fold(0xcbf29ce484222325_u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        Some(format!("url-{hash:016x}"))
    } else {
        p.fallback_cover.map(|f| format!("file-{}", f.0.iter().map(|b| format!("{b:02x}")).collect::<String>()))
    }
}

pub fn cached_playlist_cover(p: &PlaylistRef) -> Option<(Vec<u8>, u32, u32)> {
    let bytes = std::fs::read(playlist_cover_dir().join(playlist_cover_key(p)?)).ok()?;
    let rgba = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

pub async fn store_playlist_covers(session: &Session, playlists: &[PlaylistRef]) {
    let dir = playlist_cover_dir();
    let mut keep = std::collections::HashSet::new();
    for p in playlists {
        let Some(key) = playlist_cover_key(p) else { continue };
        let path = dir.join(&key);
        keep.insert(key);
        if path.exists() {
            continue;
        }
        let bytes = if let Some(url) = &p.cover_url {
            match reqwest::get(url).await {
                Ok(resp) => resp.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        } else if let Some(file_id) = p.fallback_cover {
            fetch_cover_bytes(session, file_id).await
        } else {
            continue;
        };
        match bytes {
            Ok(bytes) => {
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, bytes).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
            Err(e) => eprintln!("playlists: no se pudo bajar la portada de {}: {e}", p.uri),
        }
    }
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        if !keep.contains(e.file_name().to_string_lossy().as_ref()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
