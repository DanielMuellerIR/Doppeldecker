use super::*;
use crate::trash::journal_v2::claim::tests::hook;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: RootHandle,
    source: PathBuf,
    journal: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for (name, mode) in [("home", 0o700), ("source", 0o777), ("config", 0o700)] {
            std::fs::create_dir(root.join(name)).unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        let home = RootHandle::open_nofollow(&root.join("home")).unwrap();
        let source = root.join("source/original.txt");
        std::fs::write(&source, b"original").unwrap();
        Self {
            _temp: temp,
            journal: root.join("config/trash-journals"),
            root,
            home,
            source,
        }
    }
    fn trash(&self, paths: &[PathBuf]) -> io::Result<TrashStats> {
        trash_with_home(paths, None, &self.journal, Some(&self.home))
    }
    fn target(&self) -> PathBuf {
        self.home.path().join(".Trash/original.txt")
    }
    fn assert_settled(&self) {
        assert_eq!(std::fs::read_dir(&self.journal).unwrap().count(), 0);
        assert!(!std::fs::read_dir(self.home.path().join(".Trash"))
            .unwrap()
            .any(|item| item
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".doppeldecker-trash-")));
    }
}

#[test]
fn home_adapter_verwendet_v2_und_oeffentliche_nachfolger_bleiben_erhalten() {
    let f = Fixture::new();
    let source = f.source.clone();
    let journal = f.journal.clone();
    let _guard = hook(move |point, area| {
        if point == "before_transfer" {
            let journals = std::fs::read_dir(&journal)?.collect::<Result<Vec<_>, _>>()?;
            assert_eq!(journals.len(), 1);
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(journals[0].path())?)?;
            assert_eq!(value["version"], 2);
            assert_eq!(value["entries"][0]["phase"], "transfer_intent");
            assert_eq!(area.parent().unwrap().file_name().unwrap(), ".Trash");
            assert_eq!(std::fs::read(area.join("entry-0"))?, b"original");
            std::fs::write(&source, b"foreign-source")?;
            std::fs::write(source.with_file_name("entry-0"), b"foreign-entry")?;
            std::fs::write(
                source.with_file_name(".doppeldecker-trash-claim-public"),
                b"foreign-public",
            )?;
        }
        Ok(())
    });
    let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
    assert_eq!((result.trashed, result.failed, result.unclear), (1, 0, 0));
    assert!(result.errors.is_empty());
    assert!(result.recovery_paths.is_empty());
    assert_eq!(std::fs::read(f.target()).unwrap(), b"original");
    assert_eq!(std::fs::read(&f.source).unwrap(), b"foreign-source");
    assert_eq!(
        std::fs::read(f.source.with_file_name("entry-0")).unwrap(),
        b"foreign-entry"
    );
    assert_eq!(
        std::fs::read(f.source.with_file_name(".doppeldecker-trash-claim-public")).unwrap(),
        b"foreign-public"
    );
    f.assert_settled();
}

#[test]
fn source_swap_vor_claim_wird_exklusiv_zurueckgesetzt_und_nie_entsorgt() {
    let f = Fixture::new();
    let source = f.source.clone();
    let saved = f.root.join("saved");
    let saved_copy = saved.clone();
    let _guard = hook(move |point, _| {
        if point == "before_claim" {
            std::fs::rename(&source, &saved_copy)?;
            std::fs::write(&source, b"foreign")?;
        }
        Ok(())
    });
    let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
    assert_eq!((result.trashed, result.failed, result.unclear), (0, 1, 0));
    assert_eq!(std::fs::read(&f.source).unwrap(), b"foreign");
    assert_eq!(std::fs::read(saved).unwrap(), b"original");
    assert!(!f.target().exists());
    f.assert_settled();
}

