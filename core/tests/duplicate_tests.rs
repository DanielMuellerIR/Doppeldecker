//! Lokale Duplikate verwenden unveränderte Copy-Queue und persistierte Aliasbelege.
use doppeldecker_core::{fileops::*, *};
use std::path::{Path, PathBuf};

struct Case {
    _temp: tempfile::TempDir,
    root: PathBuf,
    journal: PathBuf,
}
impl Case {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let journal = root.join("state/queue.json");
        Self {
            _temp: temp,
            root,
            journal,
        }
    }
    fn prepare(&self, names: &[&str]) -> (Vec<PathBuf>, Vec<FileKeepBothTarget>) {
        let items = names
            .iter()
            .map(|name| self.root.join(name))
            .collect::<Vec<_>>();
        let aliases = prepare_local_duplicate_targets(
            &LocalVfs::rooted(&self.root).unwrap(),
            &self.root,
            &items,
        )
        .unwrap();
        (items, aliases)
    }
    fn enqueue(&self, items: &[PathBuf], aliases: Vec<FileKeepBothTarget>) -> OperationQueue {
        let queue = OperationQueue::open(&self.journal).unwrap();
        queue
            .enqueue(QueueOperation::File {
                kind: FileQueueKind::Copy,
                items: items
                    .iter()
                    .map(|path| path.to_str().unwrap().to_owned())
                    .collect(),
                destination: self.root.to_str().unwrap().into(),
                overwrite: false,
                expected_conflicts: Vec::new(),
                expected_conflict_proofs: Vec::new(),
                keep_both_targets: aliases,
                local_replace_best_effort_expected: Some(false),
                local_copy_new_best_effort_expected: None,
            })
            .unwrap();
        queue
    }
    fn execute(&self, queue: &OperationQueue) -> std::io::Result<QueueExecutionResult> {
        execute_local_queue_entry(
            queue,
            queue.claim_next()?.unwrap(),
            &LocalQueueExecution {
                sandbox: None,
                sync_quarantine: None,
                max_delete_entries: 100,
                max_delete_fraction: 0.25,
            },
            &Cancel::new(),
            &mut |_| {},
        )
    }
}

#[test]
fn duplizieren_mehrfachauswahl_und_leerer_ordner_ueberleben_queue_neustart() {
    let case = Case::new();
    std::fs::write(case.root.join("Brief.txt"), b"Inhalt").unwrap();
    std::fs::create_dir_all(case.root.join("Baum/Leer")).unwrap();
    std::fs::write(case.root.join("Baum/Kind"), b"Kindinhalt").unwrap();
    std::fs::create_dir(case.root.join("Leer")).unwrap();
    let (items, aliases) = case.prepare(&["Brief.txt", "Baum", "Leer"]);
    assert_eq!(aliases.len(), items.len());
    for alias in &aliases {
        assert_eq!(Path::new(&alias.target).parent(), Some(case.root.as_path()));
        assert!(!items.contains(&PathBuf::from(&alias.target)));
    }
    assert!(aliases[0].target.ends_with(".txt"));
    drop(case.enqueue(&items, aliases.clone()));
    let queue = OperationQueue::open(&case.journal).unwrap();
    let QueueExecutionResult::File(stats) = case.execute(&queue).unwrap() else {
        panic!("Kein Copy-Ergebnis")
    };
    assert_eq!((stats.files, stats.skipped), (2, 0));
    assert_eq!(queue.snapshot().entries[0].state, QueueState::Completed);
    assert_eq!(std::fs::read(&aliases[0].target).unwrap(), b"Inhalt");
    assert_eq!(
        std::fs::read(Path::new(&aliases[1].target).join("Kind")).unwrap(),
        b"Kindinhalt"
    );
    assert!(Path::new(&aliases[1].target).join("Leer").is_dir());
    assert!(Path::new(&aliases[2].target).is_dir());
    assert_eq!(
        std::fs::read(case.root.join("Brief.txt")).unwrap(),
        b"Inhalt"
    );
    assert_eq!(
        std::fs::read(case.root.join("Baum/Kind")).unwrap(),
        b"Kindinhalt"
    );
}

