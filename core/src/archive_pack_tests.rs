use super::*;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    target: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("dd-zip-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _temp: temp,
            source,
            target,
        }
    }
    fn prepare(
        &self,
        names: &[&str],
        cancel: &Cancel,
        compression: ArchivePackCompression,
    ) -> PreparedArchivePack {
        prepare_archive_pack(
            &self.source,
            &names
                .iter()
                .map(|name| self.source.join(name))
                .collect::<Vec<_>>(),
            &self.target,
            "packed.zip",
            ArchivePackOptions { compression },
            None,
            cancel,
        )
        .unwrap()
    }
    fn target_files(&self) -> Vec<PathBuf> {
        let mut pending = vec![self.target.clone()];
        let mut files = Vec::new();
        while let Some(path) = pending.pop() {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files
    }
    fn assert_no_output(&self) {
        assert!(self.target_files().is_empty(), "{:?}", self.target_files());
    }
}

#[test]
fn archive_pack_colon_names_are_omitted_and_usable_content_roundtrips() {
    for compression in [
        ArchivePackCompression::Stored,
        ArchivePackCompression::Deflate,
    ] {
        let f = Fixture::new();
        fs::write(f.source.join("a:b.txt"), b"omitted file").unwrap();
        fs::create_dir(f.source.join("bad:dir")).unwrap();
        fs::write(f.source.join("bad:dir/child"), b"omitted subtree").unwrap();
        fs::create_dir(f.source.join("folder")).unwrap();
        fs::write(f.source.join("folder/bad:child.txt"), b"omitted child").unwrap();
        fs::create_dir(f.source.join("folder/bad:dir")).unwrap();
        fs::write(
            f.source.join("folder/bad:dir/child"),
            b"omitted nested subtree",
        )
        .unwrap();
        fs::write(f.source.join("good.txt"), b"good").unwrap();
        fs::write(f.source.join("folder/good.txt"), b"nested").unwrap();
        let prepared = f.prepare(
            &["a:b.txt", "bad:dir", "folder", "good.txt"],
            &Cancel::new(),
            compression,
        );
        assert_eq!(
            (prepared.preview.files, prepared.preview.directories),
            (2, 1)
        );
        let omitted = prepared
            .preview
            .issues
            .iter()
            .filter(|issue| issue.content_omitted)
            .collect::<Vec<_>>();
        assert_eq!(omitted.len(), 4);
        for name in [
            "a:b.txt",
            "bad:dir",
            "folder/bad:child.txt",
            "folder/bad:dir",
        ] {
            assert!(
                omitted.iter().any(|issue| issue.path.contains(name)),
                "{name}"
            );
        }
        f.assert_no_output();
        prepared.execute(None, &mut |_| {}).unwrap();
        let reader = crate::archive_read::ArchiveRead::open(
            &f.target,
            Path::new("packed.zip"),
            crate::archive_read::CatalogBudget::default(),
            &Cancel::new(),
        )
        .unwrap();
        assert!(reader.is_complete());
        assert_eq!(reader.entries().len(), 3);
        assert!(reader.entries().iter().all(|entry| entry.issues.is_empty()));
        let unpacked = f.target.join("unpacked");
        fs::create_dir(&unpacked).unwrap();
        let request = crate::archive_extract::ArchiveExtractRequest {
            source_root: f.target.clone(),
            source_path: f.target.join("packed.zip"),
            snapshot: None,
            target_dir: unpacked.clone(),
            options: crate::archive_extract::ArchiveExtractOptions::default(),
            selection: crate::archive_extract::ArchiveExtractSelection::All,
            sandbox: None,
        };
        let report = request
            .prepare(&Cancel::new())
            .unwrap()
            .execute(None, &mut |_| {})
            .unwrap();
        assert_eq!((report.files, report.directories, report.bytes), (2, 1, 10));
        assert_eq!(fs::read(unpacked.join("good.txt")).unwrap(), b"good");
        assert_eq!(
            fs::read(unpacked.join("folder/good.txt")).unwrap(),
            b"nested"
        );
        assert_eq!(fs::read_dir(&unpacked).unwrap().count(), 2);
        assert_eq!(fs::read_dir(unpacked.join("folder")).unwrap().count(), 1);
        assert_eq!(fs::read(f.source.join("a:b.txt")).unwrap(), b"omitted file");
        assert_eq!(
            fs::read(f.source.join("bad:dir/child")).unwrap(),
            b"omitted subtree"
        );
        assert_eq!(
            fs::read(f.source.join("folder/bad:child.txt")).unwrap(),
            b"omitted child"
        );
        assert_eq!(
            fs::read(f.source.join("folder/bad:dir/child")).unwrap(),
            b"omitted nested subtree"
        );
    }
}

