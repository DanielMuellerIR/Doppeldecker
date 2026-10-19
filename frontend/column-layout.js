// Spaltenbreiten der Pane-Listen: ziehbare Trenner im Listenkopf, je Pane und
// Spalte gemerkte Breite, und die daraus folgende Stufe des Datumsformats.
//
// Das ist reine Darstellung. Es berührt keine Sicherheitslogik, keinen Pfad und
// keine Sortierung — sortiert wird weiterhin ausschließlich über die Rohwerte
// (Bytes und mtime), unabhängig davon, wie breit eine Spalte gerade ist.

import { byteUnits } from "./byte-units.js";
import { $, readStoredValue, writeStoredValue } from "./ui-utils.js";

const STORAGE_PREFIX = "doppeldecker-col";
const MIN_WIDTH = 40;
const MAX_WIDTH = 420;
// Beim „an den Inhalt anpassen“ ein Haar Luft lassen, damit der breiteste Wert
// auch bei gebrochenen Layoutbreiten sicher vollständig sichtbar bleibt.
const FIT_PADDING = 2;

/**
 * Breiteste Variante des Größenformats aus `fmtEntrySize`. Das Kürzel hängt an der
 * gewählten Basis („MB" dezimal, „MiB" binär), deshalb ist das keine Konstante.
 */
export function entrySizeSample() {
  return `999,99 ${byteUnits().units[2]}`;
}

// Worst-Case-Muster der vier Datumsstufen aus `fmtEntryDate`. Ziffern werden
// tabellarisch gerendert, jede „8“ ist also so breit wie jede andere Ziffer;
// „September“ ist der längste deutsche Monatsname.
const DATE_SAMPLES = [
  "88.88.88",
  "88.88.88, 88:88",
  "88.88.8888, 88:88",
  "88. September 8888 um 88:88",
];

/** Muster der ausführlichsten Datumsstufe — Zielbreite beim Anpassen an den Inhalt. */
export const DATE_WIDEST_SAMPLE = DATE_SAMPLES[DATE_SAMPLES.length - 1];

/** Mindestbreite je Datumsstufe. Nur für Diagnose und Tests; die Anzeige selbst
 *  entscheidet über `dateStageForWidth`. */
export function DATE_STAGE_WIDTHS() {
  // Kein direkter Funktionsverweis: `Array.map` übergäbe den Index als zweites
  // Argument und würde ihn damit fälschlich als Messwurzel interpretieren.
  return DATE_SAMPLES.map(sample => measureText(sample));
}

const COLUMNS = {
  size: { variable: "--col-size-w", fallback: 64, sample: () => entrySizeSample() },
  kind: { variable: "--col-kind-w", fallback: 94, sample: () => "Symbolischer Link" },
  date: { variable: "--col-date-w", fallback: 108, sample: () => DATE_SAMPLES[2] },
  created: { variable: "--col-created-w", fallback: 108, sample: () => DATE_SAMPLES[2] },
  owner: { variable: "--col-owner-w", fallback: 86, sample: () => "Eigentümer" },
  group: { variable: "--col-group-w", fallback: 74, sample: () => "Gruppe" },
  permissions: { variable: "--col-permissions-w", fallback: 82, sample: () => "rwxrwxrwx" },
};

const VIEW_COLUMNS = [
  ["size", "size"],
  ["kind", "kind"],
  ["modified", "date"],
  ["created", "created"],
  ["owner", "owner"],
  ["group", "group"],
  ["permissions", "permissions"],
];

// Jede Pane darf eine andere Textgröße verwenden. Messfühler und Cache gehören
// deshalb zu ihrem Pane-Element; ändert sich dort die berechnete Schrift, wird der
// Cache automatisch verworfen.
const measurements = new WeakMap();

/**
 * Breite eines Zellentextes in Pixeln. Gemessen wird an einem unsichtbaren Element
 * mit exakt den Zellen-Schriftregeln (`.r-size`), damit Tabellenziffern und
 * Schriftschnitt genauso zählen wie in der echten Zeile.
 */
