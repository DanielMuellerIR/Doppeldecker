use super::transfer::verify_info;
use super::*;
use std::collections::BTreeSet;

pub(in crate::trash) fn recover(
    path: &Path,
    data: &[u8],
    report: &mut super::super::super::RecoveryStats,
) -> io::Result<()> {
    let mut run = ClaimRun::reopen(path, data).map_err(|error| {
        crate::error::with_recovery_paths(error, &[path.to_string_lossy().into_owned()])
    })?;
    let result = run.recover_entries(report).and_then(|()| run.finish());
    result.map_err(|error| run.unclear(error))
}

impl ClaimRun {
    fn reopen(path: &Path, previous: &[u8]) -> io::Result<Self> {
        let parent = RootHandle::open_nofollow(
            path.parent()
                .ok_or_else(|| denied("Journal-Eltern fehlen"))?,
        )?;
        let journal_parent = directory_evidence(&parent, false)?;
        let name = path
            .file_name()
            .ok_or_else(|| denied("Journalname fehlt"))?;
        let fd = openat(
            parent.fd(),
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fstat(&fd)?;
        if from_stat(&stat).file_type != 0o100000
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_mode & 0o077 != 0
            || !acl_safe(crate::acl::read_fd(&fd)?.as_ref(), true)
        {
            return Err(denied("Journaldatei ist nicht belegbar privat"));
        }
        let journal_identity = from_stat(&stat);
        let mut data = Vec::new();
        File::from(fd)
            .take(super::super::super::MAX_JOURNAL_BYTES + 1)
            .read_to_end(&mut data)?;
        if data != previous || identity(parent.fd(), name)?.as_ref() != Some(&journal_identity) {
            return Err(denied("Journal änderte sich während der erneuten Bindung"));
        }
        let journal = decode(&data)?;
        if journal_parent != journal.journal_parent
            || name != OsStr::new(&format!("trash-{}.json", journal.run_id))
            || journal.effective_uid != unsafe { libc::geteuid() }
        {
            return Err(denied("Journalpfad, Namensraum oder UID passen nicht"));
        }
        let intent = journal
            .area_intent
            .as_ref()
            .ok_or_else(|| denied("Privater Anlageintent fehlt"))?;
        let sandbox = journal
            .sandbox
            .as_ref()
            .map(|value| open_recorded(value, None))
            .transpose()?;
        let anchor = open_recorded(&intent.anchor, sandbox.as_ref())?;
        let files = open_recorded(&journal.trash.files, sandbox.as_ref())?;
        let info = journal
            .trash
            .info
            .as_ref()
            .map(|value| open_recorded(value, sandbox.as_ref()))
            .transpose()?;
        if let Some(root) = &sandbox {
            let checked = RootHandle::open_beneath(root, parent.path())?;
            if snapshot(&checked)? != journal_parent {
                return Err(denied(
                    "Recoveryjournal liegt außerhalb der ursprünglichen Sandbox",
                ));
            }
        }
        let area = match (&journal.area, journal.phase) {
            (Some(area), BatchPhase::Settled)
                if identity(anchor.fd(), os_name(&area.name))?.is_none() =>
            {
                None
            }
            (Some(area), _) => Some(RootHandle::open_beneath(
                &anchor,
                &anchor.path().join(os_name(&area.name)),
            )?),
            (None, _) => {
                return Err(denied(
                    "Nicht abgeschlossene Bereichsanlage bleibt manuell erhalten",
                ))
            }
        };
        // Die Position bleibt der Journalindex. Terminale Einträge und rein
        // manuelle Transferphasen brauchen keinen Quellhandle; ein später
        // entfernter Quellordner darf unabhängige offene Claims nicht blockieren.
        let sources = journal
            .entries
            .iter()
            .map(|entry| match entry.phase {
                EntryPhase::Planned
                | EntryPhase::ClaimIntent
                | EntryPhase::Claimed
                | EntryPhase::Drifted
                | EntryPhase::RestoreIntent
                | EntryPhase::InfoPublishIntent
                | EntryPhase::InfoReady => {
                    open_recorded(&entry.source_parent, sandbox.as_ref()).map(Some)
                }
                EntryPhase::Trashed
                | EntryPhase::Restored
                | EntryPhase::Untouched
                | EntryPhase::TransferIntent
                | EntryPhase::Unclear => Ok(None),
            })
            .collect::<io::Result<Vec<_>>>()?;
        let run = Self {
            layout: HeldLayout {
                anchor,
                files,
                info,
                journal: parent,
                sandbox,
            },
            sources,
            area,
            journal,
            journal_name: name.to_os_string(),
            journal_identity: Some(journal_identity),
            poisoned: false,
        };
        run.check_layout()?;
        if run.journal.phase != BatchPhase::Settled {
            run.check_area()?;
        }
        Ok(run)
    }

    fn recover_entries(
        &mut self,
        report: &mut super::super::super::RecoveryStats,
    ) -> io::Result<()> {
        for index in 0..self.journal.entries.len() {
            let entry = self.journal.entries[index].clone();
            match entry.phase {
                EntryPhase::Trashed => report.completed += 1,
                EntryPhase::Restored => report.restored += 1,
                EntryPhase::Untouched => {}
                EntryPhase::Planned => {
                    self.check_area()?;
                    self.check_source(index)?;
                    if identity(
                        self.area.as_ref().expect("area_ready").fd(),
                        os_name(&entry.private_name),
                    )?
                    .is_some()
                    {
                        return Err(denied("Privater Eintrag ohne Claim-Intent bleibt manuell"));
                    }
                    if identity(self.source(index)?.fd(), os_name(&entry.source_name))?.as_ref()
                        != Some(&entry.expected)
                    {
                        report.drifted += 1;
                    }
                    self.set_phase(index, EntryPhase::Untouched)?;
                }
                EntryPhase::ClaimIntent => {
                    self.check_area()?;
                    self.check_source(index)?;
                    let area = self.area.as_ref().expect("area_ready");
                    let claim = identity(area.fd(), os_name(&entry.private_name))?;
                    let original = identity(self.source(index)?.fd(), os_name(&entry.source_name))?;
                    if claim.is_none() && original.as_ref() == Some(&entry.expected) {
                        self.set_phase(index, EntryPhase::Untouched)?;
                    } else if claim.as_ref() == Some(&entry.expected) && original.is_none() {
                        fsync(area.fd())?;
                        fsync(self.source(index)?.fd())?;
                        let mut next = self.journal.clone();
                        next.entries[index].claimed = claim;
                        next.entries[index].phase = EntryPhase::Claimed;
                        self.persist(&next)?;
                        self.journal = next;
                        self.restore(index)?;
                        report.restored += 1;
                    } else {
                        return Err(denied(
                            "Claim-Intent hat keinen eindeutigen passenden Datenort",
                        ));
                    }
                }
                EntryPhase::RestoreIntent => {
                    self.check_area()?;
                    self.check_source(index)?;
                    let area = self.area.as_ref().expect("area_ready");
                    let claim = identity(area.fd(), os_name(&entry.private_name))?;
                    let original = identity(self.source(index)?.fd(), os_name(&entry.source_name))?;
                    if claim.is_none() && original.as_ref() == entry.claimed.as_ref() {
                        fsync(area.fd())?;
                        fsync(self.source(index)?.fd())?;
                        self.set_phase(index, EntryPhase::Restored)?;
                    } else if claim.as_ref() == entry.claimed.as_ref() && original.is_none() {
                        self.restore(index)?;
                    } else {
                        return Err(denied(
                            "Restore-Intent hat keinen eindeutigen belegten Datenort",
                        ));
                    }
                    report.restored += 1;
                }
                EntryPhase::Claimed
                | EntryPhase::Drifted
                | EntryPhase::InfoPublishIntent
                | EntryPhase::InfoReady => {
                    self.restore(index)?;
                    report.restored += 1;
                }
                // Ein gerade passender Zielname ersetzt nie den fehlenden
                // dauerhaften Transferabschluss. Diesen Zweig nur lesen.
                EntryPhase::TransferIntent | EntryPhase::Unclear => {
                    return Err(denied(
                        "Nicht terminaler Transfer bleibt zur manuellen Prüfung erhalten",
                    ));
                }
            }
        }
        Ok(())
    }

    fn set_phase(&mut self, index: usize, phase: EntryPhase) -> io::Result<()> {
        let mut next = self.journal.clone();
        next.entries[index].phase = phase;
        self.persist(&next)?;
        self.journal = next;
        Ok(())
    }

    pub(in crate::trash) fn finish(&mut self) -> io::Result<()> {
        self.finish_inner().map_err(|error| self.unclear(error))
    }

    fn finish_inner(&mut self) -> io::Result<()> {
        if self.journal.phase == BatchPhase::Settled {
            return self.remove_settled_journal();
        }
        self.check_area()?;
        if !self
            .journal
            .entries
            .iter()
            .all(|entry| is_terminal(entry.phase))
        {
            return Err(denied("Cleanup benötigt terminale Datenaktionen"));
        }
        let removals = self.cleanup_set()?;
        let mut next = self.journal.clone();
        next.phase = BatchPhase::CleanupIntent;
        self.persist(&next)?;
        self.journal = next;
        for (index, private) in removals {
            self.check_area()?;
            // Die gesamte Restmenge nochmals lesen; kein fremder Zusatz wird
            // durch das Entfernen eigener bekannter Namen verdeckt.
            self.cleanup_set()?;
            let info = self.journal.entries[index]
                .info
                .as_ref()
                .expect("Info-Rest");
            let area = self.area.as_ref().expect("area_ready");
            let (root, name) = if private {
                (area, &info.private_name)
            } else {
                (
                    self.layout.info.as_ref().expect("XDG"),
                    &info.published_name,
                )
            };
            verify_info(root, os_name(name), info)?;
            boundary("before_info_cleanup", root.path())?;
            // Hook/Dateiänderung darf keine neue Wirkung auf einen Nachfolger erben.
            self.check_area()?;
            self.cleanup_set()?;
            verify_info(root, os_name(name), info)?;
            unlinkat(root.fd(), os_name(name), AtFlags::empty())?;
            boundary("info_cleanup_sync", root.path())?;
            fsync(root.fd())?;
        }
        self.cleanup_set()?;
        let area = self.area.as_ref().expect("area_ready").clone();
        // Auch ein vor dem Crash bereits entfernter Info-Rest braucht jetzt
        // eine frische Verzeichnissynchronisierung vor dem Markerabschluss.
        fsync(area.fd())?;
        if let Some(info) = &self.layout.info {
            fsync(info.fd())?;
        }
        let expected = self.journal.area.as_ref().expect("area_ready").clone();
        boundary("before_marker_cleanup", area.path())?;
        self.check_area()?;
        if names(&area)? != BTreeSet::from([expected.marker_name.0.clone()]) {
            return Err(denied("Bereich ist vor Markerentfernung nicht leer"));
        }
        unlinkat(area.fd(), os_name(&expected.marker_name), AtFlags::empty())?;
        boundary("after_marker_cleanup", area.path())?;
        fsync(area.fd())?;
        boundary("before_area_cleanup", area.path())?;
        let current = directory_evidence(&area, true)?;
        if current.identity != expected.identity
            || current.mount != expected.anchor.mount
            || current.owner != expected.owner
            || current.mode != expected.mode
            || current.acl != expected.acl
            || identity(self.layout.anchor.fd(), os_name(&expected.name))?.as_ref()
                != Some(&expected.identity)
            || !names(&area)?.is_empty()
        {
            return Err(denied("Bereich änderte sich vor rmdir"));
        }
        self.check_layout()?;
        unlinkat(
            self.layout.anchor.fd(),
            os_name(&expected.name),
            AtFlags::REMOVEDIR,
        )?;
        boundary("area_cleanup_sync", self.layout.anchor.path())?;
        fsync(self.layout.anchor.fd())?;
        let mut next = self.journal.clone();
        next.phase = BatchPhase::Settled;
        self.persist(&next)?;
        self.journal = next;
        self.remove_settled_journal()
    }

    fn cleanup_set(&self) -> io::Result<Vec<(usize, bool)>> {
        self.check_area()?;
        let area = self.area.as_ref().expect("area_ready");
        let expected_area = self.journal.area.as_ref().expect("area_ready");
        let mut expected = BTreeSet::from([expected_area.marker_name.0.clone()]);
        let mut removals = Vec::new();
        for (index, entry) in self.journal.entries.iter().enumerate() {
            if identity(area.fd(), os_name(&entry.private_name))?.is_some() {
                return Err(denied(
                    "Terminaler Eintrag besitzt noch einen privaten Datenclaim",
                ));
            }
            if let Some(info) = &entry.info {
                let private = identity(area.fd(), os_name(&info.private_name))?;
                if entry.phase == EntryPhase::Trashed {
                    if private.is_some() {
                        return Err(denied("Terminaler Transfer besitzt noch private Info"));
                    }
                    continue;
                }
                let target = self.layout.info.as_ref().expect("XDG");
                let published = identity(target.fd(), os_name(&info.published_name))?;
                if identity(self.layout.files.fd(), os_name(&entry.trash_name))?.is_some() {
                    return Err(denied("Sichtbare Daten sperren Info-Cleanup nach Restore"));
                }
                match (private, published) {
                    (Some(actual), None) if actual == info.identity => {
                        verify_info(area, os_name(&info.private_name), info)?;
                        expected.insert(info.private_name.0.clone());
                        removals.push((index, true));
                    }
                    (None, Some(actual)) if actual == info.identity => {
                        verify_info(target, os_name(&info.published_name), info)?;
                        removals.push((index, false));
                    }
                    (None, None) => {}
                    _ => return Err(denied("Fremde oder doppelte Info-Paarung bleibt erhalten")),
                }
            }
        }
        if names(area)? != expected {
            return Err(denied(
                "Zusätzlicher privater Inhalt erhält Bereich und Marker",
            ));
        }
        Ok(removals)
    }

    fn remove_settled_journal(&mut self) -> io::Result<()> {
        self.check_layout()?;
        let area = self
            .journal
            .area
            .as_ref()
            .ok_or_else(|| denied("Bereichsabschluss fehlt"))?;
        if self.journal.phase != BatchPhase::Settled
            || identity(self.layout.anchor.fd(), os_name(&area.name))?.is_some()
            || identity(self.layout.journal.fd(), &self.journal_name)? != self.journal_identity
        {
            return Err(denied(
                "Settled-Journal besitzt keinen eindeutigen Abschluss",
            ));
        }
        boundary("before_journal_cleanup", self.layout.journal.path())?;
        self.check_layout()?;
        if identity(self.layout.journal.fd(), &self.journal_name)? != self.journal_identity {
            return Err(denied("Journalname änderte sich vor Cleanup"));
        }
        if identity(self.layout.anchor.fd(), os_name(&area.name))?.is_some() {
            return Err(denied(
                "Bereichsname wurde vor Journalabschluss erneut belegt",
            ));
        }
        unlinkat(
            self.layout.journal.fd(),
            &self.journal_name,
            AtFlags::empty(),
        )?;
        boundary("journal_cleanup_sync", self.layout.journal.path())?;
        fsync(self.layout.journal.fd())?;
        Ok(())
    }
}

fn is_terminal(phase: EntryPhase) -> bool {
    matches!(
        phase,
        EntryPhase::Trashed | EntryPhase::Restored | EntryPhase::Untouched
    )
}

fn open_recorded(
    value: &DirectoryEvidence,
    sandbox: Option<&RootHandle>,
) -> io::Result<RootHandle> {
    let path = Path::new(OsStr::from_bytes(&value.path.0));
    let root = match sandbox {
        Some(root) => RootHandle::open_beneath(root, path)?,
        None => RootHandle::open_nofollow(path)?,
    };
    if snapshot(&root)? != *value {
        return Err(denied("Verzeichnis-/Mount-/Rechtebeleg passt nicht mehr"));
    }
    Ok(root)
}

fn names(root: &RootHandle) -> io::Result<BTreeSet<Vec<u8>>> {
    let mut names = BTreeSet::new();
    for entry in rustix::fs::Dir::read_from(root.fd())? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.insert(name);
        }
    }
    Ok(names)
}

