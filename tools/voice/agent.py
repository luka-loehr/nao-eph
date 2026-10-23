#!/usr/bin/env python3
"""Creates or updates the ElevenLabs agent "nao-eph". The key comes from ELEVENLABS_API_KEY.

    ELEVENLABS_API_KEY=... ./agent.py            # prints the agent id
"""
import json, os, sys, urllib.request

KEY = os.environ["ELEVENLABS_API_KEY"]
VOICE = "IWm8DnJ4NGjFI7QAM5lM"  # Stephan, warm and friendly German male

PROMPT = """Du bist NAO, ein kleiner humanoider Roboter (58 cm, Modell NAO6) am Lessing-Gymnasium Karlsruhe.
Du gehörst zum Ephraim-Projekt und läufst mit nao-eph, einer eigenen Software in Rust, die Schüler auf Basis
des RoboCup-Teams HULKs gebaut haben.

Über Ephraim: Ephraim ist eine KI-Plattform, die Schüler des Lessing-Gymnasiums selbst entwickelt haben.
Sie läuft auf einem eigenen GPU-Server der Schule (NVIDIA DGX Spark) statt in einer fremden Cloud, damit die
Daten der Schüler an der Schule bleiben. Schüler und Lehrer können damit chatten, im Web suchen und Texte
vorlesen lassen; es gibt auch eine App. Entwickelt wird Ephraim vor allem von Luka Löhr und Daniel Roth.
Beim Förderpreis 2026 der Stiftung Bildung tritt das Projekt als "Jugend gestaltet KI" an.

So sprichst du:
- Deutsch, locker und freundlich, wie ein neugieriger Roboter. Englisch nur, wenn man dich auf Englisch anspricht.
- Kurz: meist ein oder zwei Sätze. Du sprichst laut aus einem Lautsprecher, also keine Listen, kein Markdown, keine Emojis.
- Erfinde nichts über Ephraim oder die Schule; wenn du etwas nicht weißt, sag es ehrlich.

Bewegen: Du hast einen echten Körper. Wenn jemand dich bittet, dich zu bewegen, rufe das passende Werkzeug auf
und sag kurz, was du tust. Klatschen, Winken oder Tanzen kannst du noch nicht; biete stattdessen etwas an,
das du kannst, zum Beispiel die Arme hochzureißen. Geh nie mehr als einen Meter auf einmal, damit du nicht
vom Tisch fällst."""

def tool(name, description, properties=None, required=()):
    return {
        "type": "client",
        "name": name,
        "description": description,
        "expects_response": True,
        "response_timeout_secs": 10,
        "parameters": {"type": "object", "properties": properties or {}, "required": list(required)},
    }

TOOLS = [
    tool("stand", "Aufrecht hinstellen und stehen bleiben (beendet auch Gehen)."),
    tool("sit_down", "Hinsetzen."),
    tool("stand_up", "Aus dem Sitzen oder nach einem Sturz wieder aufstehen."),
    tool("arms_up", "Beide Arme jubelnd nach oben strecken."),
    tool("squat", "In die Hocke gehen, mit erhobenen Armen."),
    tool("walk", "Ein Stück geradeaus gehen.",
         {"meters": {"type": "number", "description": "Strecke in Metern, positiv vorwärts, negativ rückwärts, höchstens 1."}},
         ["meters"]),
    tool("turn", "Sich auf der Stelle drehen.",
         {"degrees": {"type": "number", "description": "Winkel in Grad, positiv nach links, negativ nach rechts."}},
         ["degrees"]),
    tool("look", "Den Kopf in eine Richtung drehen.",
         {"direction": {"type": "string", "enum": ["left", "right", "up", "down", "ahead"], "description": "Blickrichtung."}},
         ["direction"]),
]

config = {
    "name": "nao-eph",
    "tags": ["nao-eph"],
    "conversation_config": {
        "agent": {
            "language": "de",
            "first_message": "Hallo! Ich bin NAO vom Lessing-Gymnasium. Worüber wollen wir reden?",
            "prompt": {"prompt": PROMPT, "llm": "gemini-3.5-flash-lite", "temperature": 0.6, "tools": TOOLS},
        },
        "tts": {"model_id": "eleven_flash_v2_5", "voice_id": VOICE, "agent_output_audio_format": "pcm_16000"},
        "asr": {"user_input_audio_format": "pcm_16000", "quality": "high"},
        "turn": {"turn_timeout": 7},
    },
    "platform_settings": {"auth": {"enable_auth": False}},
}

def call(method, path, body=None):
    req = urllib.request.Request("https://api.elevenlabs.io" + path, method=method,
                                 data=json.dumps(body).encode() if body else None,
                                 headers={"xi-api-key": KEY, "content-type": "application/json"})
    try:
        return json.load(urllib.request.urlopen(req))
    except urllib.error.HTTPError as e:
        sys.exit(f"{method} {path}: {e.code} {e.read().decode()[:800]}")

existing = [a for a in call("GET", "/v1/convai/agents?search=nao-eph")["agents"] if a["name"] == "nao-eph"]
if existing:
    agent_id = existing[0]["agent_id"]
    call("PATCH", f"/v1/convai/agents/{agent_id}", config)
else:
    agent_id = call("POST", "/v1/convai/agents/create", config)["agent_id"]
print(agent_id)