#[test]
fn auswahl_drift_waehrend_layoutvorbereitung_beginnt_keinen_claim() {
    let f = Fixture::new();
    let source = f.source.clone();
    let saved = f.root.join("saved");
    let saved_copy = saved.clone();
    let _guard = hook(move |point, _| {
        if point == "native_layout_ready" {
            std::fs::rename(&source, &saved_copy)?;
            std::fs::write(&source, b"foreign")?;
        }
        Ok(())
    });
    assert!(f.trash(std::slice::from_ref(&f.source)).is_err());
    assert_eq!(std::fs::read(&f.source).unwrap(), b"foreign");
    assert_eq!(std::fs::read(saved).unwrap(), b"original");
    assert_eq!(std::fs::read_dir(f.journal).unwrap().count(), 0);
}

#[test]
fn umbenannter_quellelternordner_bleibt_vor_layoutabschluss_descriptor_gebunden() {
    let f = Fixture::new();
    let parent = f.root.join("source");
    let saved = f.root.join("saved-source");
    let foreign = f.root.join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("original.txt"), b"foreign").unwrap();
    let foreign_copy = foreign.clone();
    let saved_copy = saved.clone();
    let _guard = hook(move |point, _| {
        if point == "native_layout_ready" {
            std::fs::rename(&parent, &saved_copy)?;
            symlink(&foreign_copy, &parent)?;
        }
        Ok(())
    });
    let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
    assert_eq!(result.trashed, 1);
    assert!(result.errors.is_empty());
    assert_eq!(std::fs::read(f.target()).unwrap(), b"original");
    assert_eq!(
        std::fs::read(foreign.join("original.txt")).unwrap(),
        b"foreign"
    );
    assert!(!saved.join("original.txt").exists());
    f.assert_settled();
}

#[test]
fn native_kollision_und_spaeter_fsync_erhalten_bilanz_und_recovery() {
    for after_readback in [false, true] {
        let f = Fixture::new();
        let target = f.target();
        let _guard = hook(move |point, _| {
            if point == "before_transfer" && !after_readback {
                std::fs::write(&target, b"foreign")?;
            }
            if point == "transfer_sync" && after_readback {
                return Err(io::Error::other("injected"));
            }
            Ok(())
        });
        let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
        assert_eq!(result.trashed, usize::from(after_readback));
        assert_eq!(result.unclear, usize::from(!after_readback));
        assert_eq!(result.failed, 0);
        assert!(!result.recovery_paths.is_empty());
        assert_eq!(
            std::fs::read(f.target()).unwrap(),
            if after_readback {
                b"original".as_slice()
            } else {
                b"foreign".as_slice()
            }
        );
        let report = recover_trash_journals(&f.journal).unwrap();
        assert!(!report.is_clean());
    }
}

