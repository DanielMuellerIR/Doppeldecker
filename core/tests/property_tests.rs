//! Generative Invarianten für die löschende Sync-Engine.
//!
//! Jeder Fall arbeitet ausschließlich in einem frischen temporären Baum. Die
//! Strategien erzeugen bewusst kleine, aber verschachtelte Verzeichnisse, damit ein
//! Fehler gut schrumpft und anschließend als normaler Regressionstest lesbar bleibt.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use doppeldecker_core::{
    execute, plan, Cancel, Compare, ExecuteRequest, LocalVfs, MetadataProfile, Safety, SyncMode,
    SyncOptions,
};
use proptest::collection::{btree_map, vec};
use proptest::prelude::*;

#[derive(Clone, Debug)]
struct GeneratedTree {
    files: BTreeMap<(u8, u8, String), Vec<u8>>,
    empty_dirs: BTreeSet<(u8, u8, u8)>,
}

fn tree_strategy() -> impl Strategy<Value = GeneratedTree> {
    // Numerische Ordnerkomponenten halten Elternpfade eindeutig als Verzeichnisse.
    // Drei Ebenen und zusätzliche leere Blattordner prüfen mehr als den früheren
    // festen Zweiebenenbaum, bleiben beim Shrinken aber gut lesbar.
    (
        btree_map(
            (0_u8..4, 0_u8..4, "[a-z][a-z0-9]{0,7}"),
            vec(any::<u8>(), 0..192),
            0..18,
        ),
        proptest::collection::btree_set((0_u8..4, 0_u8..4, 0_u8..4), 0..8),
    )
        .prop_map(|(files, empty_dirs)| GeneratedTree { files, empty_dirs })
}

fn canonical_tempdir() -> tempfile::TempDir {
    // macOS stellt `/var` als Symlink auf `/private/var` bereit. Der Kern folgt
    // Elternsymlinks absichtlich nicht, daher entspricht der Testroot der bereits
    // kanonisierten App-Grenze.
    let root = fs::canonicalize(std::env::temp_dir()).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn write_tree(root: &Path, tree: &GeneratedTree) {
    for (first, second, third) in &tree.empty_dirs {
        fs::create_dir_all(
            root.join(format!("ordner-{first}"))
                .join(format!("ebene-{second}"))
                .join(format!("leer-{third}")),
        )
        .unwrap();
    }
    for ((first, second, name), content) in &tree.files {
        let path = root
            .join(format!("ordner-{first}"))
            .join(format!("ebene-{second}"))
            .join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DiskNode {
    Directory,
    File(Vec<u8>),
}

fn disk_tree(root: &Path) -> BTreeMap<PathBuf, DiskNode> {
    fn visit(root: &Path, directory: &Path, nodes: &mut BTreeMap<PathBuf, DiskNode>) {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                nodes.insert(relative, DiskNode::Directory);
                visit(root, &path, nodes);
            } else if metadata.is_file() {
                nodes.insert(relative, DiskNode::File(fs::read(&path).unwrap()));
            } else {
                panic!(
                    "Generator erzeugte unerwarteten Eintrag: {}",
                    path.display()
                );
            }
        }
    }

    let mut nodes = BTreeMap::new();
    visit(root, root, &mut nodes);
    nodes
}

fn mirror_options() -> SyncOptions {
    SyncOptions {
        mode: SyncMode::Mirror,
        compare: Compare::ContentHash,
        metadata: MetadataProfile::Portable,
        ..SyncOptions::default()
    }
}

fn test_safety(quarantine: PathBuf) -> Safety {
    let mut safety = Safety::headless_test();
    safety.quarantine = Some(quarantine);
    // Kleine generierte Bäume dürfen absichtlich auch vollständig geleert werden.
    // Diese Lockerung existiert nur im unmissverständlich benannten Headless-Permit.
    safety.max_delete_entries = None;
    safety.max_delete_fraction = None;
    safety.allow_empty_source_mirror = true;
    safety
}

fn execute_mirror(source: &Path, target: &Path, quarantine: PathBuf) {
    let vfs = LocalVfs::new();
    let options = mirror_options();
    let confirmed = plan(&vfs, source, &vfs, target, &options).unwrap();
    execute(
        ExecuteRequest {
            src: &vfs,
            src_root: source,
            dst: &vfs,
            dst_root: target,
            plan: &confirmed,
            safety: &test_safety(quarantine),
        },
        &Cancel::new(),
        &mut |_| {},
    )
    .unwrap();
}

fn assert_semantically_equal(source: &Path, target: &Path) {
    assert_eq!(
        disk_tree(target),
        disk_tree(source),
        "Zielbaum muss unabhängig vom Planer Pfade, Typen und Dateibytes der Quelle enthalten"
    );
    let vfs = LocalVfs::new();
    let follow_up = plan(&vfs, source, &vfs, target, &mirror_options()).unwrap();
    assert!(
        follow_up.actions.is_empty(),
        "ein erfolgreicher Lauf muss einen leeren Folgeplan erzeugen: {:?}",
        follow_up.actions
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 2_048,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn plan_ausfuehrung_konvergiert_und_ist_idempotent(
        source_tree in tree_strategy(),
        target_tree in tree_strategy(),
    ) {
        let temp = canonical_tempdir();
        let source = temp.path().join("quelle");
        let target = temp.path().join("ziel");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        write_tree(&source, &source_tree);
        write_tree(&target, &target_tree);

        execute_mirror(&source, &target, temp.path().join("quarantaene-1"));
        assert_semantically_equal(&source, &target);

        // Auch eine zweite Ausführung des leeren Plans darf keine neue Wirkung
        // erzeugen. Die erneute Planung danach bleibt deshalb ebenfalls leer.
        execute_mirror(&source, &target, temp.path().join("quarantaene-2"));
        assert_semantically_equal(&source, &target);
    }

    #[test]
    fn mirror_roundtrip_erhaelt_den_vollstaendigen_quellbaum(
        source_tree in tree_strategy(),
        unrelated_tree in tree_strategy(),
    ) {
        let temp = canonical_tempdir();
        let source = temp.path().join("quelle");
        let first_target = temp.path().join("zwischenziel");
        let roundtrip_target = temp.path().join("roundtrip-ziel");
        for root in [&source, &first_target, &roundtrip_target] {
            fs::create_dir_all(root).unwrap();
        }
        write_tree(&source, &source_tree);
        write_tree(&first_target, &unrelated_tree);

        execute_mirror(
            &source,
            &first_target,
            temp.path().join("quarantaene-hin"),
        );
        execute_mirror(
            &first_target,
            &roundtrip_target,
            temp.path().join("quarantaene-rueck"),
        );

        assert_semantically_equal(&source, &roundtrip_target);
    }
}
