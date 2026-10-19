//! Gemeinsamer privater Claim-Kern für bereits gehaltene, künstlich injizierbare
//! Layouts. Die native Ankerwahl und die produktive Transfer-/Recovery-Anbindung
//! bleiben getrennte Schritte. Kein Drop führt eine Dateiwirkung aus.
use super::*;
#[cfg(target_os = "macos")]
use crate::local_fs::dir_flags;
use crate::local_fs::{mount_identity_from_fd, RootHandle};
use crate::metadata::{Acl, Nfs4AceKind, PosixAclTag};
use crate::vfs::LocalMountIdentity;
use rustix::fs::{
    fchmod, fstat, fsync, mkdirat, openat, renameat, renameat_with, statat, unlinkat, AtFlags,
    Mode, OFlags, RenameFlags,
};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod lifecycle_tests;
mod recovery;
mod transfer;
pub(in crate::trash) use recovery::{recover, restore_legacy_claim, LegacyJournal};

pub(in crate::trash) enum TransferOutcome {
    Durable,
    /// Positiver Ziel-Readback, aber kein belegter dauerhafter Abschluss.
    Observed(io::Error),
}

#[derive(Clone)]
pub(in crate::trash) struct HeldLayout {
    pub(in crate::trash) anchor: RootHandle,
    pub(in crate::trash) files: RootHandle,
    pub(in crate::trash) info: Option<RootHandle>,
    pub(in crate::trash) journal: RootHandle,
    pub(in crate::trash) sandbox: Option<RootHandle>,
}

pub(in crate::trash) struct ClaimRun {
    layout: HeldLayout,
    sources: Vec<Option<RootHandle>>,
    area: Option<RootHandle>,
    journal: Journal,
    journal_name: OsString,
    journal_identity: Option<ObjectIdentity>,
    poisoned: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::trash) enum ClaimOutcome {
    Claimed,
    ChangedAndRestored,
}

impl ClaimRun {
    #[cfg(test)]
    pub(in crate::trash) fn prepare(layout: HeldLayout, items: &[PathBuf]) -> io::Result<Self> {
        Self::prepare_selected(layout, items, None)
    }