#[test]
fn spaeter_cleanupfehler_zaehlt_keinen_bereits_verschobenen_eintrag_als_fehlgeschlagen() {
    let f = Fixture::new();
    let guard = hook(|point, _| {
        if point == "before_marker_cleanup" {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    });
    let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
    assert_eq!((result.trashed, result.failed, result.unclear), (1, 0, 0));
    assert!(!result.recovery_paths.is_empty());
    drop(guard);
    let recovered = recover_trash_journals(&f.journal).unwrap();
    assert!(recovered.is_clean());
    assert_eq!(recovered.completed, 1);
    f.assert_settled();
}

#[test]
fn native_acl_inode_hardlinks_symlink_und_ordnerinhalt_bleiben_erhalten() {
    let f = Fixture::new();
    let trash = f.home.path().join(".Trash");
    std::fs::create_dir(&trash).unwrap();
    std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (path, ace) in [
        (&trash, "everyone deny delete"),
        (&f.source, "everyone allow read"),
    ] {
        assert!(std::process::Command::new("/bin/chmod")
            .args(["+a", ace])
            .arg(path)
            .status()
            .unwrap()
            .success());
    }
    assert!(std::process::Command::new("/usr/bin/xattr")
        .args(["-w", "com.doppeldecker.fixture", "native-value"])
        .arg(&f.source)
        .status()
        .unwrap()
        .success());
    let acl = |path: &Path| {
        crate::acl::read_fd(
            &openat(
                rustix::fs::CWD,
                path,
                OFlags::RDONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap(),
        )
        .unwrap()
    };
    let root_acl = acl(&trash);
    let source_acl = acl(&f.source);
    let before = std::fs::metadata(&f.source).unwrap();
    let hardlink = f.root.join("hardlink");
    std::fs::hard_link(&f.source, &hardlink).unwrap();
    let link = f.source.with_file_name("link");
    symlink("original.txt", &link).unwrap();
    let dir = f.source.with_file_name("folder");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("child"), b"child").unwrap();
    let result = f.trash(&[f.source.clone(), link, dir]).unwrap();
    assert_eq!(result.trashed, 3);
    assert!(result.errors.is_empty());
    assert_eq!(acl(&trash), root_acl);
    assert_eq!(acl(&f.target()), source_acl);
    let after = std::fs::metadata(f.target()).unwrap();
    assert_eq!(
        (before.ino(), before.uid(), before.gid(), before.mode()),
        (after.ino(), after.uid(), after.gid(), after.mode())
    );
    assert_eq!(std::fs::metadata(hardlink).unwrap().ino(), after.ino());
    assert_eq!(
        std::fs::read_link(trash.join("link")).unwrap(),
        Path::new("original.txt")
    );
    assert_eq!(std::fs::read(trash.join("folder/child")).unwrap(), b"child");
    let xattr = std::process::Command::new("/usr/bin/xattr")
        .args(["-p", "com.doppeldecker.fixture"])
        .arg(f.target())
        .output()
        .unwrap();
    assert!(xattr.status.success());
    assert_eq!(xattr.stdout, b"native-value\n");
    f.assert_settled();
}

#[test]
fn unsichere_trash_acl_und_ausgewaehlte_hilfswurzel_bleiben_unveraendert() {
    for unsafe_acl in [false, true] {
        let f = Fixture::new();
        let trash = f.home.path().join(".Trash");
        if unsafe_acl {
            std::fs::create_dir(&trash).unwrap();
            std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(std::process::Command::new("/bin/chmod")
                .args([
                    "+a",
                    "everyone allow read,write,execute,file_inherit,directory_inherit"
                ])
                .arg(&trash)
                .status()
                .unwrap()
                .success());
            let root = RootHandle::open_nofollow(&trash).unwrap();
            let before = crate::acl::read_fd(root.fd()).unwrap();
            assert!(f.trash(std::slice::from_ref(&f.source)).is_err());
            assert_eq!(crate::acl::read_fd(root.fd()).unwrap(), before);
        } else {
            assert!(f.trash(&[f.home.path().to_path_buf()]).is_err());
            assert!(!trash.exists());
            assert!(!f.journal.exists());
        }
        assert_eq!(std::fs::read(&f.source).unwrap(), b"original");
    }
}

#[test]
fn sandbox_verwendet_ausschliesslich_eigenen_papierkorb_und_journal() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let outside_journal = outside.path().canonicalize().unwrap().join("journal");
    let sandbox = RootHandle::open_nofollow(&f.root).unwrap();
    assert!(trash_with_home(
        std::slice::from_ref(&f.source),
        Some(sandbox.clone()),
        &outside_journal,
        Some(&f.home)
    )
    .is_err());
    assert!(!outside_journal.exists());
    assert_eq!(std::fs::read(&f.source).unwrap(), b"original");
    let result = trash_with_home(
        std::slice::from_ref(&f.source),
        Some(sandbox),
        &f.journal,
        None,
    )
    .unwrap();
    assert_eq!(result.trashed, 1);
    assert_eq!(
        std::fs::read(f.root.join(".doppeldecker-test-trash/original.txt")).unwrap(),
        b"original"
    );
    assert!(!f.home.path().join(".Trash").exists());
}

