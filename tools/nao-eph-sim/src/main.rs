//! nao-eph-sim: a clean native window onto a NAO simulated by Webots and driven by HULKs.
//!
//! Webots runs headless (`--stream=w3d`) and does physics and sensors. hulk_webots, the HULKs robot
//! software, controls the NAO as an extern controller. This app renders the streamed scene with
//! wgpu, draws the UI with GPUI and sends actions to HULKs over its debug server.

mod hulk;
mod life;
mod mesh;
mod render;
mod scene;
mod stream;

use anyhow::{Context as _, Result};
use glam::Vec3;
use gpui::{
    AnyElement, App, Application, Bounds, Context, Div, FontWeight, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, Pixels, Point, RenderImage, ScrollWheelEvent, SharedString,
    Stateful, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, div, img, point,
    prelude::*, px, rgba, size,
};
use hulk::{Action, HulkState, Message};
use render::{Camera, Renderer, View};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command as Process, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};
use stream::{Command, Connection, RunState, SimState};

/// A port of our own, so an already running Webots on 1234 never gets mixed up with ours.
const PORT: u16 = 1250;
const SIDEBAR_WIDTH: f32 = 248.;

/// `nao-eph-sim.app/Contents/Resources` when running from the app bundle.
fn bundle_resources() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let resources = exe.parent()?.parent()?.join("Resources");
    resources.join("webots").is_dir().then_some(resources)
}

/// The nao-eph checkout this binary was built from (for running from source).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The Webots shipped inside the app, or a system install when running from source.
fn webots_app() -> PathBuf {
    bundle_resources()
        .map(|r| r.join("Webots.app"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("/Applications/Webots.app"))
}

fn default_world() -> PathBuf {
    match bundle_resources() {
        Some(r) => r.join("webots/worlds/nao_eph.wbt"),
        None => repo_root().join("webots/worlds/nao_eph.wbt"),
    }
}

/// Webots caches downloaded models and textures under the SHA-1 of their URL. The app bundle ships
/// that cache pre-filled, so the world loads without downloading anything.
fn seed_asset_cache() {
    let Some(source) = bundle_resources().map(|r| r.join("webots-cache")) else { return };
    let Ok(home) = std::env::var("HOME") else { return };
    let target = Path::new(&home).join("Library/Caches/Cyberbotics/Webots/assets");
    let _ = std::fs::create_dir_all(&target);
    for entry in std::fs::read_dir(source).into_iter().flatten().flatten() {
        let dest = target.join(entry.file_name());
        if !dest.exists() {
            let _ = std::fs::copy(entry.path(), dest);
        }
    }
}

fn launch_webots(world: &Path) -> Result<Child> {
    let webots = webots_app().join("Contents/MacOS/webots");
    seed_asset_cache();
    Process::new(&webots)
        .args(["--stream=w3d", "--batch", "--minimize", "--no-rendering"])
        .arg(format!("--port={PORT}"))
        .arg(world)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", webots.display()))
}

/// Where hulk_webots runs. It reads `etc/parameters` relative to its working directory and writes
/// `logs/`, so the app bundle gets a writable folder that links to the bundled parameters.
fn hulk_workdir() -> Result<(PathBuf, PathBuf)> {
    let Some(resources) = bundle_resources() else {
        let root = repo_root().canonicalize()?;
        std::fs::create_dir_all(root.join("logs"))?;
        return Ok((root.join("target/release/hulk_webots"), root));
    };
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = Path::new(&home).join("Library/Application Support/nao-eph-sim");
    std::fs::create_dir_all(dir.join("logs"))?;
    let etc = dir.join("etc");
    let _ = std::fs::remove_file(&etc);
    std::os::unix::fs::symlink(resources.join("hulk/etc"), &etc)?;
    Ok((resources.join("hulk/hulk_webots"), dir))
}

fn launch_hulk(state: Arc<Mutex<SimState>>) -> Result<Child> {
    let (binary, workdir) = hulk_workdir()?;
    let mut child = Process::new(&binary)
        .arg("etc/parameters/framework.sim.json")
        .current_dir(workdir)
        .env("WEBOTS_HOME", webots_app())
        .env("WEBOTS_CONTROLLER_URL", format!("ipc://{PORT}/NAO"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;
    // Show HULKs' log in the console, minus its per-cycle debug chatter.
    for pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let state = state.clone();
        thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if !line.contains(" DEBUG ") && !line.contains("openvino") {
                    state.lock().unwrap().log(&format!("[hulk] {line}"));
                }
            }
        });
    }
    Ok(child)
}

