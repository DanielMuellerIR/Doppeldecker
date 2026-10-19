//! Geschlossene Ausführungsfreigaben und Pfadschranken des Sync-Kerns.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::real_mode::RealRunPermit;
use crate::vfs::Vfs;

use super::SyncPlan;

/// Nur dieser exakte Marker schaltet eine Wegwerf-Sandbox frei. Die Prüfung lebt im
/// Kern, damit auch andere Frontends die Sicherheitsgrenze nicht umgehen.
pub const TEST_SANDBOX_MARKER: &str = ".doppeldecker-test-sandbox";
pub const TEST_SANDBOX_MARKER_CONTENT: &str = "DOPPELDECKER TEST SANDBOX\n";

/// Kanonisiert und prüft eine Test-Sandbox vollständig. Ein Environment-Wert allein
/// ist nie eine Schreibfreigabe; gefährliche Eltern, Cloud- und Git-Pfade bleiben tabu.
pub fn validate_test_sandbox_root(raw: &Path) -> io::Result<PathBuf> {
    if !raw.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Test-Sandbox muss absolut sein",
        ));
    }
    let root = std::fs::canonicalize(raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Ungültige Test-Sandbox „{}“: {error}", raw.display()),
        )
    })?;

    if root == Path::new("/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Die Dateisystemwurzel darf niemals Test-Sandbox sein",
        ));
    }
    if let Ok(home_raw) = std::env::var("HOME") {
        // Ein gesetztes, aber nicht auflösbares HOME ist kein Freibrief: Dann lässt
        // sich gerade nicht ausschließen, dass die Sandbox im Benutzerordner liegt.
        let home = std::fs::canonicalize(&home_raw).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "HOME ({home_raw}) ist nicht auflösbar; Test-Sandbox bleibt gesperrt: {error}"
                ),
            )
        })?;
        if root == home || home.starts_with(&root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Benutzerordner oder einer seiner Eltern darf niemals Test-Sandbox sein",
            ));
        }
    }
    let cloud_component = root.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_lowercase();
        ["nextcloud", "dropbox", "onedrive", "google drive", "icloud"]
            .iter()
            .any(|cloud| name.contains(cloud))
    });
    if cloud_component {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cloud-Sync-Ordner dürfen niemals Test-Sandbox sein",
        ));
    }
    // `exists()` liefert bei jedem Fehler `false` — bei einem nicht durchsuchbaren
    // Vorfahren, bei ELOOP und bei einem toten `.git`-Symlink. Die Sandbox hätte dann
    // als git-frei gegolten. Der Realmodus prüft dieselbe Frage längst
    // fehlererhaltend; hier gilt jetzt derselbe Massstab: Nur ein echtes `NotFound`
    // heisst „kein Repository", jeder andere Fehler sperrt.
    for ancestor in root.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Ein Git-Repository darf niemals Test-Sandbox sein",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                    "Git-Prüfung für {} blieb unbeantwortet; Test-Sandbox bleibt gesperrt: {error}",
                    ancestor.display()
                ),
                ))
            }
        }
    }

    let marker = root.join(TEST_SANDBOX_MARKER);
    let marker_content = std::fs::read_to_string(&marker).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Test-Sandbox nicht freigeschaltet: Marker {} fehlt",
                marker.display()
            ),
        )
    })?;
    if marker_content != TEST_SANDBOX_MARKER_CONTENT {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Ungültiger Test-Sandbox-Marker: {}", marker.display()),
        ));
    }
    Ok(root)
}

/// Geschlossene Ausführungsart. Ein fehlender Sandboxpfad ist damit nie wieder ein
/// implizites „real ausführen". Das Headless-Permit ist nur über den ausdrücklich
/// benannten Debug-Testkonstruktor erhältlich. `Real` kann ausschließlich der lokale
/// Validator mit einem nicht serialisierbaren, einmaligen Permit erzeugen.
#[derive(Debug)]
pub enum ExecutionMode {
    DryRun,
    TestSandbox(TestSandboxPermit),
    Real(Box<RealRunPermit>),
    #[cfg(debug_assertions)]
    HeadlessTest(HeadlessTestPermit),
}