    pub(in crate::trash) fn prepare_selected(
        layout: HeldLayout,
        items: &[PathBuf],
        selected: Option<&[(RootHandle, super::super::Identity)]>,
    ) -> io::Result<Self> {
        super::super::validate_batch(items)?;
        if selected.is_some_and(|values| values.len() != items.len()) {
            return Err(invalid("Unvollständige Quellauswahl"));
        }
        let journal_parent = directory_evidence(&layout.journal, false)?;
        let anchor = directory_evidence(&layout.anchor, false)?;
        let files = directory_evidence(&layout.files, true)?;
        let info = layout
            .info
            .as_ref()
            .map(|root| directory_evidence(root, true))
            .transpose()?;
        if anchor.mount != files.mount
            || info
                .as_ref()
                .is_some_and(|value| value.mount != files.mount)
        {
            return Err(denied("Claim, Daten und Info brauchen denselben Mount"));
        }
        for root in [&layout.anchor, &layout.files, &layout.journal]
            .into_iter()
            .chain(layout.info.as_ref())
        {
            if items.iter().any(|item| root.path().starts_with(item)) {
                return Err(denied("Hilfswurzel liegt im ausgewählten Teilbaum"));
            }
            if let Some(sandbox) = &layout.sandbox {
                let checked = RootHandle::open_beneath(sandbox, root.path())?;
                if checked.identity() != root.identity()
                    || checked.current_mount_identity()? != root.current_mount_identity()?
                {
                    return Err(denied("Hilfswurzel verlässt den gehaltenen Sandbox-Root"));
                }
            }
        }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let run_id: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let intent = AreaIntent {
            anchor,
            name: ByteName(format!(".doppeldecker-trash-{run_id}").into_bytes()),
            marker_name: ByteName(b".doppeldecker-trash-marker".to_vec()),
            marker_nonce: nonce,
        };
        let mut sources = Vec::new();
        let mut entries = Vec::new();
        let mut targets = std::collections::HashSet::new();
        for (index, item) in items.iter().enumerate() {
            let parent = item
                .parent()
                .ok_or_else(|| denied("Quell-Elternordner fehlt"))?;
            let root = match selected {
                Some(values) if values[index].0.path() == parent => values[index].0.clone(),
                Some(_) => return Err(denied("Gehaltene Quellauswahl passt nicht zum Elternpfad")),
                None => match &layout.sandbox {
                    Some(sandbox) => RootHandle::open_beneath(sandbox, parent)?,
                    None => RootHandle::open_nofollow(parent)?,
                },
            };
            if selected.is_some() {
                if let Some(sandbox) = &layout.sandbox {
                    let checked = RootHandle::open_beneath(sandbox, parent)?;
                    if checked.identity() != root.identity()
                        || checked.current_mount_identity()? != root.current_mount_identity()?
                    {
                        return Err(denied("Gehaltene Quellauswahl verlässt die Sandboxbindung"));
                    }
                }
            }
            let source_parent = snapshot(&root)?;
            if source_parent.mount != files.mount {
                return Err(denied("Quelle liegt auf anderem Mount"));
            }
            let name = item.file_name().ok_or_else(|| denied("Quellname fehlt"))?;
            let expected = identity(root.fd(), name)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            if selected.is_some_and(|values| {
                let value = &values[index].1;
                expected
                    != (ObjectIdentity {
                        device: value.device,
                        inode: value.inode,
                        file_type: value.file_type,
                    })
            }) {
                return Err(denied(
                    "Quellauswahl änderte sich während der Layoutvorbereitung",
                ));
            }
            check_entry_mount(&root, name, &expected)?;
            let limit = if layout.info.is_some() {
                255 - b".trashinfo".len()
            } else {
                255
            };
            let mut target = None;
            for suffix in 0..100_000 {
                let candidate = super::super::collision_name(name, suffix, limit);
                if identity(layout.files.fd(), &candidate)?.is_some() {
                    continue;
                }
                if let Some(info_root) = &layout.info {
                    if identity(info_root.fd(), &super::super::info_name(&candidate))?.is_some() {
                        continue;
                    }
                }
                if targets.insert(candidate.clone()) {
                    target = Some(candidate);
                    break;
                }
            }
            let target = target.ok_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists))?;
            entries.push(Entry {
                source_parent,
                source_name: byte_name(name),
                private_name: ByteName(format!("entry-{index}").into_bytes()),
                expected,
                claimed: None,
                trash_name: byte_name(&target),
                info: None,
                phase: EntryPhase::Planned,
            });
            sources.push(Some(root));
        }
        let journal = Journal {
            version: 2,
            run_id: run_id.clone(),
            effective_uid: unsafe { libc::geteuid() },
            journal_parent,
            sandbox: layout.sandbox.as_ref().map(snapshot).transpose()?,
            area_intent: Some(intent),
            area: None,
            trash: TrashEvidence {
                platform: if layout.info.is_some() {
                    Platform::Xdg
                } else {
                    Platform::Macos
                },
                files,
                info,
            },
            entries,
            phase: BatchPhase::Planned,
        };
        journal.validate()?;
        let mut run = Self {
            layout,
            sources,
            area: None,
            journal,
            journal_name: format!("trash-{run_id}.json").into(),
            journal_identity: None,
            poisoned: false,
        };
        // Vor mkdir ist auch der genaue Anlageort dauerhaft bekannt. Ein Crash
        // vor dem Marker hinterlässt damit einen sichtbaren, aber nie blind
        // löschbaren Rest; die Quelle ist zu diesem Zeitpunkt noch unverändert.
        let initial = run.journal.clone();
        run.persist(&initial)?;
        if let Err(error) = run.prepare_area() {
            let journal = run.layout.journal.path().join(&run.journal_name);
            let area = run.layout.anchor.path().join(os_name(
                &run.journal.area_intent.as_ref().expect("Anlageort").name,
            ));
            return Err(crate::error::with_recovery_paths(
                error,
                &[
                    journal.to_string_lossy().into_owned(),
                    area.to_string_lossy().into_owned(),
                ],
            ));
        }
        Ok(run)
    }

    fn prepare_area(&mut self) -> io::Result<()> {
        self.check_layout()?;
        let intent = self
            .journal
            .area_intent
            .as_ref()
            .expect("vor planned gebunden")
            .clone();
        let path = self.layout.anchor.path().join(os_name(&intent.name));
        boundary("before_mkdir", &path)?;
        mkdirat(
            self.layout.anchor.fd(),
            os_name(&intent.name),
            Mode::from_raw_mode(0o700),
        )?;
        let child = RootHandle::open_beneath(&self.layout.anchor, &path)?;
        // Der Anlageort wurde bereits VOR mkdir auf wirksame und vererbbare
        // Fremdrechte geprüft. Dieses Entfernen betrifft nur das neue Kind,
        // niemals ACL/Modus des normalen Trash-Roots oder der Quelldatei.
        boundary("after_mkdir", &path)?;
        directory_evidence(&child, true)?;
        crate::acl::write_fd_exact(child.fd(), None)?;
        fchmod(child.fd(), Mode::from_raw_mode(0o700))?;
        let evidence = directory_evidence(&child, true)?;
        if evidence.mount != intent.anchor.mount
            || evidence.identity.device != intent.anchor.identity.device
        {
            return Err(denied("Neuer Bereich liegt auf anderem Mount"));
        }
        self.check_layout()?;
        let marker = create_private_file(child.fd(), os_name(&intent.marker_name))?;
        let marker_identity = from_stat(&fstat(&marker)?);
        normalize_private_file(&marker)?;
        let content = marker_content(&intent.marker_nonce, self.journal.effective_uid);
        let mut file = File::from(marker);
        file.write_all(&content)?;
        boundary("marker_sync", &path)?;
        file.sync_all()?;
        fsync(child.fd())?;
        fsync(self.layout.anchor.fd())?;
        let area = PrivateArea {
            anchor: intent.anchor,
            name: intent.name,
            identity: evidence.identity,
            owner: evidence.owner,
            mode: evidence.mode,
            acl: evidence.acl,
            marker_name: intent.marker_name,
            marker_identity,
            marker_nonce: intent.marker_nonce,
            marker_digest: *blake3::hash(&content).as_bytes(),
        };
        self.area = Some(child);
        let mut next = self.journal.clone();
        next.area = Some(area);
        next.phase = BatchPhase::AreaReady;
        self.persist(&next)?;
        self.journal = next;
        self.check_area()
    }

    pub(in crate::trash) fn claim(&mut self, index: usize) -> io::Result<ClaimOutcome> {
        self.check_area()?;
        let entry = self
            .journal
            .entries
            .get(index)
            .ok_or_else(|| denied("Unbekannter Claim-Eintrag"))?
            .clone();
        if entry.phase != EntryPhase::Planned {
            return Err(denied("Eintrag wurde bereits beansprucht"));
        }
        self.check_source(index)?;
        let mut next = self.journal.clone();
        next.entries[index].phase = EntryPhase::ClaimIntent;
        self.persist(&next)?;
        self.journal = next;
        boundary("before_claim", self.source(index)?.path())?;
        self.check_area()?;
        self.check_source(index)?;
        let area = self.area.as_ref().expect("area_ready").clone();
        renameat_with(
            self.source(index)?.fd(),
            os_name(&entry.source_name),
            area.fd(),
            os_name(&entry.private_name),
            RenameFlags::NOREPLACE,
        )?;
        // Ab hier darf kein Fehler eine zweite Wirkung auslösen. Der dauerhafte
        // Intent und die privaten Bytes bleiben für den späteren Leser erhalten.
        let observed = (|| {
            boundary("after_claim", area.path())?;
            let actual = identity(area.fd(), os_name(&entry.private_name))?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            boundary("claim_sync", area.path())?;
            fsync(self.source(index)?.fd())?;
            fsync(area.fd())?;
            let mut next = self.journal.clone();
            next.entries[index].claimed = Some(actual.clone());
            next.entries[index].phase = if actual == entry.expected {
                EntryPhase::Claimed
            } else {
                EntryPhase::Drifted
            };
            self.persist(&next)?;
            self.journal = next;
            Ok(actual)
        })();
        let actual = match observed {
            Ok(actual) => actual,
            Err(error) => return Err(self.unclear(error)),
        };
        if actual != entry.expected {
            self.restore(index)?;
            return Ok(ClaimOutcome::ChangedAndRestored);
        }
        Ok(ClaimOutcome::Claimed)
    }

    pub(in crate::trash) fn restore(&mut self, index: usize) -> io::Result<()> {
        self.check_area()?;
        self.check_source(index)?;
        let entry = self
            .journal
            .entries
            .get(index)
            .ok_or_else(|| denied("Unbekannter Restore-Eintrag"))?
            .clone();
        if !matches!(
            entry.phase,
            EntryPhase::Claimed
                | EntryPhase::Drifted
                | EntryPhase::RestoreIntent
                | EntryPhase::InfoPublishIntent
                | EntryPhase::InfoReady
        ) {
            return Err(denied(
                "Eintrag besitzt keinen rücksetzbaren privaten Claim",
            ));
        }
        let actual = entry
            .claimed
            .as_ref()
            .ok_or_else(|| denied("Beobachtete Claim-Identität fehlt"))?;
        let area = self.area.as_ref().expect("area_ready").clone();
        if identity(area.fd(), os_name(&entry.private_name))?.as_ref() != Some(actual) {
            return Err(denied("Privater Claim änderte seine Identität"));
        }
        let mut next = self.journal.clone();
        next.entries[index].phase = EntryPhase::RestoreIntent;
        self.persist(&next)?;
        self.journal = next;
        boundary("before_restore", self.source(index)?.path())?;
        self.check_area()?;
        self.check_source(index)?;
        let area = self.area.as_ref().expect("area_ready").clone();
        if identity(area.fd(), os_name(&entry.private_name))?.as_ref() != Some(actual) {
            return Err(denied("Privater Claim änderte seine Identität"));
        }
        renameat_with(
            area.fd(),
            os_name(&entry.private_name),
            self.source(index)?.fd(),
            os_name(&entry.source_name),
            RenameFlags::NOREPLACE,
        )?;
        let result = (|| {
            boundary("restore_sync", self.source(index)?.path())?;
            if identity(self.source(index)?.fd(), os_name(&entry.source_name))?.as_ref()
                != Some(actual)
                || identity(area.fd(), os_name(&entry.private_name))?.is_some()
            {
                return Err(denied("Restore-Abschluss nicht belegt"));
            }
            fsync(area.fd())?;
            fsync(self.source(index)?.fd())?;
            let mut next = self.journal.clone();
            next.entries[index].phase = EntryPhase::Restored;
            self.persist(&next)?;
            self.journal = next;
            Ok(())
        })();
        result.map_err(|error| self.unclear(error))
    }

    fn source(&self, index: usize) -> io::Result<&RootHandle> {
        self.sources
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| denied("Quell-Elternhandle für diese Phase fehlt"))
    }

    fn check_source(&self, index: usize) -> io::Result<()> {
        let root = self.source(index)?;
        if snapshot(root)? != self.journal.entries[index].source_parent {
            return Err(denied("Quell-Elternbindung änderte sich"));
        }
        Ok(())
    }

    fn check_layout(&self) -> io::Result<()> {
        if self.poisoned {
            return Err(denied("Ungeklärter Lauf verlangt Recovery"));
        }
        if directory_evidence(&self.layout.journal, false)? != self.journal.journal_parent
            || self.layout.sandbox.as_ref().map(snapshot).transpose()? != self.journal.sandbox
            || directory_evidence(&self.layout.anchor, false)?
                != self.journal.area_intent.as_ref().expect("Anlageort").anchor
            || directory_evidence(&self.layout.files, true)? != self.journal.trash.files
            || self
                .layout
                .info
                .as_ref()
                .map(|root| directory_evidence(root, true))
                .transpose()?
                != self.journal.trash.info
        {
            return Err(denied(
                "Gehaltene Layoutrechte oder Identität änderten sich",
            ));
        }
        Ok(())
    }

    fn check_area(&self) -> io::Result<()> {
        self.check_layout()?;
        let expected = self
            .journal
            .area
            .as_ref()
            .ok_or_else(|| denied("Privater Bereich noch nicht dauerhaft bereit"))?;
        let area = self
            .area
            .as_ref()
            .ok_or_else(|| denied("Privater Bereich nicht gehalten"))?;
        let observed = directory_evidence(area, true)?;
        if observed.identity != expected.identity
            || observed.owner != expected.owner
            || observed.mode != expected.mode
            || observed.acl != expected.acl
            || observed.mount != expected.anchor.mount
            || identity(self.layout.anchor.fd(), os_name(&expected.name))?.as_ref()
                != Some(&expected.identity)
        {
            return Err(denied("Privater Bereich wurde ausgetauscht"));
        }
        let fd = openat(
            area.fd(),
            os_name(&expected.marker_name),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        if from_stat(&fstat(&fd)?) != expected.marker_identity {
            return Err(denied("Laufmarker wurde ausgetauscht"));
        }
        let mut content = Vec::new();
        File::from(fd).take(128).read_to_end(&mut content)?;
        if content != marker_content(&expected.marker_nonce, self.journal.effective_uid)
            || blake3::hash(&content).as_bytes() != &expected.marker_digest
        {
            return Err(denied("Laufmarker stimmt nicht mehr überein"));
        }
        Ok(())
    }

    fn persist(&mut self, next: &Journal) -> io::Result<()> {
        next.validate()?;
        self.check_layout()?;
        let payload = serde_json::to_vec(next).map_err(io::Error::other)?;
        if payload.len() as u64 > super::super::MAX_JOURNAL_BYTES {
            return Err(denied("Journal ist zu groß"));
        }
        if identity(self.layout.journal.fd(), &self.journal_name)? != self.journal_identity {
            return Err(denied("Journalname wurde ausgetauscht"));
        }
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temp: OsString = format!(
            ".doppeldecker-trash-journal-{}-{suffix}.tmp",
            self.journal.run_id
        )
        .into();
        let fd = match create_private_file(self.layout.journal.fd(), &temp) {
            Ok(fd) => fd,
            Err(error) => return Err(self.unclear(error)),
        };
        let created = from_stat(&fstat(&fd)?);
        let prepared = (|| {
            normalize_private_file(&fd)?;
            let mut file = File::from(fd);
            file.write_all(&payload)?;
            boundary("journal_sync", self.layout.journal.path())?;
            file.sync_all()?;
            self.check_layout()?;
            if identity(self.layout.journal.fd(), &temp)?.as_ref() != Some(&created)
                || identity(self.layout.journal.fd(), &self.journal_name)? != self.journal_identity
            {
                return Err(denied("Journaltemp oder Zielname änderte sich"));
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            if self.check_layout().is_ok()
                && identity(self.layout.journal.fd(), &temp)
                    .ok()
                    .flatten()
                    .as_ref()
                    == Some(&created)
            {
                let _ = unlinkat(self.layout.journal.fd(), &temp, AtFlags::empty());
                let _ = fsync(self.layout.journal.fd());
            }
            return Err(self.unclear(error));
        }
        let committed = if self.journal_identity.is_some() {
            renameat(
                self.layout.journal.fd(),
                &temp,
                self.layout.journal.fd(),
                &self.journal_name,
            )
        } else {
            renameat_with(
                self.layout.journal.fd(),
                &temp,
                self.layout.journal.fd(),
                &self.journal_name,
                RenameFlags::NOREPLACE,
            )
        };
        if let Err(error) = committed {
            return Err(self.unclear(error.into()));
        }
        self.journal_identity = Some(created);
        if let Err(error) = boundary("journal_parent_sync", self.layout.journal.path())
            .and_then(|()| fsync(self.layout.journal.fd()).map_err(Into::into))
        {
            return Err(self.unclear(error));
        }
        Ok(())
    }

    pub(in crate::trash) fn unclear(&mut self, cause: io::Error) -> io::Error {
        self.poisoned = true;
        let journal = self.layout.journal.path().join(&self.journal_name);
        let area = self.layout.anchor.path().join(os_name(
            &self.journal.area_intent.as_ref().expect("Anlageort").name,
        ));
        let mut paths = vec![
            journal.to_string_lossy().into_owned(),
            area.to_string_lossy().into_owned(),
        ];
        for entry in &self.journal.entries {
            if !matches!(
                entry.phase,
                EntryPhase::Planned
                    | EntryPhase::Untouched
                    | EntryPhase::Trashed
                    | EntryPhase::Restored
            ) {
                paths.push(
                    Path::new(OsStr::from_bytes(&entry.source_parent.path.0))
                        .join(os_name(&entry.source_name))
                        .to_string_lossy()
                        .into_owned(),
                );
                paths.push(
                    self.layout
                        .files
                        .path()
                        .join(os_name(&entry.trash_name))
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            if entry.phase != EntryPhase::Trashed {
                if let (Some(root), Some(info)) = (&self.layout.info, &entry.info) {
                    paths.push(
                        root.path()
                            .join(os_name(&info.published_name))
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        crate::error::with_recovery_paths(
            crate::coded_error(
                io::ErrorKind::Other,
                crate::ErrorCode::OutcomeUnclear,
                format!("Privater Papierkorblauf ist ungeklärt: {cause}"),
            ),
            &paths,
        )
    }
}

fn byte_name(name: &OsStr) -> ByteName {
    ByteName(name.as_bytes().to_vec())
}
fn os_name(name: &ByteName) -> &OsStr {
    OsStr::from_bytes(&name.0)
}
fn from_stat(stat: &rustix::fs::Stat) -> ObjectIdentity {
    let value = super::super::Identity::from_stat(stat);
    ObjectIdentity {
        device: value.device,
        inode: value.inode,
        file_type: value.file_type,
    }
}
fn identity(parent: &OwnedFd, name: &OsStr) -> io::Result<Option<ObjectIdentity>> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(from_stat(&stat))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn snapshot(root: &RootHandle) -> io::Result<DirectoryEvidence> {
    let mount = root.current_mount_identity()?;
    let stat = fstat(root.fd())?;
    let acl = crate::acl::read_fd(root.fd())?;
    #[cfg(target_os = "macos")]
    let mode = u32::from(stat.st_mode) & 0o7777;
    #[cfg(target_os = "linux")]
    let mode = stat.st_mode & 0o7777;
    Ok(DirectoryEvidence {
        path: BytePath(root.path().as_os_str().as_bytes().to_vec()),
        identity: from_stat(&stat),
        mount: mount_evidence(&mount)?,
        owner: stat.st_uid,
        mode,
        acl: acl_evidence(acl.as_ref())?,
    })
}

pub(in crate::trash) fn check_directory(root: &RootHandle, private: bool) -> io::Result<()> {
    directory_evidence(root, private).map(|_| ())
}

pub(in crate::trash) fn check_permission_model(root: &RootHandle) -> io::Result<()> {
    permission_model(&root.current_mount_identity()?)
}

fn directory_evidence(root: &RootHandle, private: bool) -> io::Result<DirectoryEvidence> {
    let mount = root.current_mount_identity()?;
    permission_model(&mount)?;
    let evidence = snapshot(root)?;
    let acl = crate::acl::read_fd(root.fd())?;
    if acl_evidence(acl.as_ref())? != evidence.acl
        || evidence.owner != unsafe { libc::geteuid() }
        || evidence.mode & if private { 0o077 } else { 0o022 } != 0
        || !acl_safe(acl.as_ref(), private)
    {
        return Err(denied(
            "Namensraum ist nicht belegbar privat beziehungsweise exklusiv",
        ));
    }
    // Zwei Reads dürfen keinen wechselnden ACL-Stand als denselben Beleg tarnen.
    if snapshot(root)? != evidence {
        return Err(denied("Namensraumrechte änderten sich während der Prüfung"));
    }
    Ok(evidence)
}

fn acl_safe(acl: Option<&Acl>, private: bool) -> bool {
    match acl {
        None => true,
        Some(Acl::Nfs4(acl)) => acl
            .entries
            .iter()
            .all(|entry| entry.kind == Nfs4AceKind::Deny),
        Some(Acl::Posix(acl)) => {
            let mask = acl
                .access
                .iter()
                .find(|entry| entry.tag == PosixAclTag::Mask)
                .map(|entry| entry.permissions)
                .unwrap_or(7);
            acl.access.iter().all(|entry| {
                if entry.tag == PosixAclTag::UserObject || entry.tag == PosixAclTag::Mask {
                    return true;
                }
                let effective = if entry.tag == PosixAclTag::Other {
                    entry.permissions
                } else {
                    entry.permissions & mask
                };
                effective & if private { 7 } else { 2 } == 0
            })
        }
    }
}

fn permission_model(mount: &LocalMountIdentity) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let known = matches!(mount.fs_type.as_str(), "apfs" | "hfs")
        && mount.mount_flags & (libc::MNT_IGNORE_OWNERSHIP | libc::MNT_RDONLY) as u64 == 0;
    #[cfg(target_os = "linux")]
    let known = {
        match std::fs::read("/sys/fs/selinux/enforce") {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(denied("SELinux benötigt einen belegten Kontextadapter")),
        }
        matches!(
            mount.fs_type.as_str(),
            "0xef53" | "0x58465342" | "0x9123683e" | "0x1021994"
        ) && mount.mount_flags & libc::ST_RDONLY == 0
    };
    if !known {
        return Err(denied("Unbelegtes oder nur lesbares Rechte-/Owner-Modell"));
    }
    Ok(())
}

fn mount_evidence(mount: &LocalMountIdentity) -> io::Result<MountEvidence> {
    #[cfg(target_os = "macos")]
    return Ok(MountEvidence::Macos {
        fsid: mount
            .fsid
            .clone()
            .try_into()
            .map_err(|_| invalid("FSID ist nicht acht Bytes lang"))?,
        volume_id: mount.volume_id.ok_or_else(|| invalid("Volume-ID fehlt"))?,
        fs_type: mount.fs_type.clone(),
        mount_flags: mount.mount_flags,
    });
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let compact = text.trim().replace('-', "");
        if compact.len() != 32 || !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("Boot-ID ist ungültig"));
        }
        let mut boot_id = [0; 16];
        for (index, byte) in boot_id.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&compact[index * 2..index * 2 + 2], 16)
                .map_err(|_| invalid("Boot-ID enthält ungültige Bytes"))?;
        }
        Ok(MountEvidence::Linux {
            boot_id,
            mount_id: mount.mount_id,
            unique_mount_id: mount.unique_mount_id,
            fsid: mount.fsid.clone(),
            fs_type: mount.fs_type.clone(),
            mount_flags: mount.mount_flags,
        })
    }
}

fn check_entry_mount(
    parent: &RootHandle,
    name: &OsStr,
    expected: &ObjectIdentity,
) -> io::Result<()> {
    if expected.device != parent.identity().0 {
        return Err(denied("Ausgewählter Eintrag liegt auf anderem Gerät"));
    }
    #[cfg(target_os = "linux")]
    let fd = openat(
        parent.fd(),
        name,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    #[cfg(target_os = "macos")]
    let fd = if expected.file_type == 0o040000 {
        openat(parent.fd(), name, dir_flags(), Mode::empty())?
    } else {
        return Ok(());
    };
    if from_stat(&fstat(&fd)?) != *expected
        || mount_identity_from_fd(&fd)? != parent.current_mount_identity()?
    {
        return Err(denied("Ausgewählter Eintrag ist Mountpunkt oder driftete"));
    }
    Ok(())
}

fn create_private_file(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let fd = openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    Ok(fd)
}

fn normalize_private_file(fd: &OwnedFd) -> io::Result<()> {
    crate::acl::write_fd_exact(fd, None)?;
    fchmod(fd, Mode::from_raw_mode(0o600))?;
    Ok(())
}

fn acl_evidence(acl: Option<&Acl>) -> io::Result<AclEvidence> {
    acl.map(|value| {
        serde_json::to_vec(value).map(|bytes| AclEvidence::Digest(*blake3::hash(&bytes).as_bytes()))
    })
    .transpose()
    .map_err(io::Error::other)
    .map(|value| value.unwrap_or(AclEvidence::Absent))
}

fn marker_content(nonce: &[u8; 16], uid: u32) -> Vec<u8> {
    let mut value = b"Doppeldecker private trash v2\n".to_vec();
    value.extend_from_slice(nonce);
    value.extend_from_slice(&uid.to_le_bytes());
    value
}

#[cfg(test)]
type TestHook = Box<dyn FnMut(&str, &Path) -> io::Result<()>>;
#[cfg(test)]
thread_local! { static HOOK: std::cell::RefCell<Option<TestHook>> = std::cell::RefCell::new(None); }
pub(in crate::trash) fn boundary(point: &str, path: &Path) -> io::Result<()> {
    #[cfg(test)]
    return HOOK.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .map_or(Ok(()), |hook| hook(point, path))
    });
    #[cfg(not(test))]
    {
        let _ = (point, path);
        Ok(())
    }
}

#[cfg(test)]
pub(in crate::trash) mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    pub(super) struct Fixture {
        _temp: tempfile::TempDir,
        pub(super) root: PathBuf,
        pub(super) source: PathBuf,
        pub(super) layout: HeldLayout,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            for (name, mode) in [
                ("source", 0o777),
                ("anchor", 0o755),
                ("files", 0o700),
                ("info", 0o700),
                ("journal", 0o700),
            ] {
                let path = root.join(name);
                std::fs::create_dir(&path).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            let source = root.join("source/file.txt");
            std::fs::write(&source, b"original").unwrap();
            let layout = HeldLayout {
                anchor: RootHandle::open_nofollow(&root.join("anchor")).unwrap(),
                files: RootHandle::open_nofollow(&root.join("files")).unwrap(),
                info: if cfg!(target_os = "linux") {
                    Some(RootHandle::open_nofollow(&root.join("info")).unwrap())
                } else {
                    None
                },
                journal: RootHandle::open_nofollow(&root.join("journal")).unwrap(),
                sandbox: Some(RootHandle::open_nofollow(&root).unwrap()),
            };
            Self {
                _temp: temp,
                root,
                source,
                layout,
            }
        }
        pub(super) fn prepare(&self) -> ClaimRun {
            ClaimRun::prepare(self.layout.clone(), std::slice::from_ref(&self.source)).unwrap()
        }
        pub(super) fn stored(&self, run: &ClaimRun) -> Journal {
            decode(&std::fs::read(self.root.join("journal").join(&run.journal_name)).unwrap())
                .unwrap()
        }
    }

    pub(in crate::trash) struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }
    pub(in crate::trash) fn hook(
        value: impl FnMut(&str, &Path) -> io::Result<()> + 'static,
    ) -> HookGuard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(value)));
        HookGuard
    }

    #[test]
    fn privater_claim_und_restore_erhalten_identitaet_inhalt_und_modus() {
        let fixture = Fixture::new();
        std::fs::set_permissions(&fixture.source, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = std::fs::symlink_metadata(&fixture.source).unwrap();
        let mut run = fixture.prepare();
        assert_eq!(fixture.stored(&run).phase, BatchPhase::AreaReady);
        assert_eq!(run.claim(0).unwrap(), ClaimOutcome::Claimed);
        let claim = run.area.as_ref().unwrap().path().join("entry-0");
        assert!(!fixture.source.exists());
        assert_eq!(std::fs::read(&claim).unwrap(), b"original");
        assert_eq!(fixture.stored(&run).entries[0].phase, EntryPhase::Claimed);
        run.restore(0).unwrap();
        let after = std::fs::symlink_metadata(&fixture.source).unwrap();
        assert_eq!(
            (after.dev(), after.ino(), after.mode()),
            (before.dev(), before.ino(), before.mode())
        );
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert_eq!(fixture.stored(&run).entries[0].phase, EntryPhase::Restored);
    }

    #[test]
    fn uebergebene_quellhandles_umgehen_die_sandbox_nicht() {
        let fixture = Fixture::new();
        let foreign = tempfile::tempdir().unwrap();
        let root = foreign.path().canonicalize().unwrap();
        let path = root.join("foreign.txt");
        std::fs::write(&path, b"foreign").unwrap();
        let parent = RootHandle::open_nofollow(&root).unwrap();
        let expected = super::super::super::Identity::from_stat(
            &statat(
                parent.fd(),
                OsStr::new("foreign.txt"),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .unwrap(),
        );
        assert!(ClaimRun::prepare_selected(
            fixture.layout.clone(),
            std::slice::from_ref(&path),
            Some(&[(parent, expected)])
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"foreign");
        assert_eq!(
            std::fs::read_dir(fixture.root.join("journal"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn oeffentlicher_nachfolger_ersetzt_nicht_die_private_transferquelle() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        run.claim(0).unwrap();
        let foreign = fixture.root.join("source/entry-0");
        std::fs::write(&foreign, b"foreign").unwrap();
        let old_claim = fixture.root.join("source/.doppeldecker-trash-claim-public");
        std::fs::write(&old_claim, b"foreign-old").unwrap();
        assert!(matches!(
            run.transfer(0, None).unwrap(),
            TransferOutcome::Durable
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
            b"original"
        );
        assert_eq!(std::fs::read(foreign).unwrap(), b"foreign");
        assert_eq!(std::fs::read(old_claim).unwrap(), b"foreign-old");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "Nur isoliert als root für den echten UID-Wechsel ausführen"]
    fn fremde_uid_mit_gehaltenen_ordnerhandles_erreicht_den_privaten_claim_nicht() {
        use std::os::fd::AsRawFd;
        assert_eq!(unsafe { libc::geteuid() }, 0, "Isolierter Root-Test fehlt");
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        run.claim(0).unwrap();
        let source_fd = run.source(0).unwrap().fd().as_raw_fd();
        let area_fd = run.area.as_ref().unwrap().fd().as_raw_fd();
        // Nach fork ausschließlich async-signal-sichere Systemaufrufe verwenden.
        // Selbst ein bereits gehaltener privater Ordner-FD umgeht nach dem
        // UID-Wechsel seine Suchrechte nicht. Der öffentliche Quell-FD bleibt
        // dagegen absichtlich schreibbar.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setgid(65534) != 0
                    || libc::setuid(65534) != 0
                {
                    libc::_exit(20);
                }
                let fd = libc::openat(area_fd, c"entry-0".as_ptr(), libc::O_RDONLY);
                if fd != -1 || *libc::__errno_location() != libc::EACCES {
                    libc::_exit(21);
                }
                for name in [c"file.txt", c"entry-0", c".doppeldecker-trash-claim-public"] {
                    let fd = libc::openat(
                        source_fd,
                        name.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
                        0o600,
                    );
                    if fd < 0 || libc::write(fd, b"foreign".as_ptr().cast(), 7) != 7 {
                        libc::_exit(22);
                    }
                    libc::close(fd);
                }
                libc::_exit(0);
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        run.check_area().unwrap();
        assert!(matches!(
            run.transfer(0, None).unwrap(),
            TransferOutcome::Durable
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("files/file.txt")).unwrap(),
            b"original"
        );
        for name in ["file.txt", "entry-0", ".doppeldecker-trash-claim-public"] {
            assert_eq!(
                std::fs::read(fixture.root.join("source").join(name)).unwrap(),
                b"foreign"
            );
        }
    }

    #[test]
    fn quelltausch_vor_claim_setzt_nur_das_beobachtete_objekt_zurueck() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        let original = fixture.source.with_extension("original");
        std::fs::rename(&fixture.source, &original).unwrap();
        std::fs::write(&fixture.source, b"replacement").unwrap();
        assert_eq!(run.claim(0).unwrap(), ClaimOutcome::ChangedAndRestored);
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"replacement");
        assert_eq!(std::fs::read(original).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(fixture.root.join("files"))
                .unwrap()
                .count(),
            0
        );
        let stored = fixture.stored(&run);
        assert_ne!(
            stored.entries[0].claimed.as_ref().unwrap(),
            &stored.entries[0].expected
        );
        assert_eq!(stored.entries[0].phase, EntryPhase::Restored);
    }

    #[test]
    fn spaete_restore_kollision_erhaelt_originalnamen_und_privaten_claim() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        run.claim(0).unwrap();
        std::fs::write(&fixture.source, b"new").unwrap();
        assert_eq!(
            run.restore(0).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"new");
        assert_eq!(
            std::fs::read(run.area.as_ref().unwrap().path().join("entry-0")).unwrap(),
            b"original"
        );
        assert_eq!(
            fixture.stored(&run).entries[0].phase,
            EntryPhase::RestoreIntent
        );
    }

    #[test]
    fn spaete_private_claim_kollision_ersetzt_nichts() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        let private = run.area.as_ref().unwrap().path().join("entry-0");
        std::fs::write(&private, b"foreign").unwrap();
        assert_eq!(
            run.claim(0).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(private).unwrap(), b"foreign");
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert_eq!(
            fixture.stored(&run).entries[0].phase,
            EntryPhase::ClaimIntent
        );
    }

    #[test]
    fn markerdrift_und_fremde_bereichsidentitaet_sperren_vor_quellmutation() {
        for replace in [false, true] {
            let fixture = Fixture::new();
            let mut run = fixture.prepare();
            let area = run.area.as_ref().unwrap().path();
            if replace {
                std::fs::rename(area, area.with_extension("held")).unwrap();
                std::fs::create_dir(area).unwrap();
            } else {
                std::fs::write(area.join(".doppeldecker-trash-marker"), b"foreign").unwrap();
            }
            assert!(run.claim(0).is_err());
            assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
            assert_eq!(fixture.stored(&run).entries[0].phase, EntryPhase::Planned);
        }
    }

    #[test]
    fn umbenannter_anker_und_journalpfad_bleiben_am_gehaltenen_objekt() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        for name in ["anchor", "journal", "source"] {
            let path = fixture.root.join(name);
            std::fs::rename(&path, path.with_extension("held")).unwrap();
            symlink(fixture.root.join("files"), path).unwrap();
        }
        assert_eq!(run.claim(0).unwrap(), ClaimOutcome::Claimed);
        let value = decode(
            &std::fs::read(fixture.root.join("journal.held").join(&run.journal_name)).unwrap(),
        )
        .unwrap();
        assert_eq!(value.entries[0].phase, EntryPhase::Claimed);
        assert_eq!(
            std::fs::read_dir(fixture.root.join("files"))
                .unwrap()
                .count(),
            0
        );
        run.restore(0).unwrap();
        assert_eq!(
            std::fs::read(fixture.root.join("source.held/file.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn unsicherer_anker_oder_journalordner_wird_vor_anlage_abgelehnt() {
        for name in ["anchor", "journal"] {
            let fixture = Fixture::new();
            std::fs::set_permissions(
                fixture.root.join(name),
                std::fs::Permissions::from_mode(0o777),
            )
            .unwrap();
            assert!(ClaimRun::prepare(
                fixture.layout.clone(),
                std::slice::from_ref(&fixture.source)
            )
            .is_err());
            assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
            assert_eq!(
                std::fs::read_dir(fixture.root.join("anchor"))
                    .unwrap()
                    .count(),
                0
            );
            assert_eq!(
                std::fs::read_dir(fixture.root.join("journal"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn journalzieltausch_oder_rechteaenderung_ersetzt_keinen_fremden_stand() {
        for permissions in [false, true] {
            let fixture = Fixture::new();
            let mut run = fixture.prepare();
            let path = fixture.root.join("journal").join(&run.journal_name);
            if permissions {
                std::fs::set_permissions(
                    fixture.root.join("anchor"),
                    std::fs::Permissions::from_mode(0o777),
                )
                .unwrap();
            } else {
                std::fs::rename(&path, path.with_extension("held")).unwrap();
                std::fs::write(&path, b"foreign").unwrap();
            }
            assert!(run.claim(0).is_err());
            assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
            if !permissions {
                assert_eq!(std::fs::read(path).unwrap(), b"foreign");
            }
        }
    }

    #[test]
    fn fehler_vor_bereichsabschluss_erhaelt_anlageintent_und_unveraenderte_quelle() {
        for point in ["before_mkdir", "after_mkdir", "marker_sync"] {
            let fixture = Fixture::new();
            let _hook = hook(move |name, _| {
                if name == point {
                    Err(io::Error::other("injected"))
                } else {
                    Ok(())
                }
            });
            let error = ClaimRun::prepare(
                fixture.layout.clone(),
                std::slice::from_ref(&fixture.source),
            )
            .err()
            .unwrap();
            assert_eq!(crate::error::recovery_paths(&error).len(), 2);
            assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
            let names: Vec<_> = std::fs::read_dir(fixture.root.join("journal"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(names.len(), 1);
            let journal = decode(&std::fs::read(&names[0]).unwrap()).unwrap();
            assert_eq!(journal.phase, BatchPhase::Planned);
            assert!(journal.area_intent.is_some());
            assert!(journal.area.is_none());
        }
    }

    #[test]
    fn fehlgeschriebener_claimintent_verhindert_rename_und_sperrt_weiterarbeit() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        let _hook = hook(|name, _| {
            if name == "journal_sync" {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            crate::error_code(&run.claim(0).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert!(run.poisoned);
        assert!(run.claim(0).is_err());
        assert_eq!(fixture.stored(&run).entries[0].phase, EntryPhase::Planned);
    }

    #[test]
    fn spaeter_claim_fsyncfehler_erhaelt_intent_und_privates_objekt() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        let _hook = hook(|name, _| {
            if name == "claim_sync" {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            crate::error_code(&run.claim(0).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        assert!(!fixture.source.exists());
        assert_eq!(
            std::fs::read(run.area.as_ref().unwrap().path().join("entry-0")).unwrap(),
            b"original"
        );
        assert_eq!(
            fixture.stored(&run).entries[0].phase,
            EntryPhase::ClaimIntent
        );
        assert!(run.restore(0).is_err());
    }

    #[test]
    fn journal_eltern_fsyncfehler_autorisiert_keinen_claim() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        let _hook = hook(|name, _| {
            if name == "journal_parent_sync" {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            crate::error_code(&run.claim(0).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert!(run.poisoned);
        assert_eq!(
            fixture.stored(&run).entries[0].phase,
            EntryPhase::ClaimIntent
        );
    }

    #[test]
    fn spaeter_restore_fsyncfehler_erhaelt_original_und_restoreintent() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        run.claim(0).unwrap();
        let _hook = hook(|point, _| {
            if point == "restore_sync" {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            crate::error_code(&run.restore(0).unwrap_err()),
            crate::ErrorCode::OutcomeUnclear
        );
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert!(
            identity(run.area.as_ref().unwrap().fd(), OsStr::new("entry-0"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fixture.stored(&run).entries[0].phase,
            EntryPhase::RestoreIntent
        );
        assert!(run.poisoned);
        assert!(run.restore(0).is_err());
    }

    #[test]
    fn symlink_ordner_hardlink_und_langer_name_bleiben_rename_objekte() {
        let fixture = Fixture::new();
        std::fs::remove_file(&fixture.source).unwrap();
        let link = fixture.root.join("source/link");
        symlink("unberuehrtes-ziel", &link).unwrap();
        let folder = fixture.root.join("source/folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("inside"), b"inside").unwrap();
        let long = fixture.root.join("source").join("a".repeat(255));
        std::fs::write(&long, b"hardlinked").unwrap();
        let partner = fixture.root.join("partner");
        std::fs::hard_link(&long, &partner).unwrap();
        let paths = vec![link.clone(), folder.clone(), long.clone()];
        let before: Vec<_> = paths
            .iter()
            .map(|path| std::fs::symlink_metadata(path).unwrap().ino())
            .collect();
        let mut run = ClaimRun::prepare(fixture.layout.clone(), &paths).unwrap();
        for index in 0..3 {
            run.claim(index).unwrap();
            run.restore(index).unwrap();
        }
        assert_eq!(
            std::fs::read_link(link).unwrap(),
            PathBuf::from("unberuehrtes-ziel")
        );
        assert_eq!(std::fs::read(folder.join("inside")).unwrap(), b"inside");
        for (index, path) in paths.iter().enumerate() {
            assert_eq!(
                std::fs::symlink_metadata(path).unwrap().ino(),
                before[index]
            );
        }
        assert_eq!(std::fs::metadata(long).unwrap().nlink(), 2);
        assert_eq!(std::fs::read(partner).unwrap(), b"hardlinked");
    }

    #[test]
    fn sandbox_und_hilfswurzeln_im_ausgewaehlten_baum_werden_abgelehnt() {
        let fixture = Fixture::new();
        let mut layout = fixture.layout.clone();
        layout.sandbox = Some(RootHandle::open_nofollow(&fixture.root.join("source")).unwrap());
        assert!(ClaimRun::prepare(layout, std::slice::from_ref(&fixture.source)).is_err());
        assert!(ClaimRun::prepare(fixture.layout.clone(), &[fixture.root.join("anchor")]).is_err());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(fixture.root.join("journal"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allow_vererbung_sperrt_anlage_deny_anker_bleibt_unveraendert() {
        for allow in [true, false] {
            let fixture = Fixture::new();
            let path = fixture.root.join("anchor");
            let ace = if allow {
                "everyone allow read,write,execute,file_inherit,directory_inherit"
            } else {
                "everyone deny delete"
            };
            assert!(std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg(ace)
                .arg(&path)
                .status()
                .unwrap()
                .success());
            let before = crate::acl::read_fd(fixture.layout.anchor.fd()).unwrap();
            assert!(before.is_some());
            let result = ClaimRun::prepare(
                fixture.layout.clone(),
                std::slice::from_ref(&fixture.source),
            );
            if allow {
                assert!(result.is_err());
                assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
            } else {
                let mut run = result.unwrap();
                run.claim(0).unwrap();
                run.restore(0).unwrap();
            }
            assert_eq!(
                crate::acl::read_fd(fixture.layout.anchor.fd()).unwrap(),
                before
            );
            assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_default_acl_ist_bereits_vor_normalisierung_durch_maske_geschlossen() {
        use crate::metadata::{PosixAcl, PosixAclEntry};
        let fixture = Fixture::new();
        let entries = vec![
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
            fixture.layout.anchor.fd(),
            Some(&Acl::Posix(PosixAcl {
                access: vec![],
                default: entries,
            })),
        )
        .unwrap();
        let before = crate::acl::read_fd(fixture.layout.anchor.fd()).unwrap();
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = observed.clone();
        let _hook = hook(move |point, path| {
            if point == "after_mkdir" {
                let root = RootHandle::open_nofollow(path)?;
                let Some(Acl::Posix(acl)) = crate::acl::read_fd(root.fd())? else {
                    panic!("Default-ACL wurde nicht geerbt")
                };
                assert!(acl
                    .access
                    .iter()
                    .any(|entry| entry.tag == PosixAclTag::User && entry.permissions == 7));
                assert!(acl
                    .access
                    .iter()
                    .any(|entry| entry.tag == PosixAclTag::Mask && entry.permissions == 0));
                assert!(acl_safe(Some(&Acl::Posix(acl)), true));
                seen.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        });
        let mut run = fixture.prepare();
        assert!(observed.load(std::sync::atomic::Ordering::Relaxed));
        run.claim(0).unwrap();
        run.restore(0).unwrap();
        assert_eq!(
            crate::acl::read_fd(fixture.layout.anchor.fd()).unwrap(),
            before
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_nicht_utf8_name_bleibt_bytegenau_im_journal_und_restore() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        let raw = fixture
            .root
            .join("source")
            .join(OsString::from_vec(vec![255, b'%', b'2', b'F']));
        std::fs::rename(&fixture.source, &raw).unwrap();
        let mut run =
            ClaimRun::prepare(fixture.layout.clone(), std::slice::from_ref(&raw)).unwrap();
        assert_eq!(
            fixture.stored(&run).entries[0].source_name.0,
            vec![255, b'%', b'2', b'F']
        );
        run.claim(0).unwrap();
        run.restore(0).unwrap();
        assert_eq!(std::fs::read(raw).unwrap(), b"original");
    }

    #[test]
    fn bereichs_und_markerkollision_uebernehmen_keinen_bestehenden_rest() {
        for marker in [false, true] {
            let fixture = Fixture::new();
            let kept = std::sync::Arc::new(std::sync::Mutex::new(None));
            let copy = kept.clone();
            let _hook = hook(move |point, path| {
                if point
                    == if marker {
                        "after_mkdir"
                    } else {
                        "before_mkdir"
                    }
                {
                    let name = if marker {
                        path.join(".doppeldecker-trash-marker")
                    } else {
                        path.to_path_buf()
                    };
                    if marker {
                        std::fs::write(&name, b"foreign")?;
                    } else {
                        std::fs::create_dir(&name)?;
                        std::fs::write(name.join("foreign"), b"foreign")?;
                    }
                    *copy.lock().unwrap() = Some(name);
                }
                Ok(())
            });
            assert_eq!(
                ClaimRun::prepare(
                    fixture.layout.clone(),
                    std::slice::from_ref(&fixture.source)
                )
                .err()
                .unwrap()
                .kind(),
                io::ErrorKind::AlreadyExists
            );
            let name = kept.lock().unwrap().clone().unwrap();
            assert_eq!(
                std::fs::read(if marker { name } else { name.join("foreign") }).unwrap(),
                b"foreign"
            );
            assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        }
    }

    #[test]
    fn geaenderter_mountbeleg_und_unbekanntes_rechtemodell_sperren_claim() {
        let fixture = Fixture::new();
        let mut run = fixture.prepare();
        match &mut run.journal.trash.files.mount {
            MountEvidence::Macos { mount_flags, .. } | MountEvidence::Linux { mount_flags, .. } => {
                *mount_flags ^= 1
            }
        }
        assert!(run.claim(0).is_err());
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        let mut mount = fixture.layout.anchor.current_mount_identity().unwrap();
        mount.fs_type = "unknown".into();
        assert!(permission_model(&mount).is_err());
    }

    #[test]
    #[ignore = "Benötigt einen ausdrücklich gewählten künstlichen Testmount"]
    fn anderer_gehaltener_mount_wird_vor_journal_und_quellwirkung_abgelehnt() {
        let fixture = Fixture::new();
        let mount = std::env::var_os("DOPPELDECKER_PRIVATE_CLAIM_TEST_MOUNT")
            .expect("Künstlicher Testmount fehlt");
        let external = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(mount)
            .unwrap();
        let path = external.path().canonicalize().unwrap();
        let anchor = RootHandle::open_nofollow(&path).unwrap();
        let evidence = directory_evidence(&anchor, true).unwrap();
        assert_ne!(
            evidence.mount,
            snapshot(&fixture.layout.files).unwrap().mount
        );
        let mut layout = fixture.layout.clone();
        layout.anchor = anchor;
        layout.sandbox = None;
        assert_eq!(
            ClaimRun::prepare(layout, std::slice::from_ref(&fixture.source))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(&fixture.source).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(fixture.root.join("journal"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_datei_acl_bleibt_beim_privaten_claim_und_restore_unveraendert() {
        let fixture = Fixture::new();
        assert!(std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&fixture.source)
            .status()
            .unwrap()
            .success());
        let acl = |path: &Path| {
            crate::acl::read_fd(
                &openat(
                    rustix::fs::CWD,
                    path,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .unwrap(),
            )
            .unwrap()
        };
        let before = acl(&fixture.source);
        assert!(before.is_some());
        let mut run = fixture.prepare();
        run.claim(0).unwrap();
        assert_eq!(
            acl(&run.area.as_ref().unwrap().path().join("entry-0")),
            before
        );
        run.restore(0).unwrap();
        assert_eq!(acl(&fixture.source), before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_datei_acl_bleibt_beim_privaten_claim_und_restore_unveraendert() {
        use crate::metadata::{PosixAcl, PosixAclEntry};
        let fixture = Fixture::new();
        let fd = openat(
            rustix::fs::CWD,
            &fixture.source,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let entries = vec![
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
                access: entries,
                default: vec![],
            })),
        )
        .unwrap();
        let before = crate::acl::read_fd(&fd).unwrap();
        let mut run = fixture.prepare();
        run.claim(0).unwrap();
        assert_eq!(crate::acl::read_fd(&fd).unwrap(), before);
        run.restore(0).unwrap();
        assert_eq!(crate::acl::read_fd(&fd).unwrap(), before);
    }
}