/// Webots crashes in its own teardown when it gets a polite quit mid-download, so end it hard.
fn stop(children: &Mutex<Vec<Child>>) {
    for mut c in children.lock().unwrap().drain(..) {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Camera in front of the NAO, slightly to the side, so the robot looks at you.
fn camera_facing_robot(scene: &scene::Scene) -> Camera {
    let robot = scene
        .robots
        .iter()
        .find(|r| r.name == "NAO")
        .or_else(|| scene.robots.first())
        .and_then(|r| scene.world_matrix_of(r.node));
    match robot {
        Some(m) => {
            let facing = m.x_axis.truncate();
            Camera {
                target: m.w_axis.truncate() + Vec3::Z * 0.05,
                yaw: facing.y.atan2(facing.x) + 0.12,
                pitch: 0.17,
                distance: 2.9,
            }
        }
        None => Camera { target: Vec3::ZERO, yaw: 0.6, pitch: 0.3, distance: 4.0 },
    }
}

/// Studio: close in front of the robot, at its own height.
fn camera_studio(scene: &scene::Scene) -> Camera {
    let mut camera = camera_facing_robot(scene);
    camera.target.z = 0.28;
    camera.yaw -= 0.12;
    camera.pitch = 0.06;
    camera.distance = 1.35;
    camera
}

fn robot_node(scene: &scene::Scene) -> Option<usize> {
    scene.robots.iter().find(|r| r.name == "NAO").map(|r| r.node)
}

/// Eases the rendered camera toward the requested one so orbiting and zooming feel smooth.
fn ease(from: Camera, to: Camera) -> Camera {
    let k = 0.35;
    let shown = Camera {
        target: from.target.lerp(to.target, k),
        yaw: from.yaw + (to.yaw - from.yaw) * k,
        pitch: from.pitch + (to.pitch - from.pitch) * k,
        distance: from.distance + (to.distance - from.distance) * k,
    };
    let settled = (shown.target - to.target).length() < 1e-4
        && (shown.yaw - to.yaw).abs() < 1e-4
        && (shown.pitch - to.pitch).abs() < 1e-4
        && (shown.distance - to.distance).abs() < 1e-4;
    if settled { to } else { shown }
}

/// What the window asks the render thread for.
#[derive(Default)]
struct ViewInput {
    camera: Option<Camera>,
    view: View,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct FrameSlot {
    seq: u64,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

fn spawn_renderer(
    world_dir: PathBuf,
    state: Arc<Mutex<SimState>>,
    view: Arc<Mutex<ViewInput>>,
    frame: Arc<Mutex<FrameSlot>>,
    viewer: Arc<Mutex<Option<Vec3>>>,
) {
    thread::spawn(move || {
        let mut renderer = match Renderer::new(world_dir) {
            Ok(r) => r,
            Err(e) => {
                state.lock().unwrap().log(&format!("[nao-eph-sim] GPU init failed: {e:#}"));
                return;
            }
        };
        let mut shown: Option<Camera> = None;
        let mut last = (u64::MAX, None::<Camera>, 0u32, 0u32, View::World);
        let mut last_render = Instant::now();
        loop {
            thread::sleep(Duration::from_millis(6));
            let s = state.lock().unwrap();
            let Some(scene) = s.scene.as_ref() else { continue };
            let (target, width, height, mode) = {
                let mut v = view.lock().unwrap();
                let mode = v.view;
                let camera = *v.camera.get_or_insert_with(|| match mode {
                    View::World => camera_facing_robot(scene),
                    View::Studio { .. } => camera_studio(scene),
                });
                (camera, v.width, v.height, mode)
            };
            if width == 0 || height == 0 {
                continue;
            }
            let camera = shown.map_or(target, |c| ease(c, target));
            shown = Some(camera);
            // Life mode looks at whoever is watching.
            *viewer.lock().unwrap() = Some(camera.eye());
            // Re-render on any change, and every 250 ms so freshly downloaded textures appear.
            let key = (s.revision, Some(camera), width, height, mode);
            if key == last && last_render.elapsed() < Duration::from_millis(250) {
                continue;
            }
            let pixels = renderer.render(scene, s.scene_generation, &camera, width, height, mode, robot_node(scene));
            drop(s);
            last = key;
            last_render = Instant::now();
            let mut f = frame.lock().unwrap();
            f.seq += 1;
            f.width = width;
            f.height = height;
            f.pixels = pixels;
        }
    });
}

struct SimApp {
    state: Arc<Mutex<SimState>>,
    hulk: Arc<Mutex<HulkState>>,
    commands: Sender<Command>,
    actions: Sender<Message>,
    view: Arc<Mutex<ViewInput>>,
    frame: Arc<Mutex<FrameSlot>>,
    image: Option<Arc<RenderImage>>,
    shown_seq: u64,
    stale: Vec<Arc<RenderImage>>,
    drag: Option<(Point<Pixels>, MouseButton)>,
    show_console: bool,
    studio: Arc<AtomicBool>,
    voice: Option<Voice>,
    _poll: Task<()>,
}

/// A running conversation with the voice agent.
struct Voice {
    status: Arc<nao_eph_voice::Status>,
    stop: Arc<AtomicBool>,
}

impl Voice {
    fn start(actions: Sender<Message>) -> Self {
        let status = Arc::new(nao_eph_voice::Status::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_status, thread_stop) = (status.clone(), stop.clone());
        thread::spawn(move || {
            let actions = Mutex::new(actions);
            let executor = nao_eph_voice::robot::Executor::new(move |command| {
                let _ = actions.lock().unwrap().send(Message::Motion(command));
            });
            nao_eph_voice::converse_forever(nao_eph_voice::AGENT_ID, &executor, &thread_status, &thread_stop);
        });
        Self { status, stop }
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl SimApp {
    fn new(
        state: Arc<Mutex<SimState>>,
        hulk: Arc<Mutex<HulkState>>,
        commands: Sender<Command>,
        actions: Sender<Message>,
        view: Arc<Mutex<ViewInput>>,
        frame: Arc<Mutex<FrameSlot>>,
        studio: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_millis(8)).await;
                ticks += 1;
                let alive = this.update(cx, |this, cx| {
                    // Choosing another motion or mode ends the conversation.
                    if this.voice.is_some() && this.hulk.lock().unwrap().action != Action::Talk {
                        this.voice = None;
                    }
                    if this.take_frame() || ticks % 30 == 0 {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            state,
            hulk,
            commands,
            actions,
            view,
            frame,
            image: None,
            shown_seq: 0,
            stale: Vec::new(),
            drag: None,
            show_console: false,
            studio,
            voice: None,
            _poll: poll,
        }
    }

    fn take_frame(&mut self) -> bool {
        let mut f = self.frame.lock().unwrap();
        if f.seq == self.shown_seq || f.pixels.is_empty() {
            return false;
        }
        self.shown_seq = f.seq;
        let pixels = std::mem::take(&mut f.pixels);
        // GPUI reads these bytes as BGRA, which is what the renderer produced.
        let Some(buffer) = image::RgbaImage::from_raw(f.width, f.height, pixels) else {
            return false;
        };
        let new = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        if let Some(old) = self.image.replace(new) {
            self.stale.push(old);
        }
        true
    }

    fn set_view(&mut self, mode: View, cx: &mut Context<Self>) {
        let mut v = self.view.lock().unwrap();
        if v.view != mode {
            // A fresh default camera when switching between world and studio.
            if matches!(v.view, View::World) != matches!(mode, View::World) {
                v.camera = None;
            }
            v.view = mode;
        }
        drop(v);
        let in_studio = !matches!(mode, View::World);
        self.studio.store(in_studio, Ordering::Relaxed);
        // The studio is for standing still: stop any walking motion.
        let action = self.hulk.lock().unwrap().action;
        if in_studio && matches!(action, Action::WalkForward | Action::WalkCircle | Action::TurnLeft) {
            let _ = self.actions.send(Message::Action(Action::Stand));
        }
        cx.notify();
    }

    fn with_camera(&self, f: impl FnOnce(&mut Camera)) {
        if let Some(camera) = self.view.lock().unwrap().camera.as_mut() {
            f(camera);
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, _: &mut Context<Self>) {
        if event.click_count == 2 {
            // Double click: back to the default view of the robot.
            self.view.lock().unwrap().camera = None;
        }
        self.drag = Some((event.position, event.button));
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some((last, button)) = self.drag else { return };
        if event.pressed_button.is_none() {
            self.drag = None;
            return;
        }
        let delta = event.position - last;
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        let pan = button == MouseButton::Right || event.modifiers.shift;
        self.with_camera(|c| {
            if pan {
                c.pan(dx, dy);
            } else {
                c.yaw -= dx * 0.006;
                c.pitch = (c.pitch + dy * 0.005).clamp(-0.1, 1.45);
            }
        });
        self.drag = Some((event.position, button));
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, _: &mut Context<Self>) {
        let dy = f32::from(event.delta.pixel_delta(px(16.)).y);
        self.with_camera(|c| c.distance = (c.distance * (1.0 - dy * 0.0025)).clamp(0.4, 30.0));
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (connection, run_state, time) = {
            let s = self.state.lock().unwrap();
            (s.connection, s.run_state, s.time_ms)
        };
        let (hulk_connected, action) = {
            let h = self.hulk.lock().unwrap();
            (h.connected, h.action)
        };
        let mode = self.view.lock().unwrap().view;
        let live = connection == Connection::Connected && run_state != RunState::Paused && hulk_connected;
        let status = match (connection, hulk_connected) {
            (Connection::Connecting, _) => "Starting Webots…",
            (Connection::Loading, _) => "Loading world…",
            (Connection::Connected, false) => "Starting HULKs…",
            (Connection::Connected, true) if run_state == RunState::Paused => "Paused",
            (Connection::Connected, true) => "Simulation live",
        };

        let motions: Vec<AnyElement> = Action::MOTIONS
            .iter()
            .map(|&a| {
                let walks = matches!(a, Action::WalkForward | Action::WalkCircle | Action::TurnLeft);
                nav_row(a.label(), action == a, hulk_connected && !(walks && mode != View::World))
                    .on_click(cx.listener(move |this, _, _, _| {
                        let _ = this.actions.send(Message::Action(a));
                    }))
                    .into_any_element()
            })
            .collect();

        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .flex_none()
            .bg(rgba(SIDEBAR))
            .border_r_1()
            .border_color(rgba(BORDER))
            .text_color(rgba(FOREGROUND))
            // Header, below the window's traffic lights.
            .child(
                div()
                    .pt(px(44.))
                    .h(px(92.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(img(app_icon()).size(px(22.)).rounded_md())
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("nao-eph"))
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgba(BORDER))
                            .text_xs()
                            .text_color(rgba(MUTED))
                            .child("Sim"),
                    ),
            )
            .child(
                div()
                    .id("nav")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_3()
                    .pb_4()
                    .child(section_label("Motion"))
                    .child(div().flex().flex_col().gap_0p5().children(motions))
                    .child(section_label("Mode"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                toggle_row("life", "Life mode", action == Action::Life, hulk_connected, None)
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        let next = if action == Action::Life { Action::Stand } else { Action::Life };
                                        let _ = this.actions.send(Message::Action(next));
                                    })),
                            )
                            .child(
                                toggle_row("talk", "Talk", self.voice.is_some(), hulk_connected, None).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        if this.voice.take().is_none() {
                                            this.hulk.lock().unwrap().action = Action::Talk;
                                            let _ = this.actions.send(Message::Action(Action::Talk));
                                            this.voice = Some(Voice::start(this.actions.clone()));
                                        } else {
                                            let _ = this.actions.send(Message::Action(Action::Life));
                                        }
                                        cx.notify();
                                    }),
                                ),
                            )
                            .when_some(self.voice.as_ref(), |d, voice| {
                                let state = if !voice.status.connected.load(Ordering::Relaxed) {
                                    "Connecting…"
                                } else if voice.status.speaking.load(Ordering::Relaxed) {
                                    "Speaking"
                                } else {
                                    "Listening – just talk"
                                };
                                let line = voice.status.last_line.lock().unwrap().clone();
                                d.child(
                                    div()
                                        .px_3()
                                        .pt_1()
                                        .pb_2()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .text_xs()
                                        .child(div().text_color(rgba(BRAND)).child(state))
                                        .when_some(line, |d, (who, text)| {
                                            d.child(div().text_color(rgba(MUTED)).child(format!(
                                                "{}: {text}",
                                                if who == "you" { "You" } else { "NAO" }
                                            )))
                                        }),
                                )
                            }),
                    )
                    .child(section_label("View"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(nav_row("World", mode == View::World, true).on_click(cx.listener(
                                |this, _, _, cx| this.set_view(View::World, cx),
                            )))
                            .child(nav_row("Studio", mode != View::World, true).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    // Dark unless the white backdrop was chosen in this session.
                                    let dark = !matches!(mode, View::Studio { dark: false });
                                    this.set_view(View::Studio { dark }, cx)
                                },
                            )))
                            .when(mode != View::World, |d| {
                                let dark = matches!(mode, View::Studio { dark: true });
                                d.child(
                                    div()
                                        .mx_2()
                                        .my_1()
                                        .p(px(2.))
                                        .flex()
                                        .rounded_md()
                                        .bg(rgba(ACCENT))
                                        .child(segment("light", "Light", !dark).on_click(cx.listener(
                                            |this, _, _, cx| this.set_view(View::Studio { dark: false }, cx),
                                        )))
                                        .child(segment("dark", "Dark", dark).on_click(cx.listener(
                                            |this, _, _, cx| this.set_view(View::Studio { dark: true }, cx),
                                        ))),
                                )
                            })
                            .child(
                                toggle_row("console", "Console", self.show_console, true, None).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.show_console = !this.show_console;
                                        cx.notify();
                                    }),
                                ),
                            )
                            .child(nav_row("Reset camera", false, true).on_click(cx.listener(
                                |this, _, _, _| {
                                    this.view.lock().unwrap().camera = None;
                                },
                            )))
                            .child(nav_row("Restart simulation", false, true).on_click(cx.listener(
                                |this, _, _, _| {
                                    let _ = this.commands.send(Command::Reset);
                                },
                            ))),
                    ),
            )
            // Footer: one dot that turns blue when everything is live, like Kiste's console.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(rgba(BORDER))
                    .text_xs()
                    .text_color(rgba(MUTED))
                    .child(div().size(px(6.)).rounded_full().bg(rgba(if live { BRAND } else { 0x7a7a7aff })))
                    .child(status)
                    .child(div().ml_auto().child(format_time(time))),
            )
    }
}

