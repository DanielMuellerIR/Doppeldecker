use super::*;

impl ClaimRun {
    pub(in crate::trash) fn transfer(
        &mut self,
        index: usize,
        origin_mount: Option<&RootHandle>,
    ) -> io::Result<TransferOutcome> {
        self.check_area()?;
        let entry = self
            .journal
            .entries
            .get(index)
            .ok_or_else(|| denied("Unbekannter Transfer-Eintrag"))?
            .clone();
        if entry.phase != EntryPhase::Claimed || entry.claimed.as_ref() != Some(&entry.expected) {
            return Err(denied("Transfer benötigt den passenden privaten Claim"));
        }
        self.check_claim(index)?;
        if self.layout.info.is_some() {
            if let Err(error) = self.publish_info(index, origin_mount) {
                return Err(self.unclear(error));
            }
        }
        self.check_area()?;
        self.check_claim(index)?;
        self.check_published_info(index)?;
        let mut next = self.journal.clone();
        next.entries[index].phase = EntryPhase::TransferIntent;
        self.persist(&next)?;
        self.journal = next;
        let area = self.area.as_ref().expect("area_ready").clone();
        let entry = self.journal.entries[index].clone();
        let transfer = (|| {
            boundary("before_transfer", area.path())?;
            self.check_area()?;
            self.check_claim(index)?;
            self.check_published_info(index)?;
            renameat_with(
                area.fd(),
                os_name(&entry.private_name),
                self.layout.files.fd(),
                os_name(&entry.trash_name),
                RenameFlags::NOREPLACE,
            )?;
            boundary("after_transfer", self.layout.files.path())?;
            if identity(self.layout.files.fd(), os_name(&entry.trash_name))?.as_ref()
                != entry.claimed.as_ref()
                || identity(area.fd(), os_name(&entry.private_name))?.is_some()
            {
                return Err(denied(
                    "Trash-Ziel stimmt nicht mit dem privaten Claim überein",
                ));
            }
            self.check_published_info(index)?;
            Ok(())
        })();
        if let Err(error) = transfer {
            return Err(self.unclear(error));
        }
        // Erst der positive Readback erlaubt die sichtbare Erfolgsbilanz. Ein
        // späterer fsync-/Journalfehler bleibt davon getrennt und sperrt Cleanup.
        let durable = (|| {
            boundary("transfer_sync", self.layout.files.path())?;
            fsync(area.fd())?;
            fsync(self.layout.files.fd())?;
            let mut next = self.journal.clone();
            next.entries[index].phase = EntryPhase::Trashed;
            self.persist(&next)?;
            self.journal = next;
            Ok(())
        })();
        match durable {
            Ok(()) => Ok(TransferOutcome::Durable),
            Err(error) => Ok(TransferOutcome::Observed(self.unclear(error))),
        }
    }

    fn check_claim(&self, index: usize) -> io::Result<()> {
        let entry = &self.journal.entries[index];
        if identity(
            self.area.as_ref().expect("area_ready").fd(),
            os_name(&entry.private_name),
        )?
        .as_ref()
            != entry.claimed.as_ref()
            || entry.claimed.as_ref() != Some(&entry.expected)
        {
            return Err(denied("Privater Transferclaim änderte seine Identität"));
        }
        Ok(())
    }

