// Ziehbarer Splitter zwischen Seitenleiste und Arbeitsfläche (Fastra-Vorbild).
// Die Breite liegt als CSS-Variable auf dem Wurzelelement und wird lokal
// gemerkt; sie ist reine Darstellung und berührt keine Sicherheitslogik.

import { $, readStoredValue, writeStoredValue } from "./ui-utils.js";

const STORAGE_KEY = "doppeldecker-sidebar-width";
const DEFAULT_WIDTH = 190;
const MIN_WIDTH = 150;
const MAX_WIDTH = 420;

/**
 * @param onResize Rückruf nach jeder Breitenänderung. Die Panes werden dadurch
 *                 schmaler oder breiter, weshalb die Listen neu gezeichnet werden
 *                 müssen — das Datumsformat hängt an der verfügbaren Spaltenbreite.
 */
export function initSplitter({ onResize = () => {} } = {}) {
  const splitter = $("#splitter");
  const root = document.documentElement;

  function clamp(width) {
    return Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, Math.round(width)));
  }

  function apply(width, persist = true) {
    const value = clamp(width);
    root.style.setProperty("--sidebar-width", `${value}px`);
    splitter.setAttribute("aria-valuemin", String(MIN_WIDTH));
    splitter.setAttribute("aria-valuemax", String(MAX_WIDTH));
    splitter.setAttribute("aria-valuenow", String(value));
    // Ohne lokale Persistenz gilt die Breite einfach nur für diese Sitzung.
    if (persist) writeStoredValue(STORAGE_KEY, String(value));
    onResize();
    return value;
  }

  function currentWidth() {
    const raw = parseInt(getComputedStyle(root).getPropertyValue("--sidebar-width"), 10);
    return Number.isFinite(raw) ? raw : DEFAULT_WIDTH;
  }

  const saved = parseInt(readStoredValue(STORAGE_KEY) || "", 10);
  apply(Number.isFinite(saved) ? saved : DEFAULT_WIDTH, false);

  splitter.addEventListener("pointerdown", event => {
    if (event.button !== 0) return;
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = currentWidth();
    splitter.classList.add("dragging");
    splitter.setPointerCapture(event.pointerId);
    const onMove = moveEvent => apply(startWidth + (moveEvent.clientX - startX), false);
    const onUp = () => {
      splitter.classList.remove("dragging");
      splitter.removeEventListener("pointermove", onMove);
      splitter.removeEventListener("pointerup", onUp);
      splitter.removeEventListener("pointercancel", onUp);
      apply(currentWidth());
    };
    splitter.addEventListener("pointermove", onMove);
    splitter.addEventListener("pointerup", onUp);
    splitter.addEventListener("pointercancel", onUp);
  });

  // Doppelklick stellt die Standardbreite wieder her.
  splitter.addEventListener("dblclick", () => apply(DEFAULT_WIDTH));

  // Tastaturbedienung gemäß separator-Rolle: Pfeile ändern die Breite schrittweise.
  splitter.addEventListener("keydown", event => {
    const step = event.shiftKey ? 40 : 10;
    if (event.key === "ArrowLeft") {
      apply(currentWidth() - step);
      event.preventDefault();
    } else if (event.key === "ArrowRight") {
      apply(currentWidth() + step);
      event.preventDefault();
    } else if (event.key === "Home") {
      apply(DEFAULT_WIDTH);
      event.preventDefault();
    }
  });
}
