use super::*;
use crate::metadata::{Acl, PosixAcl, PosixAclEntry, PosixAclTag};
use crate::trash::journal_v2::claim::tests::hook;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    data: DataHome,
    source: PathBuf,
    journal: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("doppeldecker-xdg-")
            .tempdir()
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (name, mode) in [("home", 0o700), ("source", 0o777), ("config", 0o700)] {
            std::fs::create_dir(root.join(name)).unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        let source = root.join("source/a % file.txt");
        std::fs::write(&source, b"original").unwrap();
        Self {
            data: DataHome::new(root.join("home/.local/share")).unwrap(),
            journal: root.join("config/trash-journals"),
            source,
            root,
            _temp: temp,
        }
    }
    fn trash(&self, items: &[PathBuf]) -> io::Result<TrashStats> {
        trash_with_context(items, None, &self.journal, Some(&self.data))
    }
    fn target(&self) -> PathBuf {
        self.data
            .path
            .join("Trash/files")
            .join(self.source.file_name().unwrap())
    }
    fn info(&self) -> PathBuf {
        self.data
            .path
            .join("Trash/info")
            .join(info_name(self.source.file_name().unwrap()))
    }
    fn settled(&self) {
        assert_eq!(std::fs::read_dir(&self.journal).unwrap().count(), 0);
        let names = std::fs::read_dir(self.data.path.join("Trash"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<HashSet<_>>();
        assert_eq!(
            names,
            [OsString::from("files"), OsString::from("info")]
                .into_iter()
                .collect()
        );
    }
}

#[test]
fn home_layout_legt_fehlendes_datenhome_an_und_veroeffentlicht_info_vor_daten() {
    let f = Fixture::new();
    let original = std::fs::symlink_metadata(&f.source).unwrap();
    let target = f.target();
    let info = f.info();
    let source = f.source.clone();
    let _hook = hook(move |point, area| {
        if point == "before_transfer" {
            assert!(std::fs::symlink_metadata(&target).is_err());
            let text = std::fs::read_to_string(&info)?;
            assert!(text.starts_with("[Trash Info]\nPath=/"));
            assert!(text.contains("a%20%25%20file.txt\nDeletionDate="));
            assert_eq!(std::fs::read(area.join("entry-0"))?, b"original");
            std::fs::write(&source, b"foreign")?;
        }
        Ok(())
    });
    let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
    assert_eq!((result.trashed, result.failed, result.unclear), (1, 0, 0));
    assert!(result.errors.is_empty());
    assert_eq!(
        std::fs::symlink_metadata(f.target()).unwrap().ino(),
        original.ino()
    );
    assert_eq!(std::fs::read(&f.source).unwrap(), b"foreign");
    f.settled();
}

#[test]
fn vorhandener_home_trash_0755_und_default_acl_bleiben_unveraendert() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.data.path.join("Trash")).unwrap();
    for path in [f.root.join("home/.local"), f.data.path.clone()] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root = RootHandle::open_nofollow(&f.data.path.join("Trash")).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let entries = [
        (PosixAclTag::UserObject, None, 7),
        (PosixAclTag::User, Some(65534), 7),
        (PosixAclTag::GroupObject, None, 0),
        (PosixAclTag::Mask, None, 7),
        (PosixAclTag::Other, None, 0),
    ]
    .into_iter()
    .map(|(tag, id, permissions)| PosixAclEntry {
        tag,
        id,
        permissions,
    })
    .collect();
    crate::acl::write_fd_exact(
        root.fd(),
        Some(&Acl::Posix(PosixAcl {
            access: vec![],
            default: entries,
        })),
    )
    .unwrap();
    let before = crate::acl::read_fd(root.fd()).unwrap();
    let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
    assert_eq!(result.trashed, 1);
    assert_eq!(crate::acl::read_fd(root.fd()).unwrap(), before);
    for name in ["files", "info"] {
        let child = RootHandle::open_beneath(&root, &root.path().join(name)).unwrap();
        assert_eq!(fstat(child.fd()).unwrap().st_mode & 0o777, 0o700);
        let Some(Acl::Posix(acl)) = crate::acl::read_fd(child.fd()).unwrap() else {
            panic!("Geerbte ACL fehlt")
        };
        assert!(acl
            .access
            .iter()
            .any(|entry| entry.tag == PosixAclTag::Mask && entry.permissions == 0));
    }
    f.settled();
}

