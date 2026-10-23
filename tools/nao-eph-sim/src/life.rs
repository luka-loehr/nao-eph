//! Life mode: the robot behaves like a NAO left alone in a room, in the spirit of NAOqi's
//! Autonomous Life. It watches the viewer, glances around, wanders to free spots and makes small
//! gestures. The app decides; HULKs executes the motions.

use crate::{
    hulk::{Action, HulkState, Message},
    stream::SimState,
};
use glam::{Mat4, Vec2, Vec3};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, mpsc::Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Open floor of the nao-eph studio, kept clear of the sofa, desk and walls.
const FREE_MIN: Vec2 = Vec2::new(-0.9, -1.4);
const FREE_MAX: Vec2 = Vec2::new(1.6, 1.6);
/// The stacked crates, which wandering goals must avoid.
const CRATES: Vec2 = Vec2::new(-0.6, -1.05);

enum Phase {
    Watch,
    Glance,
    Wander(Vec2),
    FaceViewer,
    Stretch,
    Squat,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545F4914F6CDD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }
}

/// Converts a world point into HULKs' ground frame: robot-centred, x forward, y left.
fn to_ground(robot: Mat4, world: Vec3) -> Vec2 {
    robot.inverse().transform_point3(world).truncate()
}

fn walk(to: Vec2, head: Value) -> Value {
    json!({"Walk": {
        "head": head,
        "path": {"segments": [{"LineSegment": [[0.0, 0.0], [to.x, to.y]]}]},
        "left_arm": "Swing",
        "right_arm": "Swing",
        "orientation_mode": "AlignWithPath",
        "speed": "Normal",
    }})
}

/// Looks far out along the viewer's direction so the gaze stays level instead of at the floor.
fn look_at(direction: Vec2) -> Value {
    let target = direction.normalize_or_zero() * 4.0;
    json!({"LookAt": {"target": [target.x, target.y], "image_region_target": "Center", "camera": "Top"}})
}

pub fn spawn(
    sim: Arc<Mutex<SimState>>,
    hulk: Arc<Mutex<HulkState>>,
    viewer: Arc<Mutex<Option<Vec3>>>,
    hulk_tx: Sender<Message>,
) {
    thread::spawn(move || {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        let mut rng = Rng(seed | 1);
        let mut phase = Phase::Watch;
        let mut phase_end = Instant::now() + Duration::from_secs(6);
        let mut last_sent = String::new();
        let mut fallen = false;
        let mut upright_since: Option<Instant> = None;
        loop {
            thread::sleep(Duration::from_millis(250));

            // Fall recovery in every mode: injected commands would otherwise keep the robot down.
            if let Some(robot) = robot_pose(&sim) {
                let up = robot.z_axis.truncate().normalize_or_zero().z;
                if up < 0.55 {
                    let kind = if robot.x_axis.z < 0.0 { "FacingDown" } else { "FacingUp" };
                    let command = json!({"StandUp": {"kind": kind, "speed": "Default"}});
                    if !fallen || last_sent != command.to_string() {
                        last_sent = command.to_string();
                        let _ = hulk_tx.send(Message::StandUp(command));
                    }
                    fallen = true;
                    upright_since = None;
                    continue;
                }
                if fallen {
                    // Back on its feet for a moment: resume whatever was selected.
                    let since = *upright_since.get_or_insert_with(Instant::now);
                    if up < 0.9 || since.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    fallen = false;
                    last_sent.clear();
                    let action = hulk.lock().unwrap().action;
                    let _ = hulk_tx.send(Message::Action(action));
                    phase = Phase::Watch;
                    phase_end = Instant::now() + Duration::from_secs(3);
                }
            }

            if hulk.lock().unwrap().action != Action::Life {
                phase = Phase::Watch;
                phase_end = Instant::now() + Duration::from_secs(4);
                last_sent.clear();
                continue;
            }
            let Some(robot) = robot_pose(&sim) else { continue };
            let position = robot.w_axis.truncate().truncate();
            let viewer = (*viewer.lock().unwrap()).unwrap_or(Vec3::new(3.0, 0.0, 1.0));
            let to_viewer = to_ground(robot, viewer);
            let now = Instant::now();

            let command = match phase {
                Phase::Watch => json!({"Stand": {"head": look_at(to_viewer)}}),
                Phase::Glance => json!({"Stand": {"head": "LookAround"}}),
                Phase::Wander(goal) => {
                    let local = to_ground(robot, goal.extend(0.0));
                    if local.length() < 0.15 {
                        phase_end = now;
                    }
                    walk(local, json!("Center"))
                }
                Phase::FaceViewer => {
                    // A short step toward the viewer: aligning with that path turns the robot.
                    if to_viewer.y.atan2(to_viewer.x).abs() < 0.25 {
                        phase_end = now;
                    }
                    walk(to_viewer.normalize_or_zero() * 0.12, look_at(to_viewer))
                }
                Phase::Stretch => json!({"ArmsUpStand": {"head": look_at(to_viewer)}}),
                Phase::Squat => json!("ArmsUpSquat"),
            };
            // Only send when the command changes noticeably; gaze targets are rounded for that.
            let key = round_numbers(&command).to_string();
            if key != last_sent {
                last_sent = key;
                let _ = hulk_tx.send(Message::Motion(command));
            }

            if now >= phase_end {
                let (next, seconds) = match phase {
                    Phase::Watch => match rng.next() {
                        r if r < 0.35 => (Phase::Glance, rng.range(4.0, 6.0)),
                        r if r < 0.75 => (Phase::Wander(free_spot(&mut rng, position)), 20.0),
                        r if r < 0.9 => (Phase::Stretch, 3.0),
                        _ => (Phase::Squat, 2.5),
                    },
                    Phase::Wander(_) => (Phase::FaceViewer, 6.0),
                    Phase::Glance | Phase::FaceViewer | Phase::Stretch | Phase::Squat => {
                        (Phase::Watch, rng.range(6.0, 12.0))
                    }
                };
                phase = next;
                phase_end = now + Duration::from_secs_f32(seconds);
            }
        }
    });
}

fn robot_pose(sim: &Mutex<SimState>) -> Option<Mat4> {
    let s = sim.lock().unwrap();
    let scene = s.scene.as_ref()?;
    let robot = scene.robots.iter().find(|r| r.name == "NAO")?;
    scene.world_matrix_of(robot.node)
}

/// A random spot on the open floor, at least a metre away and clear of the crates.
fn free_spot(rng: &mut Rng, from: Vec2) -> Vec2 {
    for _ in 0..50 {
        let p = Vec2::new(rng.range(FREE_MIN.x, FREE_MAX.x), rng.range(FREE_MIN.y, FREE_MAX.y));
        if p.distance(from) > 1.0 && p.distance(CRATES) > 0.8 {
            return p;
        }
    }
    Vec2::ZERO
}

fn round_numbers(value: &Value) -> Value {
    match value {
        Value::Number(n) => n.as_f64().map_or(value.clone(), |f| json!((f * 5.0).round() / 5.0)),
        Value::Array(a) => Value::Array(a.iter().map(round_numbers).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), round_numbers(v))).collect()),
        _ => value.clone(),
    }
}