#[test]
#[ignore = "benötigt ein eigenes markiertes macOS-Testimage"]
fn echtes_volume_layout_achtet_mount_ownership_und_erhaelt_trash_acl() {
    let mount = PathBuf::from(
        std::env::var_os("DOPPELDECKER_NATIVE_TRASH_TEST_MOUNT").expect("eigenes Testimage"),
    );
    assert_eq!(
        std::fs::read(mount.join(".doppeldecker-trash-layout-fixture")).unwrap(),
        b"Doppeldecker native trash fixture\n"
    );
    let f = Fixture::new();
    let source = mount.join("native-original.txt");
    std::fs::write(&source, b"volume-original").unwrap();
    let expected = mount
        .join(".Trashes")
        .join(unsafe { libc::geteuid() }.to_string());
    let trash = RootHandle::open_nofollow(&expected).unwrap();
    let trash_acl = if std::env::var_os("DOPPELDECKER_NATIVE_TRASH_EXPECT_REJECT").is_some() {
        None
    } else {
        Some(crate::acl::read_fd(trash.fd()).unwrap())
    };
    let before = std::fs::metadata(&source).unwrap();
    let result = f.trash(std::slice::from_ref(&source));
    if std::env::var_os("DOPPELDECKER_NATIVE_TRASH_EXPECT_REJECT").is_some() {
        assert!(result.is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"volume-original");
        assert!(!expected.join("native-original.txt").exists());
    } else {
        let result = result.unwrap();
        assert_eq!((result.trashed, result.failed, result.unclear), (1, 0, 0));
        assert!(result.errors.is_empty());
        assert!(!source.exists());
        let target = expected.join("native-original.txt");
        assert_eq!(std::fs::read(&target).unwrap(), b"volume-original");
        assert_eq!(before.ino(), std::fs::metadata(target).unwrap().ino());
        assert_eq!(std::fs::read_dir(&f.journal).unwrap().count(), 0);
    }
    assert!(!f.home.path().join(".Trash").exists());
    if let Some(acl) = trash_acl {
        assert_eq!(crate::acl::read_fd(trash.fd()).unwrap(), acl);
    }
}

#[test]
#[ignore = "benötigt ein eigenes markiertes macOS-Testimage"]
fn mehrere_volumegruppen_werden_vor_dem_ersten_claim_vollstaendig_vorbereitet() {
    let mount = PathBuf::from(
        std::env::var_os("DOPPELDECKER_NATIVE_TRASH_TEST_MOUNT").expect("eigenes Testimage"),
    );
    assert_eq!(
        std::fs::read(mount.join(".doppeldecker-trash-layout-fixture")).unwrap(),
        b"Doppeldecker native trash fixture\n"
    );
    let f = Fixture::new();
    let source = mount.join("native-second.txt");
    std::fs::write(&source, b"second-volume").unwrap();
    let result = f.trash(&[f.source.clone(), source.clone()]);
    if std::env::var_os("DOPPELDECKER_NATIVE_TRASH_EXPECT_REJECT").is_some() {
        assert!(result.is_err());
        assert_eq!(std::fs::read(&f.source).unwrap(), b"original");
        assert_eq!(std::fs::read(source).unwrap(), b"second-volume");
    } else {
        let result = result.unwrap();
        assert_eq!((result.trashed, result.failed, result.unclear), (2, 0, 0));
        assert!(result.errors.is_empty());
        assert_eq!(std::fs::read(f.target()).unwrap(), b"original");
        assert_eq!(
            std::fs::read(
                mount
                    .join(".Trashes")
                    .join(unsafe { libc::geteuid() }.to_string())
                    .join("native-second.txt")
            )
            .unwrap(),
            b"second-volume"
        );
        f.assert_settled();
    }
}