export function measureText(text, root = document.body) {
  let state = measurements.get(root);
  if (!state) {
    const probe = document.createElement("span");
    probe.className = "r-size measure-probe";
    probe.setAttribute("aria-hidden", "true");
    root.appendChild(probe);
    state = { probe, signature: "", measured: new Map() };
    measurements.set(root, state);
  }
  const style = getComputedStyle(state.probe);
  const signature = [
    style.fontFamily,
    style.fontSize,
    style.fontWeight,
    style.fontStyle,
    style.letterSpacing,
  ].join("|");
  if (signature !== state.signature) {
    state.signature = signature;
    state.measured.clear();
  }
  if (state.measured.has(text)) return state.measured.get(text);
  state.probe.textContent = text;
  const width = Math.ceil(state.probe.getBoundingClientRect().width);
  // Vor dem ersten Layout kann die Messung 0 ergeben; dann nichts merken, damit
  // der nächste Aufruf einen echten Wert liefert.
  if (width > 0) state.measured.set(text, width);
  return width;
}

function defaultWidth(key, root = document.body) {
  const sample = COLUMNS[key].sample();
  const width = measureText(sample, root);
  return Math.max(MIN_WIDTH, width > 0 ? width + FIT_PADDING : COLUMNS[key].fallback);
}

/**
 * Größte Datumsstufe, die in der angegebenen Breite vollständig darstellbar ist.
 * Passt nicht einmal die kürzeste Form, bleibt es bei Stufe 1 — abgeschnitten wird
 * nur im Extremfall, erfunden wird nie etwas.
 */
export function dateStageForWidth(width, root = document.body) {
  let stage = 1;
  for (let index = 0; index < DATE_SAMPLES.length; index += 1) {
    if (width >= measureText(DATE_SAMPLES[index], root)) stage = index + 1;
  }
  return stage;
}

/**
 * Verdrahtet die Spaltentrenner einer Pane und stellt die gemerkten Breiten her.
 *
 * @param side      "left" oder "right" — Breiten werden je Pane getrennt gemerkt.
 * @param paneEl    Wurzelelement der Pane; trägt die Breiten als CSS-Variablen.
 * @param onChange  Rückruf nach jeder Breitenänderung (Zeilen neu zeichnen).
 * @param fitWidth  Liefert die Wunschbreite einer Spalte für den Doppelklick.
 */