#[test]
fn archive_pack_roundtrip_content_empty_directory_and_zip64_stream() {
    for compression in [
        ArchivePackCompression::Stored,
        ArchivePackCompression::Deflate,
    ] {
        let f = Fixture::new();
        fs::create_dir(f.source.join("empty")).unwrap();
        let content = b"ZIP stream content\0".repeat(9000);
        fs::write(f.source.join("file"), &content).unwrap();
        let prepared = f.prepare(&["empty", "file"], &Cancel::new(), compression);
        assert_eq!(
            (prepared.preview.files, prepared.preview.directories),
            (1, 1)
        );
        assert!(prepared
            .preview
            .issues
            .iter()
            .any(|issue| issue.path.ends_with("file")
                && issue.reason.contains("Sparse")
                && !issue.content_omitted));
        let report = prepared.execute(None, &mut |_| {}).unwrap();
        assert_eq!(report.status, ArchivePackStatus::Complete);
        assert_eq!(report.source_bytes, content.len() as u64);
        let bytes = fs::read(f.target.join("packed.zip")).unwrap();
        assert_eq!(report.archive_bytes, bytes.len() as u64);
        assert!(
            bytes.windows(4).any(|part| part == [1, 0, 16, 0]),
            "ZIP64-Größenextrafeld ist vor dem Inhaltsstream vorhanden"
        );
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(zip.len(), 2);
        assert!(zip.by_name("empty/").unwrap().is_dir());
        let mut read = Vec::new();
        zip.by_name("file").unwrap().read_to_end(&mut read).unwrap();
        assert_eq!(read, content);
        assert_eq!(f.target_files().len(), 1);
    }
}

#[test]
fn archive_pack_sparse_large_source_has_no_size_gate_or_pre_hash() {
    let f = Fixture::new();
    let file = fs::File::create(f.source.join("sparse")).unwrap();
    file.set_len(u32::MAX as u64 + 17).unwrap();
    let cancel = Cancel::new();
    let prepared = f.prepare(&["sparse"], &cancel, ArchivePackCompression::Stored);
    assert_eq!(prepared.preview.source_bytes, u32::MAX as u64 + 17);
    cancel.cancel();
    assert_eq!(
        prepared.execute(None, &mut |_| {}).unwrap().status,
        ArchivePackStatus::Cancelled
    );
    f.assert_no_output();
}

#[test]
fn archive_pack_cancel_mid_stream_cleans_only_owned_temps() {
    let f = Fixture::new();
    let bytes = vec![42; COPY_BUFFER_BYTES * 4];
    fs::write(f.source.join("file"), &bytes).unwrap();
    let cancel = Cancel::new();
    let prepared = f.prepare(&["file"], &cancel, ArchivePackCompression::Deflate);
    let report = prepared
        .execute(None, &mut |event| {
            if event.source_bytes > 0 {
                cancel.cancel();
            }
        })
        .unwrap();
    assert_eq!(report.status, ArchivePackStatus::Cancelled);
    f.assert_no_output();
    assert_eq!(fs::read(f.source.join("file")).unwrap(), bytes);
}

#[test]
fn archive_pack_late_collision_preserves_foreign_target() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"source").unwrap();
    let prepared = f.prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored);
    let failure = prepared
        .execute(None, &mut |event| {
            if matches!(event.phase, ArchivePackPhase::Committing) {
                fs::write(f.target.join("packed.zip"), b"foreign").unwrap();
            }
        })
        .unwrap_err();
    assert_eq!(error_code(&failure.error), ErrorCode::AlreadyExists);
    assert_eq!(fs::read(f.target.join("packed.zip")).unwrap(), b"foreign");
    assert_eq!(f.target_files().len(), 1);
}

#[test]
fn archive_pack_source_swap_and_short_stream_fail_without_target() {
    for swap in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), vec![7; COPY_BUFFER_BYTES * 4]).unwrap();
        let prepared = f.prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored);
        let mut changed = false;
        let failure = prepared
            .execute(None, &mut |event| {
                if !changed && event.source_bytes > 0 {
                    changed = true;
                    if swap {
                        fs::rename(f.source.join("file"), f.source.join("old")).unwrap();
                        fs::write(f.source.join("file"), b"replacement").unwrap();
                    } else {
                        fs::OpenOptions::new()
                            .write(true)
                            .open(f.source.join("file"))
                            .unwrap()
                            .set_len(1)
                            .unwrap();
                    }
                }
            })
            .unwrap_err();
        assert_eq!(error_code(&failure.error), ErrorCode::PlanDrift);
        f.assert_no_output();
    }
}

