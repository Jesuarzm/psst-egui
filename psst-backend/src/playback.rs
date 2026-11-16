//! El token OAuth crudo no sirve para Login5: se reusan las credenciales guardadas de psst-gui.
//! El Player solo procesa comandos si su loop de eventos corre (`player.handle` en un hilo propio).

use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crossbeam_channel::Sender;
use librespot_core::FileId;
use psst_core::{
    audio::output::DefaultAudioOutput,
    cache::Cache,
    cdn::Cdn,
    item_id::{ItemId, ItemIdType},
    player::{item::PlaybackItem, queue::QueueBehavior, PlaybackConfig, Player, PlayerCommand, PlayerEvent},
    session::{SessionConfig, SessionService},
};

#[derive(Clone, Default)]
pub struct QueueTrack {
    pub uri: String,
    pub title: String,
    pub artists: Vec<String>,
    pub artist_uris: Vec<String>,
    pub album: String,
    pub year: Option<i32>,
    pub duration: Duration,
    pub cover: Option<FileId>,
    pub cover_large: Option<FileId>,
}

#[derive(Clone)]
pub struct Status {
    pub playing: bool,
    pub position: Duration,
    pub duration: Duration,
    pub volume: f64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub artists: Vec<String>,
    pub artist_uris: Vec<String>,
    pub album: Option<String>,
    pub year: Option<i32>,
    pub cover: Option<FileId>,
    pub cover_large: Option<FileId>,
    pub track_uri: Option<String>,
    pub queue_behavior: QueueBehavior,
    pub meta_rev: u64,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            playing: false,
            position: Duration::ZERO,
            duration: Duration::ZERO,
            volume: 1.0,
            title: None,
            artist: None,
            artists: Vec::new(),
            artist_uris: Vec::new(),
            album: None,
            year: None,
            cover: None,
            cover_large: None,
            track_uri: None,
            queue_behavior: QueueBehavior::Sequential,
            meta_rev: 0,
        }
    }
}

#[derive(Clone)]
pub struct Playback {
    sender: Sender<PlayerEvent>,
    pub status: Arc<Mutex<Status>>,
    queue_meta: Arc<Mutex<HashMap<ItemId, QueueTrack>>>,
    upcoming: Arc<Mutex<Vec<ItemId>>>,
    notifier: Arc<Mutex<Option<crate::sysvolume::Notifier>>>,
    changed: Arc<tokio::sync::Notify>,
    track_tx: Arc<tokio::sync::watch::Sender<Option<String>>>,
}

const UPCOMING_LIMIT: usize = 50;

impl Playback {
    pub fn connect() -> Self {
        let login_creds = crate::login::load_credentials()
            .expect("no hay credenciales guardadas — falta iniciar sesión");
        let session = SessionService::with_config(SessionConfig {
            login_creds,
            proxy_url: None,
        });
        session.connected().expect("no se pudo conectar la sesion de audio");

        let cdn = Cdn::new(session.clone(), None).expect("cdn");
        let cache_dir = format!("{}/.cache/psst-tui/audio", env::var("HOME").unwrap());
        std::fs::create_dir_all(&cache_dir).expect("no se pudo crear el dir de cache");
        let cache = Cache::new(PathBuf::from(cache_dir)).expect("cache");
        let output = DefaultAudioOutput::open().expect("no se pudo abrir el dispositivo de audio");
        let player = Player::new(session, cdn, cache, PlaybackConfig::default(), &output);
        let sender = player.sender();

        let status = Arc::new(Mutex::new(Status::default()));
        let status_writer = status.clone();
        let queue_meta = Arc::new(Mutex::new(HashMap::new()));
        let queue_meta_reader = queue_meta.clone();
        let upcoming = Arc::new(Mutex::new(Vec::new()));
        let upcoming_writer = upcoming.clone();
        let notifier: Arc<Mutex<Option<crate::sysvolume::Notifier>>> = Arc::default();
        let notifier_reader = notifier.clone();
        let changed = Arc::new(tokio::sync::Notify::new());
        let changed_writer = changed.clone();
        let track_tx = Arc::new(tokio::sync::watch::channel(None).0);
        let track_writer = track_tx.clone();

        let _ = thread::Builder::new().name("player-events".into()).spawn(move || {
            let _output = output;
            let mut player = player;
            for event in player.receiver() {
                // las Position mientras suena no avisan (la UI ya repinta sola); el resto sí.
                let is_position = matches!(event, PlayerEvent::Position { .. });
                update_status(&status_writer, &queue_meta_reader, &event);
                // en pausa la UI no repinta sola: una posición nueva (seek desde MPRIS) sí avisa
                let notify = !is_position || !status_writer.lock().unwrap().playing;
                player.handle(event);
                *upcoming_writer.lock().unwrap() =
                    player.upcoming(UPCOMING_LIMIT).iter().map(|i| i.item_id).collect();
                if notify {
                    if let Some(f) = notifier_reader.lock().unwrap().as_ref() {
                        f();
                    }
                    changed_writer.notify_one();
                    let track = status_writer.lock().unwrap().track_uri.clone();
                    track_writer.send_if_modified(|current| {
                        let modified = *current != track;
                        if modified {
                            *current = track;
                        }
                        modified
                    });
                }
            }
        });

        Self {
            sender,
            status,
            queue_meta,
            upcoming,
            notifier,
            changed,
            track_tx,
        }
    }

