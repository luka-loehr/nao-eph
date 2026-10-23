![nao-eph](docs/assets/banner.png)

# nao-eph – A NAO robot that talks back

[![Rust](https://img.shields.io/badge/Rust-stable-000000?style=flat&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/Platform-NAO6%20%7C%20Webots-blue?style=flat)](https://github.com/luka-loehr/nao-eph)
[![License](https://img.shields.io/badge/License-GPL--3.0-orange?style=flat)](LICENSE)
[![Based on](https://img.shields.io/badge/Based%20on-HULKs%2Fhulk-1f6feb?style=flat&logo=github&logoColor=white)](https://github.com/HULKs/hulk)

**nao-eph** is the control stack for the NAO6 robot of Lessing-Gymnasium Karlsruhe, part of the Ephraim project. It runs the robot in Rust directly on its hardware, without Aldebaran's NAOqi middleware, and is built on the open-source HULKs framework.

---

## Features

- **No NAOqi** – talks to the robot's motors and sensors directly through LoLA, so the CPU isn't spent on idle animations
- **Written in Rust** – one fast, memory-safe codebase instead of NAOqi's C++ and Python 2 services
- **Walking, vision and behavior** – inherited from HULKs, a competitive RoboCup SPL team
- **3D simulation** – run the same code on a virtual NAO in [Webots](https://cyberbotics.com), no robot needed
- **Live speech (planned)** – real-time voice conversation and tool calling with GPT Realtime

---

## Based on HULKs

nao-eph is a modified version of [HULKs/hulk](https://github.com/HULKs/hulk) by HULKs e.V., licensed under GPL-3.0. It imports HULKs at [`robocup2025`](https://github.com/HULKs/hulk/tree/robocup2025), their last NAO release, and HULKs' own documentation at [hulks.de/hulk](https://hulks.de/hulk/) still applies to the inherited parts. See [NOTICE](NOTICE) for attribution.

---

## License

GPL-3.0 – [View License](LICENSE)

---

## Support

- [Report bugs](https://github.com/luka-loehr/nao-eph/issues)
- [luka@lukaloehr.com](mailto:luka@lukaloehr.com)

---

Developed by [Luka Löhr](https://github.com/luka-loehr)
