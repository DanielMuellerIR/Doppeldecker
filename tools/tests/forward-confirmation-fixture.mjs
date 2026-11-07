/** Große KeepBoth-Vorschau in der serialisierten Core-Form, ohne Dateiwirkung. */
export function forwardConfirmationFixture() {
  const binaryPath = path => path.split("/").map(segment => [...new TextEncoder().encode(segment)]);
  const identity = display_path => ({ provider: "local", identity: { display_path } });
  const effect = index => ({
    target_root: `/ziel-${index}`, plan: { copy_count: 1, update_count: 0, delete_count: 0,
      bytes_to_transfer: 12, metadata_report: { issues: [] } },
    irreversible_replacements: 0, irreversible_removals: 0,
    quarantined_replacements: 0, quarantined_removals: 0, limitations: [],
  });
  const effects = Array.from({ length: 24 }, (_, index) => effect(index));
  Object.assign(effects[22], { irreversible_replacements: 3, quarantined_replacements: 2,
    quarantined_removals: 4, quarantine_run: "fixture-quarantine" });
  Object.assign(effects[22].plan, { copy_count: 0, update_count: 5, delete_count: 4 });
  Object.assign(effects[23].plan, { copy_count: 0, delete_count: 7 });
  effects[23].irreversible_removals = 7;
  effects[23].plan.metadata_report.issues = [
    { path: "letzter-ordner/zeit.txt", class: "Mtime", kind: "Unsupported", reason: "Ziel unterstützt keine prüfbare Dateizeit" },
    { path: "letzter-ordner/rechte.txt", class: "Acl", kind: "IncompatibleAclModel", reason: "ACL-Modell ist nicht darstellbar <Details>" },
  ];
  const actions = Array.from({ length: 54 }, (_, index) => ({
    type: "conflict_copy", source_path: binaryPath(`konflikt-${index}/ä.txt`),
    target_path: binaryPath(`konflikt-${index}/ä-Kopie.txt`), source_side: "left", target_side: "right",
    expected_source: Array(32).fill(1), subtree: [],
  }));
  actions.push({ type: "delete", path: binaryPath("dauerhaft-entfernen.txt"), direction: "left_to_right", expected_target: Array(32).fill(2) });
  return { digest: Array(32).fill(42), effects, scope_warnings: [], limitations: [], recovery: null,
    plan: { actions, left_root_identity: identity("/paar-links"), right_root_identity: identity("/paar-rechts") } };
}
