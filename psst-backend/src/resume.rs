use std::env;

use psst_core::player::queue::QueueBehavior;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct ResumeState {
    pub playlist_uri: String,
    pub track_uri: String,
    pub position_ms: u64,
    pub queue_behavior: String,
}

fn resume_path() -> String {
    let dir = format!("{}/.cache/psst-tui", env::var("HOME").unwrap_or_default());
    let _ = std::fs::create_dir_all(&dir);
    format!("{dir}/resume.json")
}

pub fn load() -> Option<ResumeState> {
    let raw = std::fs::read_to_string(resume_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(state: &ResumeState) {
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::write(resume_path(), json);
    }
}

pub fn queue_behavior_to_str(b: QueueBehavior) -> &'static str {
    match b {
        QueueBehavior::Sequential => "Sequential",
        QueueBehavior::Random => "Random",
        QueueBehavior::LoopAll => "LoopAll",
        QueueBehavior::LoopTrack => "LoopTrack",
    }
}

pub fn queue_behavior_from_str(s: &str) -> QueueBehavior {
    match s {
        "Random" => QueueBehavior::Random,
        "LoopAll" => QueueBehavior::LoopAll,
        "LoopTrack" => QueueBehavior::LoopTrack,
        _ => QueueBehavior::Sequential,
    }
}
