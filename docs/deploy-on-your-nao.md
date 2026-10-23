# Put nao-eph on your own NAO

About 20 minutes. You end up with a NAO that stands up when you press its chest button and that you can talk to in German (or English). It answers through its own speaker and moves when you ask it to ("Setz dich hin", "Dreh dich nach links", "Arme hoch!").

> **This replaces the robot's operating system.** NAOqi, Choregraphe and Aldebaran's apps are gone afterwards. You can go back at any time by flashing the official NAOqi image the same way.

## What you need

- A **NAO6** (NAO V6). Older NAOs are not supported.
- A **USB stick** with at least 8 GB.
- An **Ethernet cable** and a network with **DHCP and internet access** (the conversation runs on ElevenLabs' servers).
- A computer with a terminal (macOS or Linux; on Windows use WSL).

## 1. Flash HULKs-OS

nao-eph runs on HULKs-OS, the open robot operating system of the RoboCup team HULKs.

1. Download **`nao-image-HULKs-OS-9.0.1.ext3.gz.opn`** from [HULKs/meta-nao releases](https://github.com/HULKs/meta-nao/releases/tag/9.0.1).
2. Write it onto the USB stick with [balenaEtcher](https://etcher.balena.io) (pick the `.opn` file, pick the stick, *Flash*).
3. Switch the NAO **off**. Plug the stick into the USB port on the back of its head.
4. Hold the **chest button for about 5 seconds** until it glows blue, then let go. The robot flashes itself; this takes about 5 minutes and it restarts on its own.
5. Remove the stick.

## 2. Connect it

Plug the Ethernet cable into the back of the NAO's head and into your router or switch. Find its IP address in your router's list of devices (the device is called `nao` or similar).

Check that it answers (password **`nao`**):

```sh
ssh nao@<ip-address>
```

## 3. Install nao-eph

1. Download **`nao-eph-robot.tar.gz`** and **`install.sh`** from the [latest nao-eph release](https://github.com/luka-loehr/nao-eph/releases/latest) into one folder.
2. In that folder, run:

```sh
chmod +x install.sh
./install.sh <ip-address>
```

Type the password `nao` when asked. Run the same command again later to update.

## 4. Talk to it

1. Put the NAO on the floor, with room around it.
2. **Press the chest button once.** It stiffens and stands up.
3. Just talk. It greets you, listens and answers. Ask it to sit down, stand up, put its arms up, squat, walk a bit, turn or look left and right.

To switch it off safely: touch all three **head buttons** for a moment. It sits down and goes limp. Then hold the chest button to power it off.

## Good to know

- **It does not hear itself:** the microphones are muted while it talks, so wait until it has finished before you answer.
- **Falls:** if it falls, it protects itself and gets back up on its own.
- **Your own agent:** by default it talks to the public `nao-eph` agent. To use your own ElevenLabs agent, run `ELEVENLABS_API_KEY=… tools/voice/agent.py` (it creates an agent with the same voice, prompt and tools) and add `Environment=NAO_EPH_AGENT_ID=<agent id>` to `/etc/systemd/system/nao-eph-voice.service` on the robot.
- **Logs:** `ssh nao@<ip-address> journalctl -u nao-eph-voice -f` shows what it hears, says and does.
- **Building it yourself:** `tools/robot/build.sh` builds `nao-eph-robot.tar.gz` in HULKs' SDK container (x86_64 Linux with Docker).