// Kiste's console palette: black and white, with one functional colour (blue) for live state.
const SIDEBAR: u32 = 0x0f0f0fff;
const FOREGROUND: u32 = 0xfafafaff;
const ROW_TEXT: u32 = 0xfafafabf;
const MUTED: u32 = 0xa1a1a1ff;
const ACCENT: u32 = 0x1f1f1fff;
const BORDER: u32 = 0xffffff1a;
const BRAND: u32 = 0x5b9bffff;

/// The app icon: inside the bundle, or from the source tree.
fn app_icon() -> PathBuf {
    bundle_resources()
        .map(|r| r.join("icon.png"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon-1024.png"))
}

fn section_label(text: &'static str) -> impl IntoElement {
    div()
        .mt_4()
        .h(px(28.))
        .px_2()
        .flex()
        .items_center()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgba(MUTED))
        .child(text)
}

fn row_base(id: &'static str, active: bool, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_2()
        .flex()
        .items_center()
        .gap_2p5()
        .rounded_md()
        .text_sm()
        .text_color(rgba(if active { FOREGROUND } else { ROW_TEXT }))
        .when(active, |d| d.bg(rgba(ACCENT)).font_weight(FontWeight::MEDIUM))
        .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(rgba(ACCENT)).text_color(rgba(FOREGROUND))))
        .when(!enabled, |d| d.text_color(rgba(0x6b6b6bff)))
}