#[test]
fn archive_pack_symlinks_unsafe_names_and_nested_aliases_are_pathwise_omitted() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("folder")).unwrap();
    fs::write(f.source.join("folder/good"), b"good").unwrap();
    fs::write(f.source.join("folder/back\\slash"), b"bad").unwrap();
    symlink("good", f.source.join("folder/link")).unwrap();
    let prepared = f.prepare(&["folder"], &Cancel::new(), ArchivePackCompression::Stored);
    assert!(prepared
        .preview
        .issues
        .iter()
        .any(|issue| issue.path.ends_with("link")));
    assert!(prepared
        .preview
        .issues
        .iter()
        .any(|issue| issue.path.contains("back\\slash")));
    prepared.execute(None, &mut |_| {}).unwrap();
    let mut zip =
        zip::ZipArchive::new(fs::File::open(f.target.join("packed.zip")).unwrap()).unwrap();
    assert_eq!(zip.len(), 2);
    assert!(zip.by_name("folder/good").is_ok());
}

#[test]
fn archive_pack_rejects_escaping_selection_and_self_capture() {
    let f = Fixture::new();
    fs::create_dir(f.source.join("folder")).unwrap();
    assert!(prepare_archive_pack(
        &f.source,
        &[f.source.join("../outside")],
        &f.target,
        "a.zip",
        ArchivePackOptions::default(),
        None,
        &Cancel::new()
    )
    .is_err());
    assert!(prepare_archive_pack(
        &f.source,
        &[f.source.join("folder")],
        &f.source.join("folder"),
        "a.zip",
        ArchivePackOptions::default(),
        None,
        &Cancel::new()
    )
    .is_err());
    f.assert_no_output();
}

struct CommitFaultVfs {
    inner: LocalVfs,
    lose_reply: bool,
}
impl Vfs for CommitFaultVfs {
    fn list(&self, p: &Path) -> io::Result<Vec<Entry>> {
        self.inner.list(p)
    }
    fn list_complete(&self, p: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
        self.inner.list_complete(p)
    }
    fn stat(&self, p: &Path) -> io::Result<Entry> {
        self.inner.stat(p)
    }
    fn exists(&self, p: &Path) -> bool {
        self.inner.exists(p)
    }
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.inner.create_dir_all(p)
    }
    fn create_dir(&self, p: &Path) -> io::Result<()> {
        self.inner.create_dir(p)
    }
    fn open_read(&self, p: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.inner.open_read(p)
    }
    fn open_write(&self, p: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.inner.open_write(p)
    }
    fn open_write_new(&self, p: &Path) -> io::Result<Box<dyn Write + Send>> {
        self.inner.open_write_new(p)
    }
    fn sync_file(&self, p: &Path) -> io::Result<()> {
        self.inner.sync_file(p)
    }
    fn sync_dir(&self, _p: &Path) -> io::Result<()> {
        Err(io::Error::other("injected directory flush failure"))
    }
    fn set_mtime(&self, p: &Path, t: i64) -> io::Result<()> {
        self.inner.set_mtime(p, t)
    }
    fn set_mode(&self, p: &Path, m: u32) -> io::Result<()> {
        self.inner.set_mode(p, m)
    }
    fn remove_file(&self, _p: &Path) -> io::Result<()> {
        panic!("Unklarer Commit darf nichts entfernen")
    }
    fn remove_dir_all(&self, _p: &Path) -> io::Result<()> {
        panic!("ZIP besitzt keinen rekursiven Löschweg")
    }
    fn rename(&self, p: &Path, q: &Path) -> io::Result<()> {
        self.inner.rename(p, q)
    }
    fn rename_noreplace(&self, p: &Path, q: &Path) -> io::Result<()> {
        self.inner.rename_noreplace(p, q)
    }
    fn rename_noreplace_with_removal_proof(
        &self,
        p: &Path,
        q: &Path,
    ) -> Result<Option<crate::vfs::ProofListEntry>, crate::vfs::MutationFailure> {
        let proof = self.inner.rename_noreplace_with_removal_proof(p, q)?;
        if self.lose_reply {
            Err(crate::vfs::MutationFailure::OutcomeUnclear(
                io::Error::other("injected lost reply"),
            ))
        } else {
            Ok(proof)
        }
    }
    fn anchor_is_current(&self) -> io::Result<bool> {
        self.inner.anchor_is_current()
    }
    fn local_root_identity(&self, p: &Path) -> io::Result<LocalRootIdentity> {
        self.inner.local_root_identity(p)
    }
    fn file_commit_class_at(&self, _p: &Path) -> io::Result<FileCommitClass> {
        Ok(FileCommitClass::Assured)
    }
}

