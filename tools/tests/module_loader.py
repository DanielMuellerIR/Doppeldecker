"""Gemeinsamer Lader für ausführbare Python-Werkzeuge mit Bindestrich im Namen."""

from __future__ import annotations

import importlib.util
from pathlib import Path
from types import ModuleType


ROOT = Path(__file__).resolve().parents[2]


def load_tool_module(module_name: str, filename: str) -> ModuleType:
    """Lädt genau ein Werkzeug unter einem testspezifisch eindeutigen Modulnamen."""
    spec = importlib.util.spec_from_file_location(module_name, ROOT / "tools" / filename)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"Werkzeugmodul kann nicht geladen werden: {filename}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