fn nav_row(label: &'static str, active: bool, enabled: bool) -> Stateful<Div> {
    row_base(label, active, enabled).child(label)
}

fn toggle_row(id: &'static str, label: &'static str, on: bool, enabled: bool, badge: Option<&'static str>) -> Stateful<Div> {
    row_base(id, false, enabled)
        .child(label)
        .child(div().ml_auto())
        .when_some(badge, |d, b| {
            d.child(
                div()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgba(BORDER))
                    .text_xs()
                    .text_color(rgba(MUTED))
                    .child(b),
            )
        })
        .when(badge.is_none(), |d| d.child(switch(on, enabled)))
}

/// One half of a two-way segmented control.
fn segment(id: &'static str, label: &'static str, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .h(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .text_xs()
        .cursor_pointer()
        .when(active, |d| d.bg(rgba(0x3a3a3aff)).text_color(rgba(FOREGROUND)).font_weight(FontWeight::MEDIUM))
        .when(!active, |d| d.text_color(rgba(MUTED)).hover(|s| s.text_color(rgba(FOREGROUND))))
        .child(label)
}

fn switch(on: bool, enabled: bool) -> impl IntoElement {
    div()
        .w(px(28.))
        .h(px(16.))
        .p(px(2.))
        .rounded_full()
        .flex()
        .when(on, |d| d.bg(rgba(BRAND)).justify_end())
        .when(!on, |d| d.bg(rgba(if enabled { 0x3a3a3aff } else { 0x262626ff })))
        .child(div().size(px(12.)).rounded_full().bg(rgba(FOREGROUND)))
}

fn format_time(ms: f64) -> String {
    let total = (ms / 1000.0).max(0.0);
    let minutes = (total / 60.0) as u64;
    format!("{minutes:02}:{:04.1}", total - minutes as f64 * 60.0)
}

impl Render for SimApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for old in self.stale.drain(..) {
            let _ = window.drop_image(old);
        }
        // Render at device resolution, capped so huge displays stay fast.
        let viewport = window.viewport_size();
        let scale = window.scale_factor().min(2.0);
        {
            let mut v = self.view.lock().unwrap();
            v.width = (((f32::from(viewport.width) - SIDEBAR_WIDTH) * scale) as u32).min(3200);
            v.height = ((f32::from(viewport.height) * scale) as u32).min(2000);
        }
        let (status_text, console): (SharedString, Vec<String>) = {
            let s = self.state.lock().unwrap();
            let status = match s.connection {
                Connection::Connecting => "Starting Webots…",
                Connection::Loading => "Loading world…",
                Connection::Connected => "",
            };
            (status.into(), s.console.iter().rev().take(16).rev().cloned().collect())
        };

        let viewport_area = div()
            .id("viewport")
            .relative()
            .flex_1()
            .h_full()
            .bg(rgba(0x0f1115ff))
            .when_some(self.image.clone(), |d, image| {
                d.child(img(image).size_full().object_fit(ObjectFit::Fill))
            })
            .when(self.image.is_none(), |d| {
                d.flex().items_center().justify_center().child(
                    div().text_color(rgba(0x9aa3b2ff)).text_sm().child(status_text),
                )
            })
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.drag = None))
            .on_mouse_up(MouseButton::Right, cx.listener(|this, _, _, _| this.drag = None))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .when(self.show_console, |d| {
                d.child(
                    div()
                        .absolute()
                        .left(px(16.))
                        .right(px(16.))
                        .bottom(px(16.))
                        .max_h(px(260.))
                        .p_3()
                        .rounded_xl()
                        .bg(rgba(0x121419f0))
                        .border_1()
                        .border_color(rgba(0xffffff1f))
                        .shadow_lg()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .overflow_hidden()
                        .text_xs()
                        .font_family("Menlo")
                        .text_color(rgba(0xc9d1dcff))
                        .when(console.is_empty(), |d| d.child("No output yet."))
                        .children(console.into_iter().map(|l| div().child(l))),
                )
            });

        div()
            .size_full()
            .flex()
            .bg(rgba(0x0f1115ff))
            .font_family(".SystemUIFont")
            .child(self.sidebar(cx))
            .child(viewport_area)
    }
}

