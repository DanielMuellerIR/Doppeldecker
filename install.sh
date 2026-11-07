#!/bin/sh
set -eu
cd "$(dirname "$0")"
case "$(uname -s)" in
  Darwin) exec ./install "$@" ;;
  Linux) exec python3 tools/install-linux.py "$@" ;;
  *) echo 'Diese Plattform wird nicht unterstützt.' >&2; exit 1 ;;
esac
