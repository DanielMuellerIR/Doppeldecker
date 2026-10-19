// Theme und Plattform vor dem ersten Paint setzen. So gibt es beim Start weder
// einen hellen Blitz im Dark Mode noch einen sichtbaren Layout-Wechsel.
// Eigene Datei statt Inline-Script, damit die strikte Content-Security-Policy
// (script-src 'self', kein 'unsafe-inline') greifen kann; synchron im <head>
// geladen bleibt die Reihenfolge identisch zum früheren Inline-Block.
(() => {
  const root = document.documentElement;
  const platform = /Mac/i.test(navigator.platform) ? "macos" : "linux";
  let theme = "auto";
  try {
    theme = localStorage.getItem("doppeldecker-theme") || "auto";
  } catch (_) {
    // localStorage kann in besonders restriktiven WebViews gesperrt sein.
  }
  if (!["auto", "light", "dark"].includes(theme)) theme = "auto";
  const dark = theme === "dark" ||
    (theme === "auto" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  root.dataset.platform = platform;
  root.dataset.theme = theme;
  root.dataset.resolvedTheme = dark ? "dark" : "light";
})();