/// Headless check: start everything, run an action, save one frame as PNG.
fn snapshot(world: &Path, out: &Path, action: Option<Action>) -> Result<()> {
    let state = Arc::new(Mutex::new(SimState::default()));
    let children = Arc::new(Mutex::new(vec![launch_webots(world)?]));
    let result = (|| {
        let _commands = stream::spawn(format!("ws://localhost:{PORT}"), state.clone());
        children.lock().unwrap().push(launch_hulk(state.clone())?);
        let hulk_state = Arc::new(Mutex::new(HulkState { connected: false, action: Action::Stand }));
        let actions = hulk::spawn(hulk_state.clone());
        let viewer = Arc::new(Mutex::new(None));
        life::spawn(state.clone(), hulk_state.clone(), viewer, Arc::new(AtomicBool::new(false)), actions.clone());
        let started = Instant::now();
        while !hulk_state.lock().unwrap().connected {
            anyhow::ensure!(started.elapsed() < Duration::from_secs(120), "HULKs did not connect");
            thread::sleep(Duration::from_millis(200));
        }
        if let Some(action) = action {
            actions.send(Message::Action(action))?;
        }
        // NAO_EPH_TALK_TEST=1 (with NAO_EPH_VOICE_TEST_WAV): hold a conversation first.
        let _voice = std::env::var_os("NAO_EPH_TALK_TEST").map(|_| {
            hulk_state.lock().unwrap().action = Action::Talk;
            let _ = actions.send(Message::Action(Action::Talk));
            Voice::start(actions.clone())
        });
        let wait = if _voice.is_some() { 30 } else { 8 };
        thread::sleep(Duration::from_secs(wait));
        let mut renderer = Renderer::new(world.parent().unwrap().to_path_buf())?;
        let (width, height) = (1600, 900);
        let mut pixels = Vec::new();
        for _ in 0..2 {
            let s = state.lock().unwrap();
            let scene = s.scene.as_ref().context("no scene")?;
            let view = match std::env::var("NAO_EPH_VIEW").as_deref() {
                Ok("studio") => View::Studio { dark: false },
                Ok("studio-dark") => View::Studio { dark: true },
                _ => View::World,
            };
            let camera = if view == View::World { camera_facing_robot(scene) } else { camera_studio(scene) };
            pixels = renderer.render(scene, s.scene_generation, &camera, width, height, view, robot_node(scene));
            drop(s);
            thread::sleep(Duration::from_secs(2));
        }
        for p in pixels.chunks_mut(4) {
            p.swap(0, 2);
        }
        image::RgbaImage::from_raw(width, height, pixels)
            .context("frame size")?
            .save(out)?;
        Ok(())
    })();
    stop(&children);
    result
}

