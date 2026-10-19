#!/usr/bin/env python3
"""Führt alle headless Code-Gates aus AGENTS.md („Bauen und testen") gebündelt aus.

Hintergrund: Die Gates liefen bisher nur, wenn jemand daran dachte — so blieb ein
roter `cargo fmt`-Check unbemerkt (backlog.md Punkt 3). Dieses Skript macht die
Liste als ein Kommando ausführbar, mit sauberen Exit-Codes und auf Wunsch
maschinenlesbarer Ausgabe. Es verändert nichts am Repo-Zustand; einzige Ausnahme
sind ignorierte, regenerierbare Artefakte: `pnpm-install` stellt `node_modules/`
aus dem gelockten Lockfile her, und die unten beschriebene Cache-Grenze kann
`target/debug` bereinigen.

Bewusst NICHT enthalten:
- `./tools/run-test-sandbox.sh` startet die GUI und bleibt ein manueller Schritt.
- Release-Gates laufen weiterhin über `pnpm release:check` und die `bundle:*`-Ketten.
- Ein Git-Hook ist eine Konfigurationsänderung und wird hier absichtlich nicht
  gesetzt (erst Automations-Doku konsultieren und Rücksprache halten).

Bedienung (Konvention wie die Headless-CLI: `--json` schreibt JSON Lines auf
stdout, menschenlesbarer Fortschritt bleibt auf stderr):

    python3 tools/check-gates.py             # alle Gates, menschenlesbar
    python3 tools/check-gates.py --json      # zusätzlich JSONL-Ereignisse auf stdout
    python3 tools/check-gates.py --list      # verfügbare Gates anzeigen
    python3 tools/check-gates.py --only fmt --only clippy
    python3 tools/check-gates.py --skip test-ui --fail-fast

Exit-Codes: 0 = alle ausgeführten Gates und die gegebenenfalls aktive Cache-Prüfung
grün, 1 = mindestens ein Gate oder die Cache-Prüfung rot, 2 = Bedienfehler
(unbekanntes Gate oder unbekannte Option).

Die vollständige Ausgabe jedes Gates landet in `target/gate-logs/<gate>.log`
(von Git ignoriert); bei einem roten Gate zeigt stderr die letzten Zeilen davon.
Die Logs enthalten nur Compiler-/Testausgabe — Gates mit Credential-Ausgabe
dürften hier nicht aufgenommen werden (Secrets-Regel in AGENTS.md).

Nach einem vollständig grünen Lauf misst das Skript den Debug-Buildcache. Überschreitet
`target/debug` die empirisch festgelegte Grenze, entfernt `cargo clean --profile dev`
nur diesen regenerierbaren Profilbaum. Release-Artefakte, Gate-Logs und Teilgate-Läufe
bleiben unberührt.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LOG_DIR = ROOT / "target" / "gate-logs"
DEBUG_CACHE_DIR = ROOT / "target" / "debug"
DEBUG_CACHE_LIMIT_GIB = 6
KIB_PER_GIB = 1024 * 1024

# Zeilen, die bei einem roten Gate als Schnelldiagnose auf stderr erscheinen.
FAILURE_TAIL_LINES = 25

# Reihenfolge wie in AGENTS.md „Bauen und testen": billige Prüfungen zuerst,
# damit ein roter fmt-Check nicht erst nach Minuten Testlauf auffällt.
GATES: list[tuple[str, list[str]]] = [
    ("fmt", ["cargo", "fmt", "--all", "--", "--check"]),
    ("pnpm-install", ["pnpm", "install", "--frozen-lockfile"]),
    (
        "test-tools",
        [
            "python3",
            "-m",
            "unittest",
            "discover",
            "-s",
            "tools/tests",
            "-p",
            "test_*.py",
        ],
    ),
    ("test-core", ["cargo", "test", "-p", "doppeldecker-core"]),
    ("test-cli", ["cargo", "test", "-p", "doppeldecker-cli"]),
    ("test-tauri", ["cargo", "test", "-p", "doppeldecker"]),
    ("clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
    # Die Fuzz-Targets liegen bewusst außerhalb des Workspace und werden von keinem
    # anderen Gate übersetzt. Ohne diese Prüfung veraltet ihr Stand still, sobald
    # sich eine Core-Schnittstelle ändert. `--locked` macht zusätzlich sichtbar,
    # wenn `fuzz/Cargo.lock` nicht mehr zu den Abhängigkeiten des Kerns passt —
    # genau diese Abweichung war am 2026-08-19 unbemerkt entstanden.
    # `--all-targets` ist nicht kosmetisch: Ohne den Schalter waehlt `cargo check`
    # nur Lib und Bins, und `fuzz/tests/seed_corpus.rs` wird nie uebersetzt. Genau
    # dieser Test haelt fest, dass die versionierten Seeds unter `fuzz/seeds/`
    # schema- und digestgueltig bleiben; ohne ihn faellt eine Schemaaenderung erst
    # bei einem echten Fuzz-Lauf mit unbrauchbaren Seeds auf.
    (
        "fuzz-check",
        [
            "cargo",
            "check",
            "--manifest-path",
            "fuzz/Cargo.toml",
            "--locked",
            "--all-targets",
        ],
    ),
    ("test-ui", ["pnpm", "test:ui"]),
]

# Auch ein Teilgate muss seine eigenen Laufzeitvoraussetzungen herstellen. Die
# Tooltests parsen die CI-YAML-Datei mit dem gelockten Node-Paket ``yaml``.
GATE_DEPENDENCIES: dict[str, tuple[str, ...]] = {
    "test-tools": ("pnpm-install",),
    "test-ui": ("pnpm-install",),
}


def select_gates(only: list[str], skip: list[str]) -> list[tuple[str, list[str]]]:
    """Wählt Gates samt Voraussetzungen in der festgelegten Reihenfolge aus."""
    selected = set(only) if only else {name for name, _ in GATES}
    # Ausgelassene Top-Level-Gates dürfen ihre Voraussetzungen nicht mehr in die
    # Closure ziehen. Eine weiterhin ausgewählte Prüfung, die selbst von einem
    # Skip abhängt, wird in der Schleife unverändert als Bedienfehler abgelehnt.
    selected.difference_update(skip)
    pending = list(selected)
    while pending:
        name = pending.pop()
        for dependency in GATE_DEPENDENCIES.get(name, ()):
            if dependency in skip:
                raise ValueError(
                    f"Gate {name} benötigt das ausgelassene Gate {dependency}"
                )
            if dependency not in selected:
                selected.add(dependency)
                pending.append(dependency)
    return [(name, command) for name, command in GATES if name in selected]


def emit_json(enabled: bool, event: dict) -> None:
    """Schreibt genau eine JSON-Zeile auf stdout, wenn --json aktiv ist."""
    if enabled:
        print(json.dumps(event, ensure_ascii=False), flush=True)


def human(message: str, json_mode: bool) -> None:
    """Menschenlesbare Ausgabe: im JSON-Modus auf stderr, sonst auf stdout.

    So bleibt stdout im JSON-Modus reines JSONL und damit sicher parsebar —
    dieselbe Trennung, die auch die Headless-CLI einhält.
    """
    print(message, file=sys.stderr if json_mode else sys.stdout, flush=True)


def measure_directory_kib(path: Path) -> int:
    """Misst tatsächlich belegte Blöcke, damit Hardlinks nicht mehrfach zählen."""
    if not path.exists():
        return 0
    completed = subprocess.run(
        ("du", "-sk", str(path)),
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or f"rc={completed.returncode}"
        raise RuntimeError(f"Cachegröße konnte nicht gemessen werden: {detail}")
    try:
        return int(completed.stdout.split(maxsplit=1)[0])
    except (IndexError, ValueError) as error:
        raise RuntimeError("Cachegröße lieferte keine KiB-Zahl.") from error


def clean_debug_profile() -> int:
    """Entfernt nur Debug-/Testartefakte; Release und Gate-Logs bleiben erhalten."""
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    log_path = LOG_DIR / "debug-cache-clean.log"
    with log_path.open("wb") as log_file:
        return run_command(("cargo", "clean", "--profile", "dev"), log_file)


def run_command(command, log_file) -> int:
    """Ein Abbruch beendet auch die von diesem Aufruf gestarteten Nachkommen."""
    interrupted = None
    child = None

    def request_stop(signum, _frame):
        nonlocal interrupted
        interrupted = interrupted or signum

    def stop_child():
        if child is None:
            return
        # Die eigene Sitzung begrenzt beide Signale auf genau diesen Prüfbaum.
        # Auch Nachkommen eines bereits beendeten direkten Kindes gehören dazu.
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        finally:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()

    previous = {
        signum: signal.signal(signum, request_stop)
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    }
    try:
        child = subprocess.Popen(
            command, cwd=ROOT, stdout=log_file, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        while True:
            try:
                rc = child.wait(timeout=0.1)
            except subprocess.TimeoutExpired:
                rc = None
            if interrupted is not None:
                raise SystemExit(128 + interrupted)
            if rc is not None:
                return rc
    except BaseException:
        stop_child()
        raise
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def enforce_debug_cache_limit(
    json_mode: bool,
    *,
    measure_fn=None,
    clean_fn=None,
) -> dict:
    """Hält den Cache nach vollständigen Gates unter der belegten Obergrenze."""
    measure = measure_fn or measure_directory_kib
    clean = clean_fn or clean_debug_profile
    limit_kib = DEBUG_CACHE_LIMIT_GIB * KIB_PER_GIB
    before_kib = measure(DEBUG_CACHE_DIR)
    before_gib = round(before_kib / KIB_PER_GIB, 2)

    if before_kib <= limit_kib:
        human(
            f"Buildcache grün: {before_gib:.2f} GiB / {DEBUG_CACHE_LIMIT_GIB} GiB",
            json_mode,
        )
        return {
            "event": "debug-cache",
            "status": "within-limit",
            "before_gib": before_gib,
            "after_gib": before_gib,
            "limit_gib": DEBUG_CACHE_LIMIT_GIB,
            "rc": 0,
        }

    human(
        f"Buildcache {before_gib:.2f} GiB überschreitet {DEBUG_CACHE_LIMIT_GIB} GiB; "
        "Debug-Profil wird bereinigt.",
        json_mode,
    )
    rc = clean()
    after_kib = measure(DEBUG_CACHE_DIR)
    after_gib = round(after_kib / KIB_PER_GIB, 2)
    status = "cleaned" if rc == 0 and after_kib <= limit_kib else "fail"
    if status == "cleaned":
        human(f"  ✓ Buildcache bereinigt: {after_gib:.2f} GiB", json_mode)
    else:
        human(
            "  ✗ Buildcache-Bereinigung fehlgeschlagen "
            f"(rc={rc}, verbleibend={after_gib:.2f} GiB)",
            json_mode,
        )
    return {
        "event": "debug-cache",
        "status": status,
        "before_gib": before_gib,
        "after_gib": after_gib,
        "limit_gib": DEBUG_CACHE_LIMIT_GIB,
        "rc": 0 if status == "cleaned" else 1,
        "log": "target/gate-logs/debug-cache-clean.log",
    }


def signal_note(rc: int) -> str | None:
    """Beschreibt einen Signalabbruch, sonst `None`.

    POSIX meldet einen per Signal beendeten Prozess über ein negatives
    Rückgabe-`rc`. Das ist etwas anderes als ein rotes Gate: Die Prüfung hat gar
    kein Ergebnis geliefert, sondern wurde von außen abgebrochen — etwa durch ein
    Timeout der aufrufenden Umgebung. Ohne diesen Hinweis sucht man den Fehler in
    den Tests, obwohl keiner fehlgeschlagen ist.
    """
    if rc >= 0:
        return None
    try:
        return signal.Signals(-rc).name
    except ValueError:
        return f"Signal {-rc}"


def run_gate(name: str, command: list[str], json_mode: bool) -> dict:
    """Führt ein einzelnes Gate aus und liefert das Ergebnis als Datensatz.

    Die Gate-Ausgabe wird nicht live durchgereicht, sondern vollständig in eine
    Logdatei geschrieben: stdout bleibt maschinenlesbar, und bei Erfolg
    interessiert die Compiler-Ausgabe normalerweise nicht.
    """
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    log_path = LOG_DIR / f"{name}.log"
    human(f"→ {name}: {' '.join(command)}", json_mode)
    started = time.monotonic()
    with log_path.open("wb") as log_file:
        try:
            # cwd=ROOT, damit das Skript aus jedem Verzeichnis heraus funktioniert.
            rc = run_command(command, log_file)
        except FileNotFoundError:
            # Werkzeug selbst fehlt (z. B. pnpm nicht installiert): als rotes Gate
            # bilanzieren statt mit Traceback abzubrechen.
            log_file.write(f"Programm nicht gefunden: {command[0]}\n".encode())
            rc = 127
    duration = round(time.monotonic() - started, 1)
    status = "pass" if rc == 0 else "fail"
    marker = "✓" if rc == 0 else "✗"
    aborted_by = signal_note(rc)
    abort_hint = f", abgebrochen durch {aborted_by}" if aborted_by else ""
    human(f"  {marker} {name}: {status} (rc={rc}{abort_hint}, {duration}s)", json_mode)
    if rc != 0:
        if aborted_by:
            print(
                f"    Von außen durch {aborted_by} beendet: Das Gate hat kein Ergebnis"
                " geliefert, es ist kein Test fehlgeschlagen.",
                file=sys.stderr,
            )
        # Schnelldiagnose: die letzten Logzeilen direkt anzeigen, der Rest steht
        # vollständig in der Logdatei.
        tail = log_path.read_text(errors="replace").splitlines()[-FAILURE_TAIL_LINES:]
        for line in tail:
            print(f"    {line}", file=sys.stderr)
        print(f"    (vollständig: {log_path})", file=sys.stderr)
    record = {
        "event": "gate",
        "gate": name,
        "command": command,
        "rc": rc,
        "duration_s": duration,
        "status": status,
        "log": str(log_path.relative_to(ROOT)),
    }
    if aborted_by:
        record["aborted_by"] = aborted_by
    return record


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Alle headless Code-Gates aus AGENTS.md gebündelt ausführen."
    )
    parser.add_argument("--json", action="store_true", help="JSON Lines auf stdout ausgeben")
    parser.add_argument("--list", action="store_true", help="verfügbare Gates anzeigen und beenden")
    parser.add_argument("--only", action="append", default=[], metavar="GATE",
                        help="nur dieses Gate ausführen (mehrfach angebbar)")
    parser.add_argument("--skip", action="append", default=[], metavar="GATE",
                        help="dieses Gate auslassen (mehrfach angebbar)")
    parser.add_argument("--fail-fast", action="store_true",
                        help="nach dem ersten roten Gate abbrechen")
    args = parser.parse_args()

    known = {name for name, _ in GATES}
    if args.list:
        for name, command in GATES:
            print(f"{name}: {' '.join(command)}")
        return 0
    for requested in [*args.only, *args.skip]:
        if requested not in known:
            print(f"Unbekanntes Gate: {requested} (verfügbar: {', '.join(sorted(known))})",
                  file=sys.stderr)
            return 2

    try:
        selected = select_gates(args.only, args.skip)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2

    results = []
    for name, command in selected:
        result = run_gate(name, command, args.json)
        emit_json(args.json, result)
        results.append(result)
        if args.fail_fast and result["rc"] != 0:
            break

    gate_failures = [r["gate"] for r in results if r["rc"] != 0]
    failed = list(gate_failures)
    skipped = sorted(known - {r["gate"] for r in results})
    cache_result = None
    if not failed and not skipped:
        try:
            cache_result = enforce_debug_cache_limit(args.json)
        except RuntimeError as error:
            human(f"✗ Buildcache-Prüfung fehlgeschlagen: {error}", args.json)
            cache_result = {
                "event": "debug-cache",
                "status": "fail",
                "rc": 1,
                "error": str(error),
            }
        emit_json(args.json, cache_result)
        if cache_result["rc"] != 0:
            failed.append("debug-cache-limit")
    rc = 1 if failed else 0
    emit_json(args.json, {
        "event": "summary",
        "passed": len(results) - len(gate_failures),
        "failed": failed,
        "skipped": skipped,
        "rc": rc,
    })
    if failed:
        human(f"ROT: {', '.join(failed)}", args.json)
    else:
        note = f" (ausgelassen: {', '.join(skipped)})" if skipped else ""
        human(f"Alle ausgeführten Gates grün{note}", args.json)
    return rc


if __name__ == "__main__":
    sys.exit(main())