export function initPaneColumns(side, paneEl, { onChange, fitWidth }) {
  function storageKey(key) {
    return `${STORAGE_PREFIX}-${side}-${key}`;
  }

  function clamp(width) {
    return Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, Math.round(width)));
  }

  function currentWidth(key) {
    const raw = parseInt(getComputedStyle(paneEl).getPropertyValue(COLUMNS[key].variable), 10);
    return Number.isFinite(raw) ? raw : defaultWidth(key, paneEl);
  }

  function apply(key, width, { persist = true, notify = true } = {}) {
    const value = clamp(width);
    paneEl.style.setProperty(COLUMNS[key].variable, `${value}px`);
    const handle = $(`.col-resize[data-col="${key}"]`, paneEl);
    if (handle) {
      handle.setAttribute("aria-valuemin", String(MIN_WIDTH));
      handle.setAttribute("aria-valuemax", String(MAX_WIDTH));
      handle.setAttribute("aria-valuenow", String(value));
    }
    // Ohne lokale Persistenz gilt die Breite einfach nur für diese Sitzung.
    if (persist) writeStoredValue(storageKey(key), String(value));
    if (notify) onChange();
    return value;
  }

  /** Vom Nutzer gemerkte Breite dieser Spalte, sonst null. */
  function storedWidth(key) {
    const saved = parseInt(readStoredValue(storageKey(key)) || "", 10);
    return Number.isFinite(saved) ? saved : null;
  }

  for (const key of Object.keys(COLUMNS)) {
    apply(key, storedWidth(key) ?? defaultWidth(key, paneEl), { persist: false, notify: false });
  }

  for (const handle of paneEl.querySelectorAll(".col-resize")) {
    const key = handle.dataset.col;
    if (!COLUMNS[key]) continue;

    // Beide beweglichen Spalten sind rechtsbündig und wachsen nach links: Der
    // Trenner liegt an ihrer linken Kante, Ziehen nach links macht sie breiter.
    handle.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      const startX = event.clientX;
      const startWidth = currentWidth(key);
      handle.classList.add("dragging");
      handle.setPointerCapture(event.pointerId);
      const onMove = moveEvent => apply(key, startWidth - (moveEvent.clientX - startX), { persist: false });
      const onUp = () => {
        handle.classList.remove("dragging");
        handle.removeEventListener("pointermove", onMove);
        handle.removeEventListener("pointerup", onUp);
        handle.removeEventListener("pointercancel", onUp);
        apply(key, currentWidth(key));
      };
      handle.addEventListener("pointermove", onMove);
      handle.addEventListener("pointerup", onUp);
      handle.addEventListener("pointercancel", onUp);
    });

    // Doppelklick passt einmalig an den Inhalt an — eine Aktion, kein Dauerschalter.
    handle.addEventListener("dblclick", () => {
      const width = fitWidth(key);
      if (Number.isFinite(width) && width > 0) apply(key, width + FIT_PADDING);
    });

    // Tastaturbedienung gemäß separator-Rolle. Die Pfeiltaste bewegt den Trenner,
    // die Breite folgt ihm — nach links also breiter, wie beim Ziehen.
    handle.addEventListener("keydown", (event) => {
      const step = event.shiftKey ? 40 : 10;
      if (event.key === "ArrowLeft") {
        apply(key, currentWidth(key) + step);
        event.preventDefault();
      } else if (event.key === "ArrowRight") {
        apply(key, currentWidth(key) - step);
        event.preventDefault();
      } else if (event.key === "Home") {
        apply(key, defaultWidth(key, paneEl));
        event.preventDefault();
      }
    });
  }

  return {
    /**
     * Standardbreiten neu berechnen — nötig, wenn sich die Größeneinheit ändert
     * („999,99 MiB" braucht mehr Platz als „999,99 MB"). Selbst eingestellte Breiten
     * bleiben unangetastet: Wer gezogen hat, hat entschieden.
     */
    refreshDefaults() {
      for (const key of Object.keys(COLUMNS)) {
        if (storedWidth(key) === null) {
          apply(key, defaultWidth(key, paneEl), { persist: false, notify: false });
        }
      }
    },

    /** Blendet optionale Spalten samt Trenner aus und baut dasselbe Raster für
     * Kopf und Zeilen. Die Namensspalte bleibt immer sichtbar und flexibel. */
    setVisibility(columns) {
      const template = ["var(--col-icon-w)", "minmax(70px, 1fr)"];
      for (const [viewKey, key] of VIEW_COLUMNS) {
        const visible = Boolean(columns[viewKey]);
        const header = $(`.list-header .col-${key}`, paneEl);
        const handle = $(`.col-resize[data-col="${key}"]`, paneEl);
        if (header) header.hidden = !visible;
        if (handle) handle.hidden = !visible;
        if (visible) {
          template.push("var(--col-split-w)", `minmax(0, var(${COLUMNS[key].variable}))`);
        }
      }
      paneEl.style.setProperty("--pane-columns", template.join(" "));
    },

    /** Stufe eines Datumsformats aus der tatsächlich gerenderten Spaltenbreite. */
    dateStage(key = "date") {
      const header = $(`.list-header .col-${key}`, paneEl);
      if (!header || header.hidden) return 3;
      const width = header.getBoundingClientRect().width;
      return width > 0 ? dateStageForWidth(width, paneEl) : 3;
    },
  };
}
