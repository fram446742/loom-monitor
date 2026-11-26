#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use niri_ipc::{
    Event, MaxBpc, Output, OutputAction, OutputConfigChanged, Request, Response, VrrToSet,
};

#[derive(Debug, Clone)]
pub struct Recorded {
    pub output: String,
    pub action: OutputAction,
}

#[derive(Debug)]
pub struct FakeState {
    pub outputs: BTreeMap<String, Output>,
    pub recorded: Vec<Recorded>,
    pub fail_on_attempt: Option<usize>,
    pub attempts: usize,
}

pub struct FakeNiri {
    pub path: PathBuf,
    pub state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    dir: PathBuf,
}

impl FakeNiri {
    pub fn start(name: &str, outputs: Vec<Output>) -> Self {
        let dir = unique_dir("fake-niri", name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("niri.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();

        let state = Arc::new(Mutex::new(FakeState {
            outputs: outputs
                .into_iter()
                .map(|output| (output.name.clone(), output))
                .collect(),
            recorded: Vec::new(),
            fail_on_attempt: None,
            attempts: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name(format!("fake-niri-{name}"))
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let state = Arc::clone(&thread_state);
                            thread::spawn(move || serve(stream, state));
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
            .unwrap();

        Self {
            path,
            state,
            stop,
            handle: Some(handle),
            dir,
        }
    }

    pub fn fail_on_attempt(&self, attempt: usize) {
        self.state.lock().unwrap().fail_on_attempt = Some(attempt);
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().recorded.clone()
    }

    pub fn action_labels(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .map(|record| loom_monitor::action_description(&record.action))
            .collect()
    }

    pub fn clear_recorded(&self) {
        self.state.lock().unwrap().recorded.clear();
    }

    pub fn output(&self, name: &str) -> Output {
        self.state
            .lock()
            .unwrap()
            .outputs
            .get(name)
            .cloned()
            .unwrap()
    }

    pub fn set_outputs(&self, outputs: Vec<Output>) {
        self.state.lock().unwrap().outputs = outputs
            .into_iter()
            .map(|output| (output.name.clone(), output))
            .collect();
    }
}

impl Drop for FakeNiri {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn serve(stream: UnixStream, state: Arc<Mutex<FakeState>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(err) => {
                write_reply(&mut writer, Err(format!("bad request: {err}")));
                break;
            }
        };
        let event_stream = matches!(request, Request::EventStream);
        let reply = handle_request(request, &state);
        write_reply(&mut writer, reply);
        if event_stream {
            let event = Event::ConfigLoaded { failed: false };
            let _ = writeln!(writer, "{}", serde_json::to_string(&event).unwrap());
            let _ = writer.flush();
            // Keep the event socket open until the client closes it.
            loop {
                let mut buffer = String::new();
                match reader.read_line(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            break;
        }
    }
}

fn write_reply(stream: &mut UnixStream, reply: Result<Response, String>) {
    let _ = writeln!(stream, "{}", serde_json::to_string(&reply).unwrap());
    let _ = stream.flush();
}

fn handle_request(request: Request, state: &Arc<Mutex<FakeState>>) -> Result<Response, String> {
    match request {
        Request::Outputs => {
            let state = state.lock().unwrap();
            Ok(Response::Outputs(
                state.outputs.clone().into_iter().collect(),
            ))
        }
        Request::Output { output, action } => {
            let mut state = state.lock().unwrap();
            let attempt = state.attempts;
            state.attempts += 1;
            state.recorded.push(Recorded {
                output: output.clone(),
                action: action.clone(),
            });
            if state.fail_on_attempt == Some(attempt) {
                return Err("injected output failure".to_owned());
            }
            if let Some(target) = state.outputs.get_mut(&output) {
                apply_action(target, &action);
            }
            Ok(Response::OutputConfigChanged(OutputConfigChanged::Applied))
        }
        Request::EventStream => Ok(Response::Handled),
        _ => Ok(Response::Handled),
    }
}

fn apply_action(output: &mut Output, action: &OutputAction) {
    match action {
        OutputAction::Off => {
            output.logical = None;
            output.current_mode = None;
        }
        OutputAction::On => {
            if output.logical.is_none() {
                output.logical = Some(niri_ipc::LogicalOutput {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    scale: 1.0,
                    transform: niri_ipc::Transform::Normal,
                });
                output.current_mode = Some(0);
            }
        }
        OutputAction::Mode { mode } => {
            if let niri_ipc::ModeToSet::Specific(mode) = mode
                && let Some(index) = output.modes.iter().position(|candidate| {
                    candidate.width == mode.width && candidate.height == mode.height
                })
            {
                output.current_mode = Some(index);
            }
        }
        OutputAction::Scale {
            scale: niri_ipc::ScaleToSet::Specific(scale),
        } => {
            if let Some(logical) = &mut output.logical {
                logical.scale = *scale;
            }
        }
        OutputAction::Transform { transform } => {
            if let Some(logical) = &mut output.logical {
                logical.transform = *transform;
            }
        }
        OutputAction::Position {
            position: niri_ipc::PositionToSet::Specific(position),
        } => {
            if let Some(logical) = &mut output.logical {
                logical.x = position.x;
                logical.y = position.y;
            }
        }
        OutputAction::Vrr {
            vrr: VrrToSet { vrr, .. },
        } => output.vrr_enabled = *vrr,
        OutputAction::MaxBpc { max_bpc } => output.max_bpc = Some(max_bpc_to_u8(*max_bpc)),
        _ => {}
    }
}

fn max_bpc_to_u8(value: MaxBpc) -> u8 {
    match value {
        MaxBpc::_6 => 6,
        MaxBpc::_8 => 8,
        MaxBpc::_10 => 10,
        MaxBpc::_12 => 12,
        MaxBpc::_14 => 14,
        MaxBpc::_16 => 16,
    }
}

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

pub fn unique_dir(prefix: &str, name: &str) -> PathBuf {
    let id = UNIQUE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "loom-monitor-it-{}-{id}-{prefix}-{name}",
        std::process::id()
    ))
}

pub struct TempState {
    pub dir: PathBuf,
    pub profiles: PathBuf,
    pub rules: PathBuf,
    pub lock: PathBuf,
}

impl TempState {
    pub fn new(name: &str) -> Self {
        let dir = unique_dir("state", name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            profiles: dir.join("profiles.json"),
            rules: dir.join("rules.json"),
            lock: dir.join("watch.lock"),
            dir,
        }
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn output_at(
    name: &str,
    make: &str,
    model: &str,
    serial: Option<&str>,
    x: i32,
    scale: f64,
) -> Output {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "make": make,
        "model": model,
        "serial": serial,
        "physical_size": null,
        "modes": [
            { "width": 1920, "height": 1080, "refresh_rate": 60000, "is_preferred": true },
            { "width": 2560, "height": 1440, "refresh_rate": 144000, "is_preferred": false }
        ],
        "current_mode": 0,
        "is_custom_mode": false,
        "vrr_supported": true,
        "vrr_enabled": true,
        "logical": {
            "x": x,
            "y": 0,
            "width": 1920,
            "height": 1080,
            "scale": scale,
            "transform": "Normal"
        },
        "max_bpc": null
    }))
    .unwrap()
}