    fn publish_info(&mut self, index: usize, origin_mount: Option<&RootHandle>) -> io::Result<()> {
        let entry = self.journal.entries[index].clone();
        let original = PathBuf::from(OsStr::from_bytes(&entry.source_parent.path.0))
            .join(os_name(&entry.source_name));
        let origin = match origin_mount {
            None => original,
            Some(root) => {
                if let Some(parent) = root.path().parent() {
                    if RootHandle::open_nofollow(parent)?.current_mount_identity()?
                        == root.current_mount_identity()?
                    {
                        return Err(denied(
                            "Relativer Info-Ursprung benötigt die tatsächliche Mountwurzel",
                        ));
                    }
                }
                if mount_evidence(&root.current_mount_identity()?)?
                    != self.journal.trash.files.mount
                {
                    return Err(denied("Info-Ursprung liegt auf anderem Mount"));
                }
                let checked = RootHandle::open_beneath(root, self.source(index)?.path())?;
                if snapshot(&checked)? != entry.source_parent {
                    return Err(denied(
                        "Relativer Info-Ursprung ist nicht an die Quelle gebunden",
                    ));
                }
                let relative = original
                    .strip_prefix(root.path())
                    .map_err(|_| denied("Info-Ursprung liegt außerhalb der Mountwurzel"))?;
                if relative
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                {
                    return Err(denied("Relativer Info-Ursprung ist ungültig"));
                }
                relative.to_path_buf()
            }
        };
        let content = format!(
            "[Trash Info]\nPath={}\nDeletionDate={}\n",
            super::super::super::percent_encode_path(&origin),
            super::super::super::local_deletion_time()
        )
        .into_bytes();
        if content.len() > 16 * 1024 {
            return Err(invalid("Info-Datei überschreitet die Größenbegrenzung"));
        }
        let area = self.area.as_ref().expect("area_ready").clone();
        let name: OsString = format!("info-{index}").into();
        let fd = create_private_file(area.fd(), &name)?;
        let object = from_stat(&fstat(&fd)?);
        normalize_private_file(&fd)?;
        let mut file = File::from(fd);
        file.write_all(&content)?;
        boundary("info_file_sync", area.path())?;
        file.sync_all()?;
        fsync(area.fd())?;
        self.check_area()?;
        let info = InfoEvidence {
            private_name: byte_name(&name),
            published_name: byte_name(&super::super::super::info_name(os_name(&entry.trash_name))),
            identity: object,
            bytes: content.len() as u64,
            digest: *blake3::hash(&content).as_bytes(),
        };
        verify_info(&area, os_name(&info.private_name), &info)?;
        let mut next = self.journal.clone();
        next.entries[index].info = Some(info.clone());
        next.entries[index].phase = EntryPhase::InfoPublishIntent;
        self.persist(&next)?;
        self.journal = next;
        boundary("before_info_publish", area.path())?;
        self.check_area()?;
        verify_info(&area, os_name(&info.private_name), &info)?;
        let target = self.layout.info.as_ref().expect("XDG");
        renameat_with(
            area.fd(),
            os_name(&info.private_name),
            target.fd(),
            os_name(&info.published_name),
            RenameFlags::NOREPLACE,
        )?;
        boundary("after_info_publish", target.path())?;
        self.check_published_info(index)?;
        boundary("info_publish_sync", target.path())?;
        fsync(area.fd())?;
        fsync(target.fd())?;
        let mut next = self.journal.clone();
        next.entries[index].phase = EntryPhase::InfoReady;
        self.persist(&next)?;
        self.journal = next;
        Ok(())
    }

    fn check_published_info(&self, index: usize) -> io::Result<()> {
        match (&self.layout.info, &self.journal.entries[index].info) {
            (None, None) => Ok(()),
            (Some(target), Some(info)) => {
                verify_info(target, os_name(&info.published_name), info)?;
                if identity(
                    self.area.as_ref().expect("area_ready").fd(),
                    os_name(&info.private_name),
                )?
                .is_some()
                {
                    return Err(denied(
                        "Info liegt gleichzeitig privat und veröffentlicht vor",
                    ));
                }
                Ok(())
            }
            _ => Err(denied("Info-Paarung fehlt")),
        }
    }
}

pub(super) fn verify_info(
    root: &RootHandle,
    name: &OsStr,
    expected: &InfoEvidence,
) -> io::Result<()> {
    let fd = openat(
        root.fd(),
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    if from_stat(&fstat(&fd)?) != expected.identity {
        return Err(denied("Info-Datei änderte ihre Identität"));
    }
    let mut content = Vec::new();
    File::from(fd)
        .take(16 * 1024 + 1)
        .read_to_end(&mut content)?;
    if content.len() as u64 != expected.bytes
        || blake3::hash(&content).as_bytes() != &expected.digest
    {
        return Err(denied("Info-Datei änderte ihren Inhalt"));
    }
    Ok(())
}