#[derive(Debug)]
pub struct TestSandboxPermit {
    root: PathBuf,
    identity: (u64, u64),
}

#[derive(Debug)]
#[cfg(debug_assertions)]
pub struct HeadlessTestPermit {
    _private: (),
}

/// Sicherheitsvorgaben für einen Ausführungslauf — die Schranke gegen Datenverlust.
#[derive(Debug)]
pub struct Safety {
    execution: ExecutionMode,
    /// Laufbezogener Wiederherstellungsordner. Bestehende oder zu löschende Zielobjekte
    /// werden dorthin verschoben, bevor ihr sichtbarer Pfad verändert wird.
    pub quarantine: Option<PathBuf>,
    /// Harte Obergrenze für rekursiv zu entfernende Einträge.
    pub max_delete_entries: Option<u64>,
    /// Harte Obergrenze als Anteil des bisherigen Zielbestands (0.0 bis 1.0).
    pub max_delete_fraction: Option<f64>,
    /// Leere Quellen dürfen standardmäßig niemals einen nichtleeren Zielbaum leeren.
    pub allow_empty_source_mirror: bool,
}

impl Default for Safety {
    fn default() -> Self {
        Self {
            execution: ExecutionMode::DryRun,
            quarantine: None,
            max_delete_entries: Some(100),
            max_delete_fraction: Some(0.25),
            allow_empty_source_mirror: false,
        }
    }
}

impl Safety {
    /// Markierte Test-Sandbox als einzige produktive Schreibart der aktuellen Version.
    /// Der Kern prüft Marker und Gefahrenpfade selbst, bevor er das nicht serialisierbare
    /// Permit erzeugt.
    pub fn test_sandbox(root: PathBuf, anchor: &dyn Vfs) -> io::Result<Self> {
        let root = validate_test_sandbox_root(&root)?;
        if anchor.anchored_root() != Some(root.as_path()) || !anchor.anchor_is_current()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Test-Sandbox-Permit benötigt den passenden gehaltenen Root-Deskriptor",
            ));
        }
        let identity = anchor.anchored_identity().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Test-Sandbox-Root besitzt keine stabile lokale Identität",
            )
        })?;
        let mut marker_content = String::new();
        anchor
            .open_read(&root.join(TEST_SANDBOX_MARKER))?
            .read_to_string(&mut marker_content)?;
        if marker_content != TEST_SANDBOX_MARKER_CONTENT {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Test-Sandbox-Marker stimmt am gehaltenen Root-Handle nicht überein",
            ));
        }
        Ok(Self {
            execution: ExecutionMode::TestSandbox(TestSandboxPermit { root, identity }),
            ..Self::default()
        })
    }

    /// Explizite Mutationsfreigabe für isolierte Headless-Tests. Produktionscode darf
    /// stattdessen nur `test_sandbox` verwenden; die Methode ist bewusst unmissverständlich
    /// benannt und erzeugt kein serialisierbares Frontend-Token.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn headless_test() -> Self {
        Self {
            execution: ExecutionMode::HeadlessTest(HeadlessTestPermit { _private: () }),
            ..Self::default()
        }
    }

    /// Ausschließlich der Realmodus-Validator darf einen bestätigten Lauf in die
    /// geschlossene Ausführungsart überführen.
    pub(crate) fn real(permit: RealRunPermit, quarantine: PathBuf) -> Self {
        Self {
            execution: ExecutionMode::Real(Box::new(permit)),
            quarantine: Some(quarantine),
            max_delete_entries: Some(50),
            max_delete_fraction: Some(0.05),
            allow_empty_source_mirror: false,
        }
    }

    pub(super) fn is_dry_run(&self) -> bool {
        matches!(self.execution, ExecutionMode::DryRun)
    }

    pub(super) fn sandbox(&self) -> Option<&Path> {
        match &self.execution {
            ExecutionMode::TestSandbox(permit) => Some(&permit.root),
            ExecutionMode::Real(_) => None,
            ExecutionMode::DryRun => None,
            #[cfg(debug_assertions)]
            ExecutionMode::HeadlessTest(_) => None,
        }
    }

    fn sandbox_identity(&self) -> Option<(u64, u64)> {
        match &self.execution {
            ExecutionMode::TestSandbox(permit) => Some(permit.identity),
            ExecutionMode::Real(_) => None,
            ExecutionMode::DryRun => None,
            #[cfg(debug_assertions)]
            ExecutionMode::HeadlessTest(_) => None,
        }
    }

    pub(crate) fn real_permit(&self) -> Option<&RealRunPermit> {
        match &self.execution {
            ExecutionMode::Real(permit) => Some(permit.as_ref()),
            _ => None,
        }
    }

    pub(super) fn local_execution(&self) -> Option<&crate::real_mode::LocalSyncExecution> {
        self.real_permit().and_then(RealRunPermit::local_execution)
    }

    pub(super) fn irreversible_replacements(&self) -> bool {
        self.local_execution()
            .is_some_and(|p| p.irreversible_replacements > 0)
    }

    #[cfg(test)]
    pub(crate) fn real_permit_mut(&mut self) -> Option<&mut RealRunPermit> {
        match &mut self.execution {
            ExecutionMode::Real(permit) => Some(permit.as_mut()),
            _ => None,
        }
    }
}

