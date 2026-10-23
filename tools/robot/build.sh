#!/usr/bin/env bash
# Builds nao-eph-robot.tar.gz (HULKs for the NAO plus nao-eph-voice) in HULKs' SDK container.
# Needs an x86_64 Linux machine (or Mac with x86 emulation) with Docker.
set -euo pipefail
cd "$(dirname "$0")/../.."
SDK_VERSION="$(sed -n 's/^sdk_version = "\(.*\)"/\1/p' hulk.toml)"
docker run --rm -v "$PWD":/hulk -v nao-eph-cargo:/root/.cargo -w /hulk "ghcr.io/hulks/naosdk:$SDK_VERSION" /bin/sh -euc '
  . /naosdk/environment-setup-corei7-64-aldebaran-linux
  cargo build --release --target x86_64-aldebaran-linux-gnu -p hulk_nao -p nao_eph_voice
  rm -rf /tmp/robot && mkdir -p /tmp/robot/bin
  cp target/x86_64-aldebaran-linux-gnu/release/hulk_nao /tmp/robot/bin/hulk
  cp target/x86_64-aldebaran-linux-gnu/release/nao-eph-voice /tmp/robot/bin/
  cp -rL etc tools/robot/nao-eph-voice.service /tmp/robot/
  tar -C /tmp/robot -czf tools/robot/nao-eph-robot.tar.gz .
'
ls -lh tools/robot/nao-eph-robot.tar.gz
