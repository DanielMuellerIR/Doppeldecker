#!/bin/sh
set -eu
cd "$(dirname "$0")"
case "$(uname -s)" in
  Darwin) exec ./release "$@" ;;
  Linux) exec pnpm bundle:linux "$@" ;;
  *) echo 'Diese Plattform wird nicht unterstützt.' >&2; exit 1 ;;
esac
