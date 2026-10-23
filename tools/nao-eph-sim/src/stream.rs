//! Client for the Webots w3d streaming server (`webots --stream=w3d`).

use crate::scene::{Scene, axis_angle};
use glam::Vec3;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    io::ErrorKind,
    net::TcpStream,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::Duration,
};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Connecting,
    Loading,
    Connected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Paused,
    RealTime,
    Fast,
}

/// Everything the UI and renderer read. `revision` changes whenever the scene graph changes.
pub struct SimState {
    pub connection: Connection,
    pub run_state: RunState,
    pub time_ms: f64,
    pub world: String,
    pub scene: Option<Scene>,
    pub revision: u64,
    pub scene_generation: u64,
    pub console: VecDeque<String>,
}

impl Default for SimState {
    fn default() -> Self {
        Self {
            connection: Connection::Connecting,
            run_state: RunState::Paused,
            time_ms: 0.0,
            world: String::new(),
            scene: None,
            revision: 0,
            scene_generation: 0,
            console: VecDeque::new(),
        }
    }
}

impl SimState {
    pub fn log(&mut self, line: &str) {
        for l in line.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
            if self.console.len() >= 500 {
                self.console.pop_front();
            }
            self.console.push_back(l.to_string());
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    RealTime,
    Reset,
}

impl Command {
    fn wire(self) -> &'static str {
        match self {
            Command::RealTime => "real-time:-1",
            Command::Reset => "reset",
        }
    }
}

#[derive(Deserialize)]
struct UpdateBatch {
    time: Option<f64>,
    #[serde(default)]
    updates: Vec<Update>,
}

#[derive(Deserialize)]
struct Update {
    id: u64,
    translation: Option<String>,
    rotation: Option<String>,
}

pub fn spawn(url: String, state: Arc<Mutex<SimState>>) -> Sender<Command> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        loop {
            state.lock().unwrap().connection = Connection::Connecting;
            if let Err(err) = run(&url, &state, &rx) {
                state.lock().unwrap().log(&format!("[nao-eph-sim] {err:#}"));
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
    tx
}

fn run(url: &str, state: &Arc<Mutex<SimState>>, commands: &Receiver<Command>) -> anyhow::Result<()> {
    // Webots crashes if a client asks for the scene before the world has loaded. hulk_webots only
    // opens its debug port once the world is ready, so wait for that (with a fallback timeout).
    let started = std::time::Instant::now();
    while TcpStream::connect(("127.0.0.1", 1337)).is_err() && started.elapsed() < Duration::from_secs(180) {
        thread::sleep(Duration::from_millis(250));
    }
    let (mut socket, _) = tungstenite::connect(url)?;
    set_read_timeout(&mut socket);
    socket.send(Message::text("w3d"))?;
    state.lock().unwrap().connection = Connection::Loading;
    loop {
        while let Ok(command) = commands.try_recv() {
            socket.send(Message::text(command.wire()))?;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                handle(text.as_str(), state);
                // The simulation always runs live; there is no paused or recorded mode.
                if text.as_str() == "scene load completed" {
                    socket.send(Message::text(Command::RealTime.wire()))?;
                }
            }
            Ok(Message::Binary(bytes)) => handle(&String::from_utf8_lossy(&bytes), state),
            Ok(Message::Close(_)) => anyhow::bail!("Webots closed the stream"),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

fn set_read_timeout(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(5)));
        let _ = stream.set_nodelay(true);
    }
}

fn handle(message: &str, state: &Arc<Mutex<SimState>>) {
    if let Some(json) = message.strip_prefix("application/json:") {
        let Ok(batch) = serde_json::from_str::<UpdateBatch>(json) else {
            return;
        };
        let mut s = state.lock().unwrap();
        if let Some(t) = batch.time {
            s.time_ms = t;
        }
        if let Some(scene) = s.scene.as_mut() {
            for u in &batch.updates {
                let translation = u.translation.as_deref().and_then(|t| {
                    let v: Vec<f32> = t.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                    (v.len() >= 3).then(|| Vec3::new(v[0], v[1], v[2]))
                });
                scene.apply_update(u.id, translation, u.rotation.as_deref().and_then(axis_angle));
            }
            s.revision += 1;
        }
    } else if let Some(x3d) = message.strip_prefix("model:") {
        let parsed = Scene::parse(x3d);
        let mut s = state.lock().unwrap();
        match parsed {
            Ok(scene) => {
                s.scene = Some(scene);
                s.scene_generation += 1;
                s.revision += 1;
            }
            Err(e) => s.log(&format!("[nao-eph-sim] could not parse scene: {e:#}")),
        }
    } else if let Some(world) = message.strip_prefix("world:") {
        let name = world.split(':').next().unwrap_or(world);
        state.lock().unwrap().world = name.rsplit('/').next().unwrap_or(name).to_string();
    } else if let Some(time) = message.strip_prefix("time: ") {
        state.lock().unwrap().time_ms = time.trim().parse().unwrap_or(0.0);
    } else if message.starts_with("pause") || message == "paused by client" {
        state.lock().unwrap().run_state = RunState::Paused;
    } else if message == "real-time" {
        state.lock().unwrap().run_state = RunState::RealTime;
    } else if message == "run" || message == "fast" {
        state.lock().unwrap().run_state = RunState::Fast;
    } else if message == "scene load completed" {
        state.lock().unwrap().connection = Connection::Connected;
    } else if let Some(out) = message.strip_prefix("stdout:").or_else(|| message.strip_prefix("stderr:")) {
        let out = out.replace("\nstdout:", "\n").replace("\nstderr:", "\n");
        state.lock().unwrap().log(&out);
    }
}
