use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct State {
    stream: Option<(u32, f64)>,
    /// tras un cambio local se ignoran las lecturas hasta acá: si no, una vieja pisa el slider al arrastrar.
    hold_until: Option<Instant>,
}

pub type Notifier = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct SystemVolume {
    state: Arc<Mutex<State>>,
    notifier: Arc<Mutex<Option<Notifier>>>,
}

impl SystemVolume {
    pub fn spawn(app_name: &'static str) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let state_for_worker = state.clone();
        let notifier: Arc<Mutex<Option<Notifier>>> = Arc::default();
        let notifier_for_worker = notifier.clone();
        let (tx, rx) = mpsc::channel::<()>();
        let _ = thread::Builder::new().name("pactl-subscribe".into()).spawn(move || loop {
            if let Ok(mut child) = Command::new("pactl")
                .arg("subscribe")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                if let Some(out) = child.stdout.take() {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        if line.contains("sink-input") {
                            let _ = tx.send(());
                        }
                    }
                }
                let _ = child.wait();
            }
            thread::sleep(Duration::from_secs(5));
        });
        let _ = thread::Builder::new().name("pactl-volume".into()).spawn(move || loop {
            if let Some(found) = read_stream(app_name) {
                let mut s = state_for_worker.lock().unwrap();
                let before = s.stream;
                if s.hold_until.map_or(true, |t| Instant::now() >= t) {
                    s.stream = Some(found);
                } else if let Some((_, held)) = s.stream {
                    s.stream = Some((found.0, held));
                }
                let changed = s.stream != before;
                drop(s);
                if changed {
                    if let Some(notify) = notifier_for_worker.lock().unwrap().as_ref() {
                        notify();
                    }
                }
            }
            if rx.recv().is_err() {
                return;
            }
            while rx.try_recv().is_ok() {}
            thread::sleep(Duration::from_millis(50));
        });
        Self { state, notifier }
    }

    pub fn set_notifier(&self, notify: Notifier) {
        *self.notifier.lock().unwrap() = Some(notify);
    }

    pub fn get(&self) -> Option<f64> {
        self.state.lock().unwrap().stream.map(|(_, v)| v)
    }

    pub fn set(&self, volume: f64) -> bool {
        let volume = volume.clamp(0.0, 1.0);
        let mut s = self.state.lock().unwrap();
        let Some((idx, _)) = s.stream else {
            return false;
        };
        s.stream = Some((idx, volume));
        s.hold_until = Some(Instant::now() + Duration::from_millis(1200));
        drop(s);
        let percent = format!("{}%", (volume * 100.0).round() as u32);
        let _ = thread::Builder::new().name("pactl-set".into()).spawn(move || {
            let _ = Command::new("pactl")
                .args(["set-sink-input-volume", &idx.to_string(), &percent])
                .status();
        });
        true
    }
}

fn read_stream(app_name: &str) -> Option<(u32, f64)> {
    let out = Command::new("pactl").args(["-f", "json", "list", "sink-inputs"]).output().ok()?;
    let inputs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    let input = inputs.iter().find(|i| {
        let props = &i["properties"];
        ["application.name", "node.name"]
            .iter()
            .any(|k| props[k].as_str().is_some_and(|v| v.contains(app_name)))
    })?;
    let index = input["index"].as_u64()? as u32;
    let values: Vec<f64> = input["volume"]
        .as_object()?
        .values()
        .filter_map(|c| c["value"].as_f64())
        .collect();
    if values.is_empty() {
        return None;
    }
    let avg = values.iter().sum::<f64>() / values.len() as f64;
    Some((index, (avg / 65536.0).clamp(0.0, 1.0)))
}