/// Sicherheitsvorgaben prüfen, bevor auch nur ein Zielpfad geöffnet wird.
pub(super) fn validate_safety(
    src: &dyn Vfs,
    src_root: &Path,
    dst: &dyn Vfs,
    dst_root: &Path,
    plan: &SyncPlan,
    safety: &Safety,
) -> io::Result<()> {
    if let Some(permit) = safety.real_permit() {
        permit.consume_and_validate(src, src_root, dst, dst_root, plan)?;
        if safety.quarantine.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Realmodus benötigt eine gebundene Quarantäne",
            ));
        }
    }
    if let Some(root) = safety.sandbox() {
        let canon_root = canonical_existing(root)?;
        validate_anchor_identity(src, dst, safety)?;
        let canon_src = canonical_existing(src_root)?;
        let canon_dst = canonical_existing(dst_root)?;
        // Das Sandbox-Root selbst bleibt immer tabu. Dadurch kann die Quarantäne als
        // geschützter Geschwisterordner neben dem eigentlichen Ziel liegen.
        if canon_src == canon_root || !canon_src.starts_with(&canon_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Sicherheits-Sandbox aktiv: Quelle „{}“ muss ein Unterordner von „{}“ sein — Abbruch.",
                    canon_src.display(),
                    canon_root.display()
                ),
            ));
        }
        if canon_dst == canon_root || !canon_dst.starts_with(&canon_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "Sicherheits-Sandbox aktiv: Ziel „{}“ muss ein Unterordner von „{}“ sein — Abbruch.",
                    canon_dst.display(),
                    canon_root.display()
                ),
            ));
        }
        if let Some(quarantine) = &safety.quarantine {
            guard_path(safety.sandbox(), quarantine, "Quarantäne")?;
            if quarantine.starts_with(dst_root) || dst_root.starts_with(quarantine) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Quarantäne und Ziel dürfen nicht ineinander liegen",
                ));
            }
        }
    }

    // Ein Dry-Run muss auch hochriskante Pläne vollständig anzeigen dürfen. Die Gates
    // blockieren deshalb erst die reale Ausführung.
    if safety.is_dry_run() || plan.delete_count == 0 {
        return Ok(());
    }
    // Alle vier Gates tragen den Code `MirrorBlocked`: GUI/CLI können so erkennen,
    // dass nicht die Operation selbst scheiterte, sondern ein Sicherheitslimit griff.
    if plan.source_count == 0 && !safety.allow_empty_source_mirror {
        return Err(crate::error::coded_error(
            io::ErrorKind::PermissionDenied,
            crate::error::ErrorCode::MirrorBlocked,
            "Leere Quelle würde Zielobjekte entfernen — reale Ausführung gesperrt",
        ));
    }
    if plan.skip_count > 0 {
        return Err(crate::error::coded_error(
            io::ErrorKind::PermissionDenied,
            crate::error::ErrorCode::MirrorBlocked,
            format!(
                "Mirror-Entfernungen gesperrt: {} Quell-/Zielkonflikte oder nicht unterstützte Einträge wurden ausgelassen",
                plan.skip_count
            ),
        ));
    }
    if let Some(max) = safety.max_delete_entries {
        if plan.delete_count > max {
            return Err(crate::error::coded_error(
                io::ErrorKind::PermissionDenied,
                crate::error::ErrorCode::MirrorBlocked,
                format!(
                    "Löschgrenze überschritten: {} Einträge geplant, höchstens {} erlaubt",
                    plan.delete_count, max
                ),
            ));
        }
    }
    if let Some(max) = safety.max_delete_fraction {
        if !max.is_finite() || !(0.0..=1.0).contains(&max) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Löschanteil muss eine endliche Zahl zwischen 0 und 1 sein",
            ));
        }
        if plan.target_count > 0 && plan.delete_count as f64 / plan.target_count as f64 > max {
            return Err(crate::error::coded_error(
                io::ErrorKind::PermissionDenied,
                crate::error::ErrorCode::MirrorBlocked,
                format!(
                    "Löschanteil überschritten: {} von {} Zieleinträgen geplant, höchstens {:.0}% erlaubt",
                    plan.delete_count,
                    plan.target_count,
                    max * 100.0
                ),
            ));
        }
    }
    Ok(())
}