/// macOS window restoration can deadlock GPUI when it reopens a window from an earlier session
/// (e.g. after the app was force-quit). The app always starts fresh, so turn restoration off.
fn disable_window_restoration() {
    let Ok(home) = std::env::var("HOME") else { return };
    for id in ["com.lukaloehr.nao-eph", "com.lukaloehr.nao-eph-sim.dev"] {
        let saved = Path::new(&home).join(format!("Library/Saved Application State/{id}.savedState"));
        let _ = std::fs::remove_dir_all(saved);
        let _ = Process::new("/usr/bin/defaults")
            .args(["write", id, "ApplePersistenceIgnoreState", "-bool", "true"])
            .status();
    }
}

fn main() -> Result<()> {
    disable_window_restoration();
    let mut args = std::env::args().skip(1);
    let mut world = default_world();
    let mut snapshot_out = None;
    let mut snapshot_action = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" => snapshot_out = args.next().map(PathBuf::from),
            "--walk" => snapshot_action = Some(Action::WalkForward),
            "--life" => snapshot_action = Some(Action::Life),
            _ => world = PathBuf::from(arg),
        }
    }
    let world = world.canonicalize().with_context(|| format!("world {}", world.display()))?;
    if let Some(out) = snapshot_out {
        return snapshot(&world, &out, snapshot_action);
    }

    let state = Arc::new(Mutex::new(SimState::default()));
    let children = Arc::new(Mutex::new(vec![launch_webots(&world)?]));
    match launch_hulk(state.clone()) {
        Ok(child) => children.lock().unwrap().push(child),
        Err(e) => state.lock().unwrap().log(&format!("[nao-eph-sim] HULKs not started: {e:#}")),
    }
    let commands = stream::spawn(format!("ws://localhost:{PORT}"), state.clone());
    // Start in Life mode so the robot behaves like a real NAO from the first second.
    let hulk_state = Arc::new(Mutex::new(HulkState { connected: false, action: Action::Life }));
    let actions = hulk::spawn(hulk_state.clone());
    // NAO_EPH_VIEW=studio or studio-dark opens straight into the studio.
    let start_view = match std::env::var("NAO_EPH_VIEW").as_deref() {
        Ok("studio") => View::Studio { dark: false },
        Ok("studio-dark") => View::Studio { dark: true },
        _ => View::World,
    };
    let view = Arc::new(Mutex::new(ViewInput { view: start_view, ..Default::default() }));
    let frame = Arc::new(Mutex::new(FrameSlot::default()));
    let viewer = Arc::new(Mutex::new(None));
    let studio = Arc::new(AtomicBool::new(start_view != View::World));
    spawn_renderer(world.parent().unwrap().to_path_buf(), state.clone(), view.clone(), frame.clone(), viewer.clone());
    life::spawn(state.clone(), hulk_state.clone(), viewer, studio.clone(), actions.clone());

    Application::new().run(move |cx: &mut App| {
        let quit_children = children.clone();
        cx.on_app_quit(move |_| {
            stop(&quit_children);
            async {}
        })
        .detach();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1480.), px(920.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("nao-eph".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(16.), px(18.))),
                }),
                window_min_size: Some(size(px(900.), px(560.))),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| SimApp::new(state, hulk_state, commands, actions, view, frame, studio, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
    Ok(())
}
