#!/usr/bin/env python3
"""Lokaler Mock-Server für reproduzierbare Frontend-Tests ohne Tauri-Fenster."""

from __future__ import annotations

import argparse
import json
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FRONTEND = ROOT / "frontend"
PRODUCTION_CSP = json.loads((ROOT / "src-tauri" / "tauri.conf.json").read_text())["app"][
    "security"
]["csp"]

# Der Rust-Test prüft diese Fixture gegen die registrierten Command-Signaturen.
MOCK_TAURI = (
    b"const mockTauriCommandArgs = "
    + (ROOT / "tools" / "tests" / "tauri-command-args.json").read_bytes()
    + b";\nconst mockFavoriteWire = "
    + (ROOT / "tools" / "tests" / "favorite-wire.json").read_bytes()
    + b";\nconst mockSmallControlWire = "
    + (ROOT / "tools" / "tests" / "small-control-wire.json").read_bytes()
    + b";\n"
    + (ROOT / "tools" / "mock-tauri.js").read_bytes()
)


class Handler(SimpleHTTPRequestHandler):
    """Liefert echte Assets aus und injiziert nur die kleine Tauri-Testbrücke."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(FRONTEND), **kwargs)

    def end_headers(self) -> None:
        # Immer frische Assets: Der Testserver darf nie gecachte Module liefern,
        # sonst prüfen Preview und Suite versehentlich alten Frontend-Stand.
        self.send_header("Cache-Control", "no-store")
        self.send_header(
            "Content-Security-Policy",
            PRODUCTION_CSP,
        )
        super().end_headers()

    def do_GET(self) -> None:  # noqa: N802 - Signatur der Standardbibliothek
        request_path = self.path.split("?", 1)[0]
        if request_path == "/favicon.ico":
            self.send_response(204)
            self.end_headers()
            return
        if request_path == "/mock-tauri.js":
            body = MOCK_TAURI
            self.send_response(200)
            self.send_header("Content-Type", "text/javascript; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if request_path in ("/", "/index.html"):
            html = (FRONTEND / "index.html").read_text()
            html = html.replace(
                '<script type="module" src="main.js"></script>',
                '<script src="/mock-tauri.js"></script>'
                '<script type="module" src="main.js"></script>',
            )
            body = html.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8765)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"Mock-UI: http://127.0.0.1:{args.port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