/// Test-Sandbox-Läufe müssen ausschließlich über VFS-Instanzen arbeiten, die an
/// dieselbe gehaltene Wurzel gebunden sind. Ein sichtbarer Root-Austausch stoppt den
/// Lauf; die descriptor-relativen Operationen könnten dabei ohnehin nicht entkommen.
pub(super) fn validate_anchor_identity(
    src: &dyn Vfs,
    dst: &dyn Vfs,
    safety: &Safety,
) -> io::Result<()> {
    if let Some(permit) = safety.real_permit() {
        return permit.validate_roots(src, dst);
    }
    let Some(root) = safety.sandbox() else {
        return Ok(());
    };
    for (role, vfs) in [("Quell-VFS", src), ("Ziel-VFS", dst)] {
        if vfs.anchored_root() != Some(root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{role} ist nicht an die freigegebene Test-Sandbox gebunden"),
            ));
        }
        if vfs.anchored_identity() != safety.sandbox_identity() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{role} besitzt nicht die vom Test-Permit gebundene Root-Identität"),
            ));
        }
        if !vfs.anchor_is_current()? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{role}-Wurzel wurde seit der Freigabe ausgetauscht"),
            ));
        }
    }
    Ok(())
}

/// Kanonischer Pfad des tiefsten EXISTIERENDEN Präfixes von `p` (für die Sandbox-
/// Prüfung, auch wenn das Ziel selbst noch nicht existiert).
///
/// Wird auch von [`crate::fileops`] genutzt, damit die interaktiven Datei-Operationen
/// dieselbe Sandbox-Auflösung verwenden wie der Sync.
pub(crate) fn canonical_existing(p: &Path) -> io::Result<PathBuf> {
    let mut cur = p;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cur) {
            return Ok(canonical);
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Pfad nicht auflösbar: {}", p.display()),
                ));
            }
        }
    }
}

