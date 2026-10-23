//! Talk to the NAO. An ElevenLabs conversational agent listens through the microphone, answers
//! through the speaker and moves the robot with client tools that become HULKs motion commands.
//! The same code runs on the robot (ALSA, HULKs on localhost) and in the simulator app.

pub mod audio;
pub mod robot;

use std::{
    io::ErrorKind,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use color_eyre::{eyre::eyre, Result};
use serde_json::{json, Value};
use tungstenite::{stream::MaybeTlsStream, Message};

/// The public "nao-eph" agent (tools/voice/agent.py creates and updates it).
pub const AGENT_ID: &str = "agent_4301m5n2danmf5ktpg7redx31ner";

/// The microphone stays muted while the robot talks and this long after, so the agent does not
/// hear its own voice (the NAO's microphones sit right next to its speakers).
const ECHO_TAIL: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Status {
    pub connected: AtomicBool,
    pub speaking: AtomicBool,
    /// The last thing said, by "you" or "nao".
    pub last_line: Mutex<Option<(&'static str, String)>>,
}

/// Holds a conversation until `stop` is set or the connection fails.
pub fn converse(
    agent_id: &str,
    executor: &robot::Executor,
    status: &Status,
    stop: &AtomicBool,
) -> Result<()> {
    let audio = audio::Audio::open()?;
    let url = format!("wss://api.elevenlabs.io/v1/convai/conversation?agent_id={agent_id}");
    let (mut socket, _) = tungstenite::connect(url)?;
    match socket.get_mut() {
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(Some(Duration::from_millis(10)))?,
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(Duration::from_millis(10)))?,
        _ => {}
    }
    socket.send(Message::text(json!({"type": "conversation_initiation_client_data"}).to_string()))?;
    status.connected.store(true, Ordering::SeqCst);
    let mut muted_until = Instant::now();

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::SeqCst) {
            let speaking = !audio.playback.lock().unwrap().is_empty();
            status.speaking.store(speaking, Ordering::SeqCst);
            if speaking {
                muted_until = Instant::now() + ECHO_TAIL;
            }
            while let Ok(mut chunk) = audio.captured.try_recv() {
                if Instant::now() < muted_until {
                    chunk.fill(0);
                }
                let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                socket.send(Message::text(json!({"user_audio_chunk": STANDARD.encode(bytes)}).to_string()))?;
            }

            let event: Value = match socket.read() {
                Ok(Message::Text(text)) => serde_json::from_str(&text)?,
                Ok(Message::Close(_)) => return Err(eyre!("the agent ended the conversation")),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    continue
                }
                Err(e) => return Err(e.into()),
            };
            match event["type"].as_str().unwrap_or_default() {
                "ping" => {
                    let id = &event["ping_event"]["event_id"];
                    socket.send(Message::text(json!({"type": "pong", "event_id": id}).to_string()))?;
                }
                "audio" => {
                    let pcm = STANDARD.decode(event["audio_event"]["audio_base_64"].as_str().unwrap_or_default())?;
                    let mut playback = audio.playback.lock().unwrap();
                    playback.extend(pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0));
                }
                "interruption" => audio.playback.lock().unwrap().clear(),
                "user_transcript" => {
                    let text = event["user_transcription_event"]["user_transcript"].as_str().unwrap_or_default();
                    eprintln!("you: {text}");
                    *status.last_line.lock().unwrap() = Some(("you", text.to_owned()));
                }
                "agent_response" => {
                    let text = event["agent_response_event"]["agent_response"].as_str().unwrap_or_default();
                    eprintln!("nao: {text}");
                    *status.last_line.lock().unwrap() = Some(("nao", text.to_owned()));
                }
                "client_tool_call" => {
                    let call = &event["client_tool_call"];
                    let name = call["tool_name"].as_str().unwrap_or_default();
                    eprintln!("tool: {name} {}", call["parameters"]);
                    let (result, is_error) = match robot::plan(name, &call["parameters"]) {
                        Ok(steps) => {
                            executor.run(steps);
                            ("ok".to_owned(), false)
                        }
                        Err(message) => (message, true),
                    };
                    socket.send(Message::text(
                        json!({"type": "client_tool_result", "tool_call_id": call["tool_call_id"], "result": result, "is_error": is_error})
                            .to_string(),
                    ))?;
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = socket.close(None);
    status.connected.store(false, Ordering::SeqCst);
    status.speaking.store(false, Ordering::SeqCst);
    result
}

/// Keeps a conversation going, reconnecting after errors, until `stop` is set.
pub fn converse_forever(agent_id: &str, executor: &robot::Executor, status: &Arc<Status>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        if let Err(error) = converse(agent_id, executor, status, stop) {
            eprintln!("nao-eph-voice: {error:#}");
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}
