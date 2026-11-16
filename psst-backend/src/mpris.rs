//! `mpris_server::Player` no es Send: corre en su propio hilo con un runtime current_thread + LocalSet.

use std::{env, time::Duration};

use librespot_core::{session::Session, FileId};
use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Player, Time, TrackId};
use psst_core::player::queue::QueueBehavior;

use crate::{cover, playback::Playback};

fn queue_behavior_to_mpris(behavior: QueueBehavior) -> (bool, LoopStatus) {
    match behavior {
        QueueBehavior::Sequential => (false, LoopStatus::None),
        QueueBehavior::Random => (true, LoopStatus::None),
        QueueBehavior::LoopAll => (false, LoopStatus::Playlist),
        QueueBehavior::LoopTrack => (false, LoopStatus::Track),
    }
}

pub fn spawn(playback: Playback, session: Session) {
    let _ = std::thread::Builder::new().name("mpris".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("no se pudo crear el runtime de MPRIS");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, run(playback, session));
    });
}

async fn run(playback: Playback, session: Session) {
    // el bus se queda "psst-tui" a propósito: lo usa `playerctl -p psst-tui`.
    let player = match Player::builder("psst-tui")
        .identity("Psst")
        .can_play(true)
        .can_pause(true)
        .can_go_next(true)
        .can_go_previous(true)
        .can_seek(true)
        .can_control(true)
        .build()
        .await
    {
        Ok(player) => player,
        Err(err) => {
            eprintln!("mpris: no se pudo registrar el servicio D-Bus ({err}), sigo sin MPRIS");
            return;
        }
    };

    {
        let pb = playback.clone();
        player.connect_play(move |_| pb.play());
    }
    {
        let pb = playback.clone();
        player.connect_pause(move |_| pb.pause());
    }
    {
        let pb = playback.clone();
        player.connect_play_pause(move |_| pb.pause_or_resume());
    }
    {
        let pb = playback.clone();
        player.connect_stop(move |_| pb.stop());
    }
    {
        let pb = playback.clone();
        player.connect_next(move |_| pb.next());
    }
    {
        let pb = playback.clone();
        player.connect_previous(move |_| pb.previous());
    }
    {
        let pb = playback.clone();
        player.connect_set_volume(move |_, volume| pb.set_volume(volume));
    }
    {
        // Seek en MPRIS es un offset relativo a la posición actual.
        let pb = playback.clone();
        player.connect_seek(move |_, offset| {
            let st = pb.status.lock().unwrap().clone();
            let new_us = (st.position.as_micros() as i64 + offset.as_micros()).max(0) as u128;
            let clamped = new_us.min(st.duration.as_micros());
            pb.seek(Duration::from_micros(clamped as u64));
        });
    }
    {
        let pb = playback.clone();
        player.connect_set_position(move |_, _track_id, position| {
            let us = position.as_micros().max(0) as u64;
            pb.seek(Duration::from_micros(us));
        });
    }
    {
        // sin este callback la propiedad queda de solo lectura y el botón de aleatorio de noctalia no hace nada.
        let pb = playback.clone();
        player.connect_set_shuffle(move |_, shuffle| {
            let current = pb.status.lock().unwrap().queue_behavior;
            let next = if shuffle {
                QueueBehavior::Random
            } else if current == QueueBehavior::Random {
                QueueBehavior::Sequential
            } else {
                current
            };
            pb.set_queue_behavior(next);
        });
    }
    {
        let pb = playback.clone();
        player.connect_set_loop_status(move |_, loop_status| {
            let next = match loop_status {
                LoopStatus::None => QueueBehavior::Sequential,
                LoopStatus::Track => QueueBehavior::LoopTrack,
                LoopStatus::Playlist => QueueBehavior::LoopAll,
            };
            pb.set_queue_behavior(next);
        });
    }

    tokio::task::spawn_local(player.run());

    // un archivo por carátula: noctalia cachea por URL y con un nombre fijo la carátula quedaba pegada.
    let cover_cache_dir = {
        let dir = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
        let _ = std::fs::create_dir_all(&dir);
        dir
    };

    let mut last_status = PlaybackStatus::Stopped;
    let mut last_track_uri: Option<String> = None;
    let mut last_duration_us: i64 = -1;
    let mut last_meta_rev: u64 = 0;
    let mut last_cover: Option<FileId> = None;
    let mut cached_art_url: Option<String> = None;
    let mut last_volume = player.volume();
    let mut last_queue_behavior: Option<QueueBehavior> = None;
    let mut last_position_us: i64 = -1;

    let changed = playback.changed();
    loop {
        let snap = {
            let st = playback.status.lock().unwrap();
            let duration_us = st.duration.as_micros() as i64;
            let meta_changed =
                st.track_uri != last_track_uri || duration_us != last_duration_us || st.meta_rev != last_meta_rev;
            Snapshot {
                stopped: st.title.is_none(),
                playing: st.playing,
                position_us: st.position.as_micros() as i64,
                duration_us,
                volume: st.volume,
                queue_behavior: st.queue_behavior,
                art_cover: st.cover_large.or(st.cover),
                meta: meta_changed.then(|| MetaSnapshot {
                    title: st.title.clone(),
                    uri: st.track_uri.clone(),
                    artists: st.artists.clone(),
                    album: st.album.clone(),
                    meta_rev: st.meta_rev,
                }),
            }
        };

        let status = if snap.stopped {
            PlaybackStatus::Stopped
        } else if snap.playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        };
        if status != last_status {
            let _ = player.set_playback_status(status).await;
            last_status = status;
        }

        // aparte de la metadata: esa se publica dos veces por track (la duración llega tarde) y la carátula se bajaría dos veces.
        if snap.art_cover != last_cover {
            last_cover = snap.art_cover;
            cached_art_url = match snap.art_cover {
                Some(file_id) => {
                    let name = file_id.to_base16().unwrap_or_default();
                    let path = format!("{cover_cache_dir}/mpris-cover-{name}.jpg");
                    let path_exists = std::path::Path::new(&path).exists();
                    let ready = path_exists
                        || cover::fetch_cover_bytes(&session, file_id)
                            .await
                            .is_ok_and(|bytes| std::fs::write(&path, &bytes).is_ok());
                    ready.then(|| format!("file://{path}"))
                }
                None => None,
            };
        }

        if let Some(meta) = snap.meta {
            let mut metadata = Metadata::new();
            if let (Some(title), Some(uri)) = (&meta.title, &meta.uri) {
                metadata.set_title(Some(title.clone()));
                metadata.set_artist((!meta.artists.is_empty()).then(|| meta.artists.clone()));
                metadata.set_album(meta.album.clone());
                metadata.set_length(Some(Time::from_micros(snap.duration_us)));
                metadata.set_trackid(Some(track_id_for(uri)));
                metadata.set_art_url(cached_art_url.clone());
            }
            let _ = player.set_metadata(metadata).await;
            last_track_uri = meta.uri;
            last_duration_us = snap.duration_us;
            last_meta_rev = meta.meta_rev;
        }

        if snap.position_us != last_position_us {
            player.set_position(Time::from_micros(snap.position_us));
            last_position_us = snap.position_us;
        }

        if (last_volume - snap.volume).abs() > 0.001 {
            let _ = player.set_volume(snap.volume).await;
            last_volume = snap.volume;
        }

        if last_queue_behavior != Some(snap.queue_behavior) {
            last_queue_behavior = Some(snap.queue_behavior);
            let (shuffle, loop_status) = queue_behavior_to_mpris(snap.queue_behavior);
            let _ = player.set_shuffle(shuffle).await;
            let _ = player.set_loop_status(loop_status).await;
        }

        let interval = if snap.playing { Duration::from_millis(400) } else { Duration::from_secs(2) };
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = changed.notified() => {}
        }
    }
}

struct Snapshot {
    stopped: bool,
    playing: bool,
    position_us: i64,
    duration_us: i64,
    volume: f64,
    queue_behavior: QueueBehavior,
    art_cover: Option<FileId>,
    meta: Option<MetaSnapshot>,
}

struct MetaSnapshot {
    title: Option<String>,
    uri: Option<String>,
    artists: Vec<String>,
    album: Option<String>,
    meta_rev: u64,
}

fn track_id_for(uri: &str) -> TrackId {
    let id = uri.rsplit(':').next().unwrap_or("track");
    TrackId::try_from(format!("/org/psst_tui/track/{id}")).unwrap_or(TrackId::NO_TRACK)
}