/// Lokale Sync-Wurzeln auflösen und jede Gleichheit oder Verschachtelung ablehnen.
/// Quelle-im-Ziel könnte die Quelle als „überzählig" löschen; Ziel-in-Quelle würde sich
/// selbst immer tiefer kopieren. Beides ist unabhängig vom gewählten Modus ungültig.
pub fn validate_local_sync_roots(source: &Path, target: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let source = std::fs::canonicalize(source)?;
    let target = std::fs::canonicalize(target)?;
    if !source.is_dir() || !target.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Quelle und Ziel müssen vorhandene Ordner sein",
        ));
    }
    if source == target || source.starts_with(&target) || target.starts_with(&source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Quelle und Ziel dürfen weder identisch noch ineinander verschachtelt sein: „{}“ ↔ „{}“",
                source.display(),
                target.display()
            ),
        ));
    }
    Ok((source, target))
}

/// Zusätzliche Schranke direkt vor JEDEM Schreib-, Rename- oder Löschzugriff. Der real
/// aufgelöste tiefste vorhandene Pfadpräfix muss in der Sandbox bleiben; dadurch werden
/// auch Symlinks in Elternverzeichnissen erkannt.
pub(super) fn guard_path(sandbox: Option<&Path>, target: &Path, kind: &str) -> io::Result<()> {
    if let Some(root) = sandbox {
        crate::path::reject_parent_traversal(target, || {
            format!(
                "Sicherheits-Sandbox: {kind} „{}“ enthält „..“ — abgebrochen.",
                target.display()
            )
        })?;
        let canon_root = canonical_existing(root)?;
        let canon_target = canonical_existing(target)?;
        if !canon_target.starts_with(&canon_root) {
            return Err(crate::error::coded_error(
                io::ErrorKind::PermissionDenied,
                crate::error::ErrorCode::SandboxViolation,
                format!(
                    "Sicherheits-Sandbox: {} „{}“ liegt nicht unter „{}“ — abgebrochen.",
                    kind,
                    canon_target.display(),
                    canon_root.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `exists()` liefert bei jedem Fehler `false`. Ein `.git`, dessen Ziel fehlt,
    /// hätte die Sandbox damit als git-frei durchgehen lassen — obwohl der Eintrag
    /// da ist und die Wurzel offensichtlich zu einem Repository gehört.
    #[test]
    fn toter_git_symlink_sperrt_die_test_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(temp.path()).unwrap();
        std::os::unix::fs::symlink(repo.join("gibt-es-nicht"), repo.join(".git")).unwrap();
        let sandbox = repo.join("sandbox");
        std::fs::create_dir(&sandbox).unwrap();
        std::fs::write(
            sandbox.join(crate::TEST_SANDBOX_MARKER),
            crate::TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();

        let fehler = validate_test_sandbox_root(&sandbox).unwrap_err();
        assert_eq!(fehler.kind(), io::ErrorKind::InvalidInput);
        assert!(
            fehler.to_string().contains("Git-Repository"),
            "unerwartete Meldung: {fehler}"
        );
    }

    /// Dieselbe Lücke wie in `fileops::guard`: `canonical_existing` löst nur den
    /// nächsten vorhandenen Vorfahren auf. Ein `..` danach muss deshalb schon
    /// vorher abgewiesen werden.
    #[test]
    fn sandbox_weist_ein_punktpunkt_nach_fehlender_komponente_ab() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let sandbox = root.join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::create_dir_all(root.join("draussen")).unwrap();

        let escape = sandbox.join("fehlt/../../draussen/datei.txt");
        let error = guard_path(Some(&sandbox), &escape, "Schreibziel").unwrap_err();
        assert_eq!(
            crate::error::error_code(&error),
            crate::error::ErrorCode::SandboxViolation
        );

        guard_path(
            Some(&sandbox),
            &sandbox.join("neu/tief/datei.txt"),
            "Schreibziel",
        )
        .unwrap();
    }
}
