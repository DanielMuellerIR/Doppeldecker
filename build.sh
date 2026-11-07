#!/bin/sh
# Lokale Pakete bauen; veröffentlichbare Linux-Pakete entstehen über release.sh.
set -eu
cd "$(dirname "$0")"
case "$(uname -s)" in
  Darwin) exec ./build-app "$@" ;;
  Linux)
    python3 tools/check-release.py
    python3 tools/build-release.py linux-local "$@"
    exec python3 tools/verify-release-privacy.py
    ;;
  *) echo 'Diese Plattform wird nicht unterstützt.' >&2; exit 1 ;;
esac
