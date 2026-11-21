// Dünne, lazy Tauri-Brücke. Der globale Zugriff erfolgt erst beim Aufruf, damit ein
// fehlender Backend-Start als sichtbarer UI-Fehler erscheint statt das Modul zu stoppen.
/** @typedef {import("./ipc-results.js").GeneratedIpcResults} GeneratedIpcResults */
/** @typedef {{paths: string[], cut: boolean, version: string}} FileClipboard */
/** @typedef {GeneratedIpcResults & {read_file_clipboard: FileClipboard}} IpcResults */
/** @typedef {{
 *   "queue-updated": GeneratedIpcResults["list_operation_queue"],
 *   "update-progress": import("./update-wire.js").UpdateProgressWire,
 *   "batch-rename-progress": import("./batch-rename-wire.js").BatchRenameProgressWire,
 *   "desktop-preferences-changed": GeneratedIpcResults["desktop_preferences"],
 * }} IpcEvents */

/**
 * @template {string} Command
 * @param {Command} command
 * @param {unknown} [args]
 * @returns {Promise<Command extends keyof IpcResults ? IpcResults[Command] : any>}
 */
export const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
// `event.listen` registriert ohne Ziel einen prozessweiten Listener. Bei mehreren
// Fenstern würde daher jedes Webview denselben Menübefehl und Queue-Abschluss
// empfangen. Das aktuelle Webview bindet den Listener an sein eigenes Label.
/**
 * @template {string} EventName
 * @param {EventName} event
 * @param {(message: {payload: EventName extends keyof IpcEvents ? IpcEvents[EventName] : any}) => void} callback
 */
export const listen = (event, callback) => (
  window.__TAURI__.webview.getCurrentWebview().listen(event, callback)
);

/** @returns {{onMoved: (callback: () => void) => Promise<() => void>, onResized: (callback: () => void) => Promise<() => void>} | undefined} */
export const currentTauriWindow = () => window.__TAURI__?.window?.getCurrentWindow?.();