#[test]
fn unsicheres_datenhome_und_symlink_eltern_beginnen_keinen_claim() {
    let f = Fixture::new();
    std::fs::set_permissions(f.root.join("home"), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(f.trash(std::slice::from_ref(&f.source)).is_err());
    assert_eq!(std::fs::read(&f.source).unwrap(), b"original");
    std::fs::set_permissions(f.root.join("home"), std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(f.root.join("source"), f.root.join("home/.local")).unwrap();
    assert!(DataHome::new(f.data.path.clone()).is_err());
    assert_eq!(std::fs::read(&f.source).unwrap(), b"original");
}

#[test]
fn info_kollision_nach_claim_bleibt_manuell_und_fremde_info_erhalten() {
    let f = Fixture::new();
    let info = f.info();
    let _hook = hook(move |point, _| {
        if point == "before_info_publish" {
            std::fs::write(&info, b"foreign-info")?;
        }
        Ok(())
    });
    let result = f.trash(std::slice::from_ref(&f.source)).unwrap();
    assert_eq!((result.trashed, result.failed, result.unclear), (0, 0, 1));
    assert_eq!(std::fs::read(f.info()).unwrap(), b"foreign-info");
    assert!(std::fs::symlink_metadata(f.target()).is_err());
    let recovery = recover_trash_journals(&f.journal).unwrap();
    assert!(!recovery.is_clean());
    assert_eq!(std::fs::read(f.info()).unwrap(), b"foreign-info");
}

#[test]
fn auswahl_drift_vor_claim_und_datenhome_austausch_bleiben_ohne_quellwirkung() {
    let f = Fixture::new();
    let source = f.source.clone();
    let saved = f.root.join("saved");
    let _hook = hook(move |point, _| {
        if point == "native_layout_ready" {
            std::fs::rename(&source, &saved)?;
            std::fs::write(&source, b"foreign")?;
        }
        Ok(())
    });
    assert!(f.trash(std::slice::from_ref(&f.source)).is_err());
    assert_eq!(std::fs::read(&f.source).unwrap(), b"foreign");
    drop(_hook);
    let f = Fixture::new();
    std::fs::rename(f.root.join("home"), f.root.join("old-home")).unwrap();
    std::fs::create_dir(f.root.join("home")).unwrap();
    assert!(f.trash(std::slice::from_ref(&f.source)).is_err());
    assert_eq!(std::fs::read(&f.source).unwrap(), b"original");
}

#[test]
fn sandbox_layout_verwendet_nur_eigenen_papierkorb_und_absolute_info() {
    let f = Fixture::new();
    let stats = trash(
        std::slice::from_ref(&f.source),
        &Some(f.root.clone()),
        &f.journal,
    )
    .unwrap();
    assert_eq!(stats.trashed, 1);
    let root = f.root.join(".doppeldecker-test-trash");
    assert_eq!(
        std::fs::read(root.join("files").join(f.source.file_name().unwrap())).unwrap(),
        b"original"
    );
    assert!(std::fs::read_to_string(
        root.join("info")
            .join(info_name(f.source.file_name().unwrap()))
    )
    .unwrap()
    .contains("Path=/"));
    assert!(std::fs::symlink_metadata(&f.data.path).is_err());
}

#[test]
fn linux_rename_erhaelt_acl_inode_hardlinks_symlink_und_ordnerinhalt() {
    let f = Fixture::new();
    let parent = RootHandle::open_nofollow(f.source.parent().unwrap()).unwrap();
    let file = File::open(&f.source).unwrap();
    let fd: OwnedFd = file.try_clone().unwrap().into();
    let access = [
        (PosixAclTag::UserObject, None, 6),
        (PosixAclTag::User, Some(65534), 4),
        (PosixAclTag::GroupObject, None, 0),
        (PosixAclTag::Mask, None, 4),
        (PosixAclTag::Other, None, 0),
    ]
    .into_iter()
    .map(|(tag, id, permissions)| PosixAclEntry {
        tag,
        id,
        permissions,
    })
    .collect();
    crate::acl::write_fd_exact(
        &fd,
        Some(&Acl::Posix(PosixAcl {
            access,
            default: vec![],
        })),
    )
    .unwrap();
    let before_acl = crate::acl::read_fd(&fd).unwrap();
    let before = file.metadata().unwrap();
    let hardlink = parent.path().join("hardlink");
    std::fs::hard_link(&f.source, &hardlink).unwrap();
    let link = parent.path().join("link");
    symlink(&f.source, &link).unwrap();
    let dir = parent.path().join("directory");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("inside"), b"nested").unwrap();
    let stats = f.trash(&[f.source.clone(), link, dir]).unwrap();
    assert_eq!((stats.trashed, stats.failed, stats.unclear), (3, 0, 0));
    let target = File::open(f.target()).unwrap();
    let target_fd: OwnedFd = target.try_clone().unwrap().into();
    let after = target.metadata().unwrap();
    assert_eq!(
        (after.ino(), after.uid(), after.gid(), after.mode()),
        (before.ino(), before.uid(), before.gid(), before.mode())
    );
    assert_eq!(crate::acl::read_fd(&target_fd).unwrap(), before_acl);
    assert_eq!(std::fs::metadata(hardlink).unwrap().ino(), after.ino());
    assert_eq!(
        std::fs::read_link(f.data.path.join("Trash/files/link")).unwrap(),
        f.source
    );
    assert_eq!(
        std::fs::read(f.data.path.join("Trash/files/directory/inside")).unwrap(),
        b"nested"
    );
    f.settled();
}

#[test]
#[ignore = "benötigt eigene markierte Linux-Mounts und künstliche XDG-Roots"]
fn echte_xdg_mountlayouts_datenhome_und_stapelgrenzen() {
    let root = PathBuf::from(
        std::env::var_os("DOPPELDECKER_XDG_TRASH_TEST_ROOT").expect("eigene Fixture fehlt"),
    );
    assert_eq!(
        std::fs::read(root.join(".doppeldecker-xdg-trash-fixture")).unwrap(),
        b"Doppeldecker XDG trash fixture\n"
    );
    let case = std::env::var("DOPPELDECKER_XDG_TRASH_TEST_CASE").unwrap();
    let volume = root.join("volume");
    let cross_data = case == "cross-data-home";
    let source = if cross_data {
        root.join("home/source/original.txt")
    } else {
        volume.join("source/original.txt")
    };
    let data = DataHome::new(if cross_data {
        volume.join("data")
    } else {
        root.join("home/data")
    })
    .unwrap();
    let parent = RootHandle::open_nofollow(source.parent().unwrap()).unwrap();
    let destination = destination(&parent, &data).unwrap();
    let mount = destination.origin_mount.as_ref().unwrap();
    assert_ne!(
        mount.current_mount_identity().unwrap(),
        data.ancestor.current_mount_identity().unwrap()
    );
    let uid = unsafe { libc::geteuid() };
    let expected = match case.as_str() {
        "shared" | "shared-default-acl" => volume.join(".Trash").join(uid.to_string()),
        "cross-data-home" => root.join(format!(".Trash-{uid}")),
        _ => volume.join(format!(".Trash-{uid}")),
    };
    if matches!(
        case.as_str(),
        "shared-denied" | "shared-unsafe-uid" | "fallback-selected"
    ) {
        assert_eq!(
            destination.root,
            volume.join(".Trash").join(uid.to_string())
        );
    } else {
        assert_eq!(destination.root, expected);
    }
    let journal = root.join("config/trash-journals");
    let shared_acl_before = if case == "shared-default-acl" {
        Some(
            crate::acl::read_fd(
                RootHandle::open_nofollow(&volume.join(".Trash"))
                    .unwrap()
                    .fd(),
            )
            .unwrap(),
        )
    } else {
        None
    };
    let original = std::fs::symlink_metadata(&source).unwrap();
    let mut items = vec![source.clone()];
    if case == "fallback-selected" {
        items.push(expected.clone());
    }
    if case == "shared-denied" {
        items.push(volume.join("source/second.txt"));
    }
    if case == "unsafe-second-group" {
        items.insert(0, root.join("home/source/original.txt"));
    }
    let result = trash_with_context(&items, None, &journal, Some(&data));
    if matches!(
        case.as_str(),
        "unsafe-second-group" | "readonly" | "fallback-selected"
    ) {
        assert!(result.is_err());
        if case == "fallback-selected" {
            assert_eq!(std::fs::read(&source).unwrap(), b"original");
            assert_eq!(std::fs::read_dir(&expected).unwrap().count(), 0);
            assert!(
                matches!(std::fs::symlink_metadata(&journal),Err(error) if error.kind()==io::ErrorKind::NotFound)
            );
            return;
        }
        for item in &items {
            assert_eq!(std::fs::read(item).unwrap(), b"original");
        }
        return;
    }
    let result = result.unwrap();
    assert_eq!(
        (result.trashed, result.failed, result.unclear),
        (items.len(), 0, 0)
    );
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    if case == "shared-denied" {
        assert_eq!(
            std::fs::read(expected.join("files/second.txt")).unwrap(),
            b"second"
        );
        assert!(
            std::fs::read_to_string(expected.join("info/second.txt.trashinfo"))
                .unwrap()
                .contains("Path=source/second.txt\n")
        );
    }
    assert_eq!(
        std::fs::symlink_metadata(expected.join("files/original.txt"))
            .unwrap()
            .ino(),
        original.ino()
    );
    assert_eq!(
        std::fs::read(expected.join("files/original.txt")).unwrap(),
        b"original"
    );
    assert!(
        matches!(std::fs::symlink_metadata(&source),Err(error) if error.kind()==io::ErrorKind::NotFound)
    );
    let text = std::fs::read_to_string(expected.join("info/original.txt.trashinfo")).unwrap();
    let origin = if cross_data {
        "home/source/original.txt"
    } else {
        "source/original.txt"
    };
    assert!(
        text.starts_with(&format!("[Trash Info]\nPath={origin}\nDeletionDate=")),
        "{text}"
    );
    assert_eq!(std::fs::read_dir(&journal).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&expected).unwrap().count(), 2);
    if case == "symlink-shared" {
        assert_eq!(
            std::fs::read(root.join("foreign/untouched")).unwrap(),
            b"foreign"
        );
        assert!(std::fs::symlink_metadata(volume.join(".Trash"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
    if case == "shared-default-acl" {
        let shared = RootHandle::open_nofollow(&volume.join(".Trash")).unwrap();
        assert_eq!(
            Some(crate::acl::read_fd(shared.fd()).unwrap()),
            shared_acl_before
        );
    }
    assert!(
        matches!(std::fs::symlink_metadata(data.path.join("Trash")),Err(error) if error.kind()==io::ErrorKind::NotFound)
    );
}