    pub fn changed(&self) -> Arc<tokio::sync::Notify> {
        self.changed.clone()
    }

    pub fn watch_track(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.track_tx.subscribe()
    }

    pub fn set_notifier(&self, notify: crate::sysvolume::Notifier) {
        *self.notifier.lock().unwrap() = Some(notify);
    }

    fn notify(&self) {
        if let Some(f) = self.notifier.lock().unwrap().as_ref() {
            f();
        }
        self.changed.notify_one();
    }

    pub fn play_playlist(&self, tracks: &[QueueTrack]) -> usize {
        let start = self.pick_start_index(tracks.len());
        self.load_queue(tracks, start);
        start
    }

    pub fn pick_start_index(&self, len: usize) -> usize {
        let random = self.status.lock().unwrap().queue_behavior == QueueBehavior::Random;
        if random && len > 0 {
            rand::random_range(0..len)
        } else {
            0
        }
    }

    pub fn load_queue(&self, tracks: &[QueueTrack], position: usize) {
        let mut items = Vec::with_capacity(tracks.len());
        let mut meta_by_id = HashMap::with_capacity(tracks.len());
        for track in tracks {
            if let Some(item) = to_item(&track.uri) {
                meta_by_id.insert(item.item_id, track.clone());
                items.push(item);
            }
        }
        if items.is_empty() {
            return;
        }
        *self.queue_meta.lock().unwrap() = meta_by_id;
        let position = position.min(items.len() - 1);
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::LoadQueue {
            items,
            position,
        }));
    }

    pub fn pause_or_resume(&self) {
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::PauseOrResume));
    }

    pub fn play(&self) {
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::Resume));
    }

    pub fn pause(&self) {
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::Pause));
    }

    pub fn seek(&self, position: Duration) {
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::Seek { position }));
    }

    /// Adelanta `Status.position` para que la barra no salte atrás; `seek` no lo hace a propósito (la reanudación espera la posición real).
    pub fn seek_to(&self, position: Duration) {
        let target = {
            let mut s = self.status.lock().unwrap();
            if s.title.is_none() {
                return;
            }
            let target = position.min(s.duration);
            s.position = target;
            target
        };
        self.seek(target);
    }

    pub fn seek_relative(&self, delta_secs: f64) {
        let pos = self.status.lock().unwrap().position.as_secs_f64();
        self.seek_to(Duration::from_secs_f64((pos + delta_secs).max(0.0)));
    }

    pub fn next(&self) {
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::Next));
    }

    pub fn previous(&self) {
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::Previous));
    }

    pub fn upcoming(&self) -> Vec<(usize, QueueTrack)> {
        let ids = self.upcoming.lock().unwrap().clone();
        let meta = self.queue_meta.lock().unwrap();
        ids.iter()
            .enumerate()
            .filter_map(|(i, id)| meta.get(id).map(|t| (i + 1, t.clone())))
            .collect()
    }

    pub fn skip_ahead(&self, count: usize) {
        let _ = self
            .sender
            .send(PlayerEvent::Command(PlayerCommand::SkipAhead { count }));
    }

    pub fn stop(&self) {
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::Stop));
    }

    pub fn set_volume(&self, volume: f64) {
        let volume = volume.clamp(0.0, 1.0);
        self.status.lock().unwrap().volume = volume;
        let _ = self.sender.send(PlayerEvent::Command(PlayerCommand::SetVolume { volume }));
    }

    pub fn cycle_queue_behavior(&self) {
        let next = match self.status.lock().unwrap().queue_behavior {
            QueueBehavior::Sequential => QueueBehavior::Random,
            QueueBehavior::Random => QueueBehavior::LoopAll,
            QueueBehavior::LoopAll => QueueBehavior::LoopTrack,
            QueueBehavior::LoopTrack => QueueBehavior::Sequential,
        };
        self.set_queue_behavior(next);
    }

    pub fn update_track_artists(&self, uri: &str, artists: Vec<String>, artist_uris: Vec<String>) {
        for meta in self.queue_meta.lock().unwrap().values_mut() {
            if meta.uri == uri {
                meta.artists = artists.clone();
                meta.artist_uris = artist_uris.clone();
            }
        }
        let mut s = self.status.lock().unwrap();
        if s.track_uri.as_deref() == Some(uri) {
            s.artist = Some(crate::library::join_artists(&artists));
            s.artists = artists;
            s.artist_uris = artist_uris;
            s.meta_rev += 1;
            drop(s);
            self.notify();
        }
    }

    pub fn set_queue_behavior(&self, behavior: QueueBehavior) {
        self.status.lock().unwrap().queue_behavior = behavior;
        let _ = self
            .sender
            .send(PlayerEvent::Command(PlayerCommand::SetQueueBehavior { behavior }));
    }
}

