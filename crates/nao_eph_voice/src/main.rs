//! `nao-eph-voice`: runs on the NAO next to HULKs (systemd unit `nao-eph-voice.service`), or on a
//! Mac against hulk_webots with `--simulation`.

use std::sync::{atomic::AtomicBool, Arc};

use clap::Parser;
use nao_eph_voice::{converse_forever, robot, Status, AGENT_ID};

#[derive(Parser)]
struct Arguments {
    /// ElevenLabs agent to talk to.
    #[arg(long, env = "NAO_EPH_AGENT_ID", default_value = AGENT_ID)]
    agent_id: String,
    /// HULKs debug server.
    #[arg(long, default_value = "ws://localhost:1337")]
    hulk: String,
    /// Put the robot into the active state without the chest button (Webots).
    #[arg(long)]
    simulation: bool,
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let arguments = Arguments::parse();
    let executor = robot::Executor::new(robot::hulk_sink(arguments.hulk, arguments.simulation));
    converse_forever(&arguments.agent_id, &executor, &Arc::new(Status::default()), &AtomicBool::new(false));
    Ok(())
}
