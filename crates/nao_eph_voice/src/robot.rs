//! The agent's tools as HULKs motion commands, and a minimal client for HULKs' debug server that
//! injects them through `behavior.injected_motion_command`.

use std::{
    io::ErrorKind,
    net::TcpStream,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    thread,
    time::Duration,
};

use color_eyre::{eyre::eyre, Result};
use serde_json::{json, Value};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

/// Measured in Webots with HULKs' normal walk: about 0.13 m/s forward, 0.45 rad/s on the spot.
const FORWARD_SPEED: f32 = 0.13;
const TURN_RATE: f32 = 0.45;
const MAX_WALK: f32 = 1.0;

/// A motion command and how long to keep it before the next step.
pub struct Step {
    pub command: Value,
    pub hold: Duration,
}

pub fn stand() -> Value {
    json!({"Stand": {"head": "Center"}})
}

fn walk_towards(to: [f32; 2]) -> Value {
    // Paths are in the robot's ground frame and re-read every cycle, so a goal ahead keeps it
    // walking until the next command.
    json!({"Walk": {
        "head": "Center",
        "path": {"segments": [{"LineSegment": [[0.0, 0.0], to]}]},
        "left_arm": "Swing",
        "right_arm": "Swing",
        "orientation_mode": "AlignWithPath",
        "speed": "Normal",
    }})
}

fn hold(command: Value, seconds: f32) -> Step {
    Step { command, hold: Duration::from_secs_f32(seconds.max(0.0)) }
}

/// Translates a tool call from the agent into motion steps, or an error message for the agent.
pub fn plan(tool: &str, parameters: &Value) -> Result<Vec<Step>, String> {
    let number = |name: &str| {
        parameters[name]
            .as_f64()
            .map(|v| v as f32)
            .ok_or_else(|| format!("missing number parameter '{name}'"))
    };
    Ok(match tool {
        "stand" | "stand_up" => vec![hold(stand(), 0.0)],
        "sit_down" => vec![hold(json!({"SitDown": {"head": "Center"}}), 0.0)],
        "arms_up" => vec![hold(json!({"ArmsUpStand": {"head": "Center"}}), 3.0), hold(stand(), 0.0)],
        "squat" => vec![hold(json!("ArmsUpSquat"), 3.0), hold(stand(), 0.0)],
        "walk" => {
            let meters = number("meters")?.clamp(0.0, MAX_WALK);
            vec![hold(walk_towards([1.0, 0.0]), meters / FORWARD_SPEED), hold(stand(), 0.0)]
        }
        "turn" => {
            let degrees = number("degrees")?.clamp(-360.0, 360.0);
            let side = degrees.signum() * 0.5;
            let seconds = degrees.abs().to_radians() / TURN_RATE;
            vec![hold(walk_towards([0.02, side]), seconds), hold(stand(), 0.0)]
        }
        "look" => {
            let target = match parameters["direction"].as_str() {
                Some("left") => Some([1.0, 1.2]),
                Some("right") => Some([1.0, -1.2]),
                Some("ahead") | None => None,
                Some(other) => return Err(format!("unknown direction '{other}'")),
            };
            let head = match target {
                Some(target) => json!({"LookAt": {"target": target, "image_region_target": "Center", "camera": null}}),
                None => json!("Center"),
            };
            vec![hold(json!({"Stand": {"head": head}}), 0.0)]
        }
        other => return Err(format!("unknown tool '{other}'")),
    })
}

/// Runs motion plans one at a time; a new plan cancels the rest of the current one.
pub struct Executor {
    generation: Arc<AtomicU64>,
    sink: Arc<dyn Fn(Value) + Send + Sync>,
}

impl Executor {
    pub fn new(sink: impl Fn(Value) + Send + Sync + 'static) -> Self {
        Self { generation: Arc::new(AtomicU64::new(0)), sink: Arc::new(sink) }
    }

    pub fn run(&self, steps: Vec<Step>) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let current = self.generation.clone();
        let sink = self.sink.clone();
        thread::spawn(move || {
            for step in steps {
                if current.load(Ordering::SeqCst) != generation {
                    return;
                }
                sink(step.command);
                thread::sleep(step.hold);
            }
        });
    }
}

/// Connects to HULKs' debug server (port 1337) and keeps writing the latest motion command.
/// With `force_playing` (simulation, no chest button) the robot is put into the active state.
pub fn hulk_sink(address: String, force_playing: bool) -> impl Fn(Value) + Send + Sync + 'static {
    let (sender, receiver) = channel::<Value>();
    thread::spawn(move || {
        let mut last = stand();
        loop {
            if let Err(error) = serve(&address, force_playing, &receiver, &mut last) {
                eprintln!("nao-eph-voice: HULKs connection: {error}");
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
    let sender: std::sync::Mutex<Sender<Value>> = std::sync::Mutex::new(sender);
    move |command| {
        let _ = sender.lock().unwrap().send(command);
    }
}

fn serve(address: &str, force_playing: bool, commands: &Receiver<Value>, last: &mut Value) -> Result<()> {
    let (mut socket, _) = tungstenite::connect(address)?;
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
    }
    let mut id = 0;
    let mut write = |socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, path: &str, value: Value| {
        id += 1;
        let request = json!({"id": id, "kind": {"Write": {"path": path, "value": {"Text": value}}}});
        socket.send(Message::text(request.to_string()))
    };
    if force_playing {
        write(&mut socket, "parameters.primary_state_filter.injected_primary_state", json!("Playing"))?;
    }
    write(&mut socket, "parameters.behavior.injected_motion_command", last.clone())?;
    loop {
        while let Ok(command) = commands.try_recv() {
            write(&mut socket, "parameters.behavior.injected_motion_command", command.clone())?;
            *last = command;
        }
        match socket.read() {
            Ok(Message::Close(_)) => return Err(eyre!("HULKs closed the connection")),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
    }
}
