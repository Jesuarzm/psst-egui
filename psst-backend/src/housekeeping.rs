use std::{
    env, fs,
    path::Path,
    thread,
    time::SystemTime,
};

fn trim_audio_cache(dir: &Path, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, u64, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let used = meta.accessed().ok()?.max(meta.modified().ok()?);
            meta.is_file().then(|| (used, meta.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max_bytes {
        return;
    }
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

fn trim_covers(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut covers: Vec<(SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("mpris-cover-"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if covers.len() <= keep {
        return;
    }
    covers.sort_by_key(|c| std::cmp::Reverse(c.0));
    for (_, path) in covers.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

pub fn spawn_cache_cleanup(max_audio_bytes: u64, keep_covers: usize) {
    let _ = thread::Builder::new().name("cache-cleanup".into()).spawn(move || {
        let root = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
        trim_audio_cache(&Path::new(&root).join("audio/audio"), max_audio_bytes);
        trim_covers(Path::new(&root), keep_covers);
    });
}