pub(in crate::trash) fn restore_legacy_claim(
    original: &Path,
    claim_name: &str,
    expected: super::super::super::Identity,
) -> io::Result<()> {
    super::super::super::validate_absolute_path(original)?;
    let parent = original
        .parent()
        .ok_or_else(|| denied("Legacy-Quell-Eltern fehlen"))?;
    let name = original
        .file_name()
        .ok_or_else(|| denied("Legacy-Quellname fehlt"))?;
    if !byte_name(name).valid() || !ByteName(claim_name.as_bytes().to_vec()).valid() {
        return Err(invalid("Legacy-Name ist ungültig"));
    }
    let root = RootHandle::open_nofollow(parent)?;
    let evidence = directory_evidence(&root, true)?;
    let expected = ObjectIdentity {
        device: expected.device,
        inode: expected.inode,
        file_type: expected.file_type,
    };
    if !expected.valid() {
        return Err(invalid("Legacy-Claimidentität ist ungültig"));
    }
    check_entry_mount(&root, OsStr::new(claim_name), &expected)?;
    if identity(root.fd(), OsStr::new(claim_name))?.as_ref() != Some(&expected)
        || identity(root.fd(), name)?.is_some()
        || directory_evidence(&root, true)? != evidence
    {
        return Err(denied(
            "Legacy-Claim besitzt keinen privaten eindeutigen Rückweg",
        ));
    }
    boundary("before_legacy_restore", root.path())?;
    if directory_evidence(&root, true)? != evidence
        || identity(root.fd(), OsStr::new(claim_name))?.as_ref() != Some(&expected)
    {
        return Err(denied("Privater Legacy-Claim änderte sich"));
    }
    renameat_with(
        root.fd(),
        OsStr::new(claim_name),
        root.fd(),
        name,
        RenameFlags::NOREPLACE,
    )?;
    if identity(root.fd(), name)?.as_ref() != Some(&expected)
        || identity(root.fd(), OsStr::new(claim_name))?.is_some()
    {
        return Err(denied("Legacy-Restore hat keinen passenden Abschluss"));
    }
    fsync(root.fd())?;
    Ok(())
}