#[test]
fn archive_pack_unclear_commit_and_flush_keep_target_and_recovery_paths() {
    for lose_reply in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"recoverable").unwrap();
        let prepared = f.prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored);
        assert_eq!(prepared.preview.commit_class, FileCommitClass::Assured);
        let dst = CommitFaultVfs {
            inner: LocalVfs::rooted(&f.target).unwrap(),
            lose_reply,
        };
        let failure = prepared.execute_with_target(&dst, &mut |_| {}).unwrap_err();
        assert_eq!(failure.report.status, ArchivePackStatus::OutcomeUnclear);
        assert_eq!(error_code(&failure.error), ErrorCode::OutcomeUnclear);
        assert!(failure
            .report
            .recovery_paths
            .iter()
            .any(|path| path.ends_with("packed.zip")));
        let mut zip =
            zip::ZipArchive::new(fs::File::open(f.target.join("packed.zip")).unwrap()).unwrap();
        let mut bytes = Vec::new();
        zip.by_name("file")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"recoverable");
    }
}

#[test]
fn archive_pack_cancel_without_private_staging_reports_retained_temp() {
    let f = Fixture::new();
    fs::set_permissions(&f.target, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(f.source.join("file"), vec![9; COPY_BUFFER_BYTES * 2]).unwrap();
    let cancel = Cancel::new();
    let prepared = f.prepare(&["file"], &cancel, ArchivePackCompression::Stored);
    let report = prepared
        .execute(None, &mut |event| {
            if event.source_bytes > 0 {
                cancel.cancel();
            }
        })
        .unwrap();
    assert_eq!(report.status, ArchivePackStatus::Cancelled);
    assert!(!f.target.join("packed.zip").exists());
    assert_eq!(report.recovery_paths.len(), 1);
    assert!(Path::new(&report.recovery_paths[0]).is_file());
}

#[test]
fn archive_pack_readback_detects_modified_archive_before_commit() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"content").unwrap();
    let prepared = f.prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored);
    let failure = prepared
        .execute(None, &mut |event| {
            if matches!(event.phase, ArchivePackPhase::Verifying) {
                let temp = f
                    .target_files()
                    .into_iter()
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .contains("fileop-tmp")
                    })
                    .unwrap();
                fs::write(temp, b"corrupted archive").unwrap();
            }
        })
        .unwrap_err();
    assert_eq!(error_code(&failure.error), ErrorCode::InvalidData);
    f.assert_no_output();
}

#[test]
fn archive_pack_bound_target_root_and_source_identity_drift_before_write() {
    for target_swap in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"bound").unwrap();
        let prepared = f.prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored);
        if target_swap {
            fs::rename(&f.target, f.target.with_file_name("old-target")).unwrap();
            fs::create_dir(&f.target).unwrap();
        } else {
            fs::rename(f.source.join("file"), f.source.join("old-file")).unwrap();
            fs::write(f.source.join("file"), b"other").unwrap();
        }
        let failure = prepared.execute(None, &mut |_| {}).unwrap_err();
        assert_eq!(error_code(&failure.error), ErrorCode::PlanDrift);
        f.assert_no_output();
    }
}

#[test]
fn archive_pack_ambiguous_selected_names_do_not_hide_independent_content() {
    let f = Fixture::new();
    fs::write(f.source.join("alias"), b"ambiguous").unwrap();
    fs::write(f.source.join("good"), b"included").unwrap();
    let prepared = f.prepare(
        &["alias", "alias", "good"],
        &Cancel::new(),
        ArchivePackCompression::Stored,
    );
    assert_eq!(prepared.preview.files, 1);
    assert!(prepared
        .preview
        .issues
        .iter()
        .any(|issue| issue.path.contains("alias") && issue.reason.contains("gesamte Gruppe")));
    prepared.execute(None, &mut |_| {}).unwrap();
    let mut zip =
        zip::ZipArchive::new(fs::File::open(f.target.join("packed.zip")).unwrap()).unwrap();
    assert_eq!(zip.len(), 1);
    assert!(zip.by_name("good").is_ok());
}