#[test]
fn gewoehnliche_selbstkopie_und_unvollstaendige_aliasauswahl_bleiben_wirkungslos() {
    for partial in [false, true] {
        let case = Case::new();
        std::fs::write(case.root.join("A"), b"A").unwrap();
        std::fs::write(case.root.join("B"), b"B").unwrap();
        let (items, mut aliases) = case.prepare(&["A", "B"]);
        let targets = aliases
            .iter()
            .map(|alias| alias.target.clone())
            .collect::<Vec<_>>();
        aliases.truncate(usize::from(partial));
        let queue = case.enqueue(&items, aliases);
        assert!(case.execute(&queue).is_err());
        assert!(targets.iter().all(|target| !Path::new(target).exists()));
        assert_eq!(std::fs::read(case.root.join("A")).unwrap(), b"A");
        assert_eq!(std::fs::read(case.root.join("B")).unwrap(), b"B");
    }
}

#[test]
fn duplizieren_bemerkte_quell_und_kinddrift_stoppen_vor_zielwirkung() {
    for directory in [false, true] {
        let case = Case::new();
        let name = if directory { "Baum" } else { "Datei" };
        let content = if directory {
            std::fs::create_dir(case.root.join(name)).unwrap();
            case.root.join("Baum/Kind")
        } else {
            case.root.join(name)
        };
        std::fs::write(&content, b"vorher").unwrap();
        let (items, aliases) = case.prepare(&[name]);
        let target = aliases[0].target.clone();
        let queue = case.enqueue(&items, aliases);
        std::fs::write(&content, b"andere").unwrap();
        let error = case.execute(&queue).unwrap_err();
        assert_eq!(error_code(&error), ErrorCode::PlanDrift);
        assert!(!Path::new(&target).exists());
        assert_eq!(std::fs::read(content).unwrap(), b"andere");
    }
}

#[test]
fn duplizieren_spaete_case_und_unicode_zielkollisionen_ersetzen_nichts() {
    for kind in ["exact", "case", "unicode"] {
        let case = Case::new();
        std::fs::write(case.root.join("é.txt"), b"Original").unwrap();
        let (items, aliases) = case.prepare(&["é.txt"]);
        let candidate = PathBuf::from(&aliases[0].target);
        let name = candidate.file_name().unwrap().to_str().unwrap();
        let occupied = case.root.join(match kind {
            "case" => name.to_uppercase(),
            "unicode" => name.replace('é', "e\u{301}"),
            _ => name.into(),
        });
        let queue = case.enqueue(&items, aliases);
        std::fs::write(&occupied, b"Fremd").unwrap();
        assert_eq!(
            error_code(&case.execute(&queue).unwrap_err()),
            ErrorCode::PlanDrift
        );
        assert_eq!(std::fs::read(&occupied).unwrap(), b"Fremd");
        assert_eq!(std::fs::read(&items[0]).unwrap(), b"Original");
    }
}

#[test]
fn duplizieren_fremde_aliasziele_und_gemischte_eltern_werden_abgelehnt() {
    let case = Case::new();
    std::fs::write(case.root.join("A"), b"A").unwrap();
    std::fs::write(case.root.join("B"), b"B").unwrap();
    std::fs::create_dir(case.root.join("Unter")).unwrap();
    std::fs::write(case.root.join("Unter/C"), b"C").unwrap();
    let vfs = LocalVfs::rooted(&case.root).unwrap();
    for paths in [
        vec![],
        vec![case.root.join("A"), case.root.join("Unter/C")],
        vec![case.root.join("Unter"), case.root.join("Unter/C")],
    ] {
        assert!(prepare_local_duplicate_targets(&vfs, &case.root, &paths).is_err());
    }
    let (items, aliases) = case.prepare(&["A"]);
    for target in [
        case.root.join("B"),
        case.root.join("Unter/A"),
        case.root.join("../Ausbruch"),
    ] {
        let mut forged = aliases.clone();
        forged[0].target = target.to_str().unwrap().into();
        assert!(verify_keep_both_targets(&vfs, &vfs, &items, &case.root, &forged).is_err());
    }
}
