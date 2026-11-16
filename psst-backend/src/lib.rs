//! Los datos en disco (`~/.cache/psst-tui/`) y el bus MPRIS conservan el nombre "psst-tui" a propósito: cambiarlo pierde cachés y rompe `playerctl -p psst-tui`.

pub mod actions;
pub mod config;
pub mod cover;
pub mod housekeeping;
pub mod library;
pub mod login;
pub mod lyrics;
pub mod mpris;
pub mod playback;
pub mod resume;
pub mod romanize;
pub mod search;
pub mod settings;
pub mod sysvolume;
pub mod translate;
