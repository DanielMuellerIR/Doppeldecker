// Dünne, lazy Tauri-Brücke. Der globale Zugriff erfolgt erst beim Aufruf, damit ein
// fehlender Backend-Start als sichtbarer UI-Fehler erscheint statt das Modul zu stoppen.
export const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
// `event.listen` registriert ohne Ziel einen prozessweiten Listener. Bei mehreren
// Fenstern würde daher jedes Webview denselben Menübefehl und Queue-Abschluss
// empfangen. Das aktuelle Webview bindet den Listener an sein eigenes Label.
export const listen = (event, callback) => (
  window.__TAURI__.webview.getCurrentWebview().listen(event, callback)
);