fn to_item(track_uri: &str) -> Option<PlaybackItem> {
    let id62 = track_uri.strip_prefix("spotify:track:")?;
    let item_id = ItemId::from_base62(id62, ItemIdType::Track)?;
    Some(PlaybackItem {
        item_id,
        norm_level: psst_core::audio::normalize::NormalizationLevel::Track,
    })
}

fn update_status(
    status: &Arc<Mutex<Status>>,
    queue_meta: &Arc<Mutex<HashMap<ItemId, QueueTrack>>>,
    event: &PlayerEvent,
) {
    let mut s = status.lock().unwrap();
    match event {
        PlayerEvent::Playing { path, position } | PlayerEvent::Resuming { path, position } => {
            s.playing = true;
            s.position = *position;
            s.duration = path.duration;
            if let Some(meta) = queue_meta.lock().unwrap().get(&path.item_id) {
                s.title = Some(meta.title.clone());
                s.artist = (!meta.artists.is_empty()).then(|| crate::library::join_artists(&meta.artists));
                s.artists = meta.artists.clone();
                s.artist_uris = meta.artist_uris.clone();
                s.album = (!meta.album.is_empty()).then(|| meta.album.clone());
                s.year = meta.year;
                s.cover = meta.cover;
                s.cover_large = meta.cover_large;
                s.track_uri = Some(meta.uri.clone());
            }
        }
        PlayerEvent::Position { path, position } => {
            s.position = *position;
            s.duration = path.duration;
        }
        PlayerEvent::Pausing { path, position } => {
            s.playing = false;
            s.position = *position;
            s.duration = path.duration;
        }
        PlayerEvent::EndOfTrack => {
            // no se borran title/track_uri: la siguiente pista llega enseguida y así no parpadea.
            s.playing = false;
            s.position = Duration::ZERO;
        }
        PlayerEvent::Stopped => {
            let volume = s.volume;
            let queue_behavior = s.queue_behavior;
            let meta_rev = s.meta_rev;
            *s = Status {
                volume,
                queue_behavior,
                meta_rev,
                ..Status::default()
            };
        }
        _ => {}
    }
}
