// Reiner, gemeinsam verwendeter Darstellungszustand für Ordner und Tabs.
export const COLUMN_KEYS = ["size", "kind", "modified", "created", "owner", "group", "permissions"];
export const BUILTIN_DEFAULTS = Object.freeze({
  columns: Object.freeze({
    size: true,
    kind: false,
    modified: true,
    created: false,
    owner: false,
    group: false,
    permissions: false,
  }),
  foldersFirst: true,
  foldersAlphabetical: true,
  boldFolders: true,
  zebraRows: false,
  relativeDates: false,
  showHidden: false,
  density: "compact",
  textSize: "m",
});

export function copyOptions(value) {
  return { ...value, columns: { ...value.columns } };
}

function bool(value, fallback) {
  return typeof value === "boolean" ? value : fallback;
}

/** Fremde oder alte localStorage-Werte dürfen keine unbekannten CSS-Zustände
 * erzeugen. Jedes Feld wird deshalb einzeln gegen den eingebauten Satz geprüft. */
export function sanitizeViewOptions(value, fallback = BUILTIN_DEFAULTS) {
  const source = value && typeof value === "object" ? value : {};
  const sourceColumns = source.columns && typeof source.columns === "object"
    ? source.columns
    : {};
  const columns = {};
  for (const key of COLUMN_KEYS) {
    columns[key] = bool(sourceColumns[key], fallback.columns[key]);
  }
  return {
    columns,
    foldersFirst: bool(source.foldersFirst, fallback.foldersFirst),
    foldersAlphabetical: bool(source.foldersAlphabetical, fallback.foldersAlphabetical),
    boldFolders: bool(source.boldFolders, fallback.boldFolders),
    zebraRows: bool(source.zebraRows, fallback.zebraRows),
    relativeDates: bool(source.relativeDates, fallback.relativeDates),
    showHidden: bool(source.showHidden, fallback.showHidden),
    density: ["compact", "normal"].includes(source.density)
      ? source.density
      : fallback.density,
    textSize: ["s", "m", "l"].includes(source.textSize)
      ? source.textSize
      : fallback.textSize,
  };
}