pub(in crate::trash) struct LegacyJournal {
    root: RootHandle,
    evidence: DirectoryEvidence,
    name: OsString,
    identity: ObjectIdentity,
}

impl LegacyJournal {
    pub(in crate::trash) fn bind(path: &Path, previous: &[u8]) -> io::Result<Self> {
        let root = RootHandle::open_nofollow(
            path.parent()
                .ok_or_else(|| denied("Legacy-Journal-Eltern fehlen"))?,
        )?;
        let evidence = directory_evidence(&root, false)?;
        let name = path
            .file_name()
            .ok_or_else(|| denied("Legacy-Journalname fehlt"))?
            .to_os_string();
        let fd = openat(
            root.fd(),
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fstat(&fd)?;
        let object = from_stat(&stat);
        if object.file_type != 0o100000
            || stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_mode & 0o077 != 0
            || !acl_safe(crate::acl::read_fd(&fd)?.as_ref(), true)
        {
            return Err(denied("Legacy-Journaldatei ist nicht privat"));
        }
        let mut data = Vec::new();
        File::from(fd)
            .take(super::super::super::MAX_JOURNAL_BYTES + 1)
            .read_to_end(&mut data)?;
        if data != previous || identity(root.fd(), &name)?.as_ref() != Some(&object) {
            return Err(denied("Legacy-Journal änderte sich während der Bindung"));
        }
        Ok(Self {
            root,
            evidence,
            name,
            identity: object,
        })
    }

    fn check(&self) -> io::Result<()> {
        if directory_evidence(&self.root, false)? != self.evidence
            || identity(self.root.fd(), &self.name)?.as_ref() != Some(&self.identity)
        {
            return Err(denied("Legacy-Journalbindung änderte sich"));
        }
        Ok(())
    }

    pub(in crate::trash) fn save(
        &mut self,
        journal: &super::super::super::TrashJournal,
    ) -> io::Result<()> {
        self.check()?;
        let payload = serde_json::to_vec_pretty(journal).map_err(io::Error::other)?;
        if payload.len() as u64 > super::super::super::MAX_JOURNAL_BYTES {
            return Err(invalid("Legacy-Journal ist zu groß"));
        }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let random: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let name: OsString = format!(".doppeldecker-trash-legacy-{random}.tmp").into();
        let fd = create_private_file(self.root.fd(), &name)?;
        let created = from_stat(&fstat(&fd)?);
        let result = (|| {
            normalize_private_file(&fd)?;
            let mut file = File::from(fd);
            file.write_all(&payload)?;
            file.sync_all()?;
            boundary("before_legacy_journal_commit", self.root.path())?;
            self.check()?;
            if identity(self.root.fd(), &name)?.as_ref() != Some(&created) {
                return Err(denied("Legacy-Journaltemp wurde ausgetauscht"));
            }
            renameat(self.root.fd(), &name, self.root.fd(), &self.name)?;
            self.identity = created.clone();
            fsync(self.root.fd())?;
            Ok(())
        })();
        if let Err(error) = result {
            if self.check().is_ok()
                && identity(self.root.fd(), &name)
                    .is_ok_and(|value| value.as_ref() == Some(&created))
            {
                let _ = unlinkat(self.root.fd(), &name, AtFlags::empty());
                let _ = fsync(self.root.fd());
            }
            return Err(crate::error::with_recovery_paths(
                error,
                &[
                    self.root
                        .path()
                        .join(&self.name)
                        .to_string_lossy()
                        .into_owned(),
                    self.root.path().join(&name).to_string_lossy().into_owned(),
                ],
            ));
        }
        Ok(())
    }

    pub(in crate::trash) fn remove(&self) -> io::Result<()> {
        self.check()?;
        boundary("before_legacy_journal_cleanup", self.root.path())?;
        self.check()?;
        unlinkat(self.root.fd(), &self.name, AtFlags::empty())?;
        fsync(self.root.fd())?;
        Ok(())
    }
}