#[test]
fn archive_pack_non_utf8_and_special_file_omissions_preserve_valid_selection() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new();
    let raw = f.source.join(std::ffi::OsString::from_vec(vec![0xff]));
    // APFS lehnt diesen Namen bereits bei der Anlage mit EILSEQ ab. Der
    // Backendrequest muss ihn trotzdem ohne Dekodierung als ausgelassen bilanzieren.
    #[cfg(target_os = "linux")]
    fs::write(&raw, b"raw").unwrap();
    fs::write(f.source.join("good"), b"usable").unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(f.source.join("socket")).unwrap();
    let prepared = prepare_archive_pack(
        &f.source,
        &[raw, f.source.join("socket"), f.source.join("good")],
        &f.target,
        "packed.zip",
        ArchivePackOptions::default(),
        None,
        &Cancel::new(),
    )
    .unwrap();
    assert_eq!(prepared.preview.files, 1);
    assert_eq!(
        prepared
            .preview
            .issues
            .iter()
            .filter(|issue| issue.content_omitted)
            .count(),
        2
    );
    assert!(prepared
        .preview
        .issues
        .iter()
        .any(|issue| issue.path.contains("Rohname hex: ff")));
    prepared.execute(None, &mut |_| {}).unwrap();
    let mut zip =
        zip::ZipArchive::new(fs::File::open(f.target.join("packed.zip")).unwrap()).unwrap();
    assert_eq!(zip.len(), 1);
    assert!(zip.by_name("good").is_ok());
}

#[test]
fn archive_pack_invalid_request_is_rejected_before_nonexistent_source_access() {
    let f = Fixture::new();
    let error = match prepare_archive_pack(
        &f.source.join("absent"),
        &[f.source.join("absent/file")],
        &f.target,
        "../escape.zip",
        ArchivePackOptions::default(),
        None,
        &Cancel::new(),
    ) {
        Ok(_) => panic!("invalid name accepted"),
        Err(error) => error,
    };
    assert_eq!(error_code(&error), ErrorCode::InvalidInput);
    f.assert_no_output();
}

#[test]
fn archive_pack_unusable_selected_directory_is_omitted_without_blocking_valid_file() {
    let f = Fixture::new();
    fs::create_dir(f.source.join(" padded ")).unwrap();
    fs::write(f.source.join("good"), b"ok").unwrap();
    let prepared = f.prepare(
        &[" padded ", "good"],
        &Cancel::new(),
        ArchivePackCompression::Stored,
    );
    assert_eq!(prepared.preview.files, 1);
    assert_eq!(prepared.preview.directories, 0);
    assert!(prepared
        .preview
        .issues
        .iter()
        .any(|issue| issue.content_omitted && issue.path.contains(" padded ")));
    prepared.execute(None, &mut |_| {}).unwrap();
}

#[test]
fn archive_pack_file_only_final_checks_after_readback_and_before_commit() {
    for committing in [false, true] {
        let f = Fixture::new();
        fs::write(f.source.join("file"), b"original").unwrap();
        let prepared = f.prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored);
        let failure = prepared
            .execute(None, &mut |event| {
                if matches!(event.phase, ArchivePackPhase::Committing) && committing
                    || matches!(event.phase, ArchivePackPhase::Verifying) && !committing
                {
                    fs::write(f.source.join("file"), b"changed!").unwrap();
                }
            })
            .unwrap_err();
        assert_eq!(error_code(&failure.error), ErrorCode::PlanDrift);
        f.assert_no_output();
    }
}

#[test]
fn archive_pack_versions_digest_detects_same_size_change_with_restored_mtime() {
    let f = Fixture::new();
    fs::write(f.source.join("file"), b"original").unwrap();
    let before = f
        .prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored)
        .binding()
        .unwrap();
    let old_time = filetime::FileTime::from_last_modification_time(
        &fs::metadata(f.source.join("file")).unwrap(),
    );
    fs::write(f.source.join("file"), b"changed!").unwrap();
    filetime::set_file_mtime(f.source.join("file"), old_time).unwrap();
    let after = f
        .prepare(&["file"], &Cancel::new(), ArchivePackCompression::Stored)
        .binding()
        .unwrap();
    assert_ne!(before.digest, after.digest);
}
