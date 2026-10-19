import { writeStoredValue } from "./ui-utils.js";

// Darstellung ohne Abhängigkeit vom übrigen App-Zustand. Theme-Boot setzt die erste
// Wahl bereits vor dem Rendern; dieses Modul übernimmt danach Persistenz und
// Livewechsel. Umgeschaltet wird ausschließlich über das native Menü „Darstellung".

const STORAGE_KEY = "doppeldecker-theme";
const systemDarkQuery = window.matchMedia("(prefers-color-scheme: dark)");
let preference = document.documentElement.dataset.theme || "auto";

export function setTheme(nextPreference, persist = true) {
  if (!["auto", "light", "dark"].includes(nextPreference)) nextPreference = "auto";
  preference = nextPreference;
  const resolved = preference === "auto"
    ? (systemDarkQuery.matches ? "dark" : "light")
    : preference;
  const root = document.documentElement;
  root.dataset.theme = preference;
  root.dataset.resolvedTheme = resolved;
  // Die Darstellung funktioniert auch ohne lokale Persistenz weiter.
  if (persist) writeStoredValue(STORAGE_KEY, preference);
}

/** Hält den „Auto"-Modus live am System, ohne eigenes UI-Element. */
export function initTheme() {
  setTheme(preference, false);
  // Ältere WebViews kennen nur addListener; der Fallback hält ältere WebKitGTK-
  // Versionen bedienbar, ohne die moderne API doppelt anzumelden.
  const onSystemThemeChange = () => {
    if (preference === "auto") setTheme("auto", false);
  };
  if (systemDarkQuery.addEventListener) {
    systemDarkQuery.addEventListener("change", onSystemThemeChange);
  } else {
    systemDarkQuery.addListener(onSystemThemeChange);
  }
}

export function themePreference() { return preference; }
