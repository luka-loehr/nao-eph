//! Client for the HULKs robot software (hulk_webots) through its debug server, the same one twix
//! uses. Actions are motion commands injected via `behavior.injected_motion_command`.

use serde_json::{Value, json};
use std::{
    io::ErrorKind,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::Duration,
};
use tungstenite::{Message as WsMessage, stream::MaybeTlsStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Stand,
    LookAround,
    WalkForward,
    WalkCircle,
    TurnLeft,
    ArmsUp,
    Squat,
    SitDown,
    /// nao-eph's Life mode drives the robot with `Message::Motion`.
    Life,
    /// Talking to the voice agent, whose tools also arrive as `Message::Motion`.
    Talk,
}

/// What the app sends to the HULKs client thread.
pub enum Message {
    Action(Action),
    /// A raw `MotionCommand` from a behavior running in the app (Life mode or the voice agent).
    Motion(Value),
    /// Getting up after a fall, sent in every mode because injected commands override HULKs'
    /// own fall recovery.
    StandUp(Value),
}

impl Action {
    pub const MOTIONS: [Action; 8] = [
        Action::Stand,
        Action::LookAround,
        Action::WalkForward,
        Action::WalkCircle,
        Action::TurnLeft,
        Action::ArmsUp,
        Action::Squat,
        Action::SitDown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Stand => "Stand",
            Action::LookAround => "Look around",
            Action::WalkForward => "Walk forward",
            Action::WalkCircle => "Walk in a circle",
            Action::TurnLeft => "Turn on the spot",
            Action::ArmsUp => "Arms up",
            Action::Squat => "Squat",
            Action::SitDown => "Sit down",
            Action::Life => "Life mode",
            Action::Talk => "Talk",
        }
    }

    /// The `MotionCommand` to inject, in HULKs' serde format. Paths are in the robot's ground
    /// frame and re-read every cycle, so a goal ahead of the robot keeps it walking.
    fn motion_command(self) -> Value {
        let walk = |to: [f32; 2], head: &str| {
            json!({"Walk": {
                "head": head,
                "path": {"segments": [{"LineSegment": [[0.0, 0.0], to]}]},
                "left_arm": "Swing",
                "right_arm": "Swing",
                "orientation_mode": "AlignWithPath",
                "speed": "Normal",
            }})
        };
        match self {
            Action::Stand => json!({"Stand": {"head": "Center"}}),
            Action::LookAround => json!({"Stand": {"head": "LookAround"}}),
            Action::WalkForward => walk([1.0, 0.0], "Center"),
            // A goal always 45° to the left makes the robot keep turning while it walks.
            Action::WalkCircle => walk([0.5, 0.5], "Center"),
            Action::TurnLeft => walk([0.02, 0.5], "Center"),
            Action::ArmsUp => json!({"ArmsUpStand": {"head": "Center"}}),
            Action::Squat => json!("ArmsUpSquat"),
            Action::SitDown => json!({"SitDown": {"head": "Center"}}),
            // Life mode starts by standing; its own commands follow as `Message::Motion`.
            Action::Life | Action::Talk => json!({"Stand": {"head": "Center"}}),
        }
    }
}

pub struct HulkState {
    pub connected: bool,
    pub action: Action,
}

pub fn spawn(state: Arc<Mutex<HulkState>>) -> Sender<Message> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        loop {
            // Retries until hulk_webots is up; it only listens once Webots has loaded the world.
            let _ = run(&state, &rx);
            state.lock().unwrap().connected = false;
            thread::sleep(Duration::from_millis(500));
        }
    });
    tx
}

fn run(state: &Arc<Mutex<HulkState>>, messages: &Receiver<Message>) -> anyhow::Result<()> {
    let (mut socket, _) = tungstenite::connect("ws://localhost:1337")?;
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
    }
    let mut id = 0usize;
    let mut write = |socket: &mut tungstenite::WebSocket<_>, path: &str, value: Value| {
        id += 1;
        let request = json!({"id": id, "kind": {"Write": {"path": path, "value": {"Text": value}}}});
        socket.send(WsMessage::text(request.to_string()))
    };
    // No chest button in the simulation: put the robot straight into a stiff, active state.
    write(&mut socket, "parameters.primary_state_filter.injected_primary_state", json!("Playing"))?;
    let current = state.lock().unwrap().action;
    write(&mut socket, "parameters.behavior.injected_motion_command", current.motion_command())?;
    state.lock().unwrap().connected = true;
    loop {
        while let Ok(message) = messages.try_recv() {
            match message {
                Message::Action(action) => {
                    write(&mut socket, "parameters.behavior.injected_motion_command", action.motion_command())?;
                    state.lock().unwrap().action = action;
                }
                Message::Motion(command) => {
                    if matches!(state.lock().unwrap().action, Action::Life | Action::Talk) {
                        write(&mut socket, "parameters.behavior.injected_motion_command", command)?;
                    }
                }
                Message::StandUp(command) => {
                    write(&mut socket, "parameters.behavior.injected_motion_command", command)?;
                }
            }
        }
        // Drain replies; the connection only needs to stay alive.
        match socket.read() {
            Ok(WsMessage::Close(_)) => anyhow::bail!("HULKs closed the connection"),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
    }
}
