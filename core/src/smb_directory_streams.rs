//! Gewöhnlicher Verzeichnisstream-Sync: Nur der lebende exklusive Creator ist
//! Tempbesitz. Die Basis-ID identifiziert ausschließlich den Ordner, nie ein ADS.
use super::*;
use crate::directory_streams::{
    DirectoryStreamEffect, DirectoryStreamOutcome, DirectoryStreamReport, DirectoryStreamTemp,
    DirectoryStreamTransaction,
};
use crate::local_staging::{
    NamedStreamIntentBinding, NamedStreamIntentPhase, RemoteTempIntent, StagingOwner,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

type StreamSet = BTreeMap<Vec<u8>, NamedBlobSummary>;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static TEST_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn test_fault(point: &str) -> io::Result<()> {
    TEST_FAULT.with(|fault| {
        if fault.get() == Some(point) {
            fault.set(None);
            Err(io::Error::other(format!(
                "Testfehler nach bestätigtem SMB-{point}"
            )))
        } else {
            Ok(())
        }
    })
}

fn stream_set(values: &[NamedBlobSummary]) -> io::Result<StreamSet> {
    let mut set = BTreeMap::new();
    let mut lookups = BTreeSet::new();
    for value in values {
        if !lookups.insert(lookup_key(stream_name(&value.name)?))
            || set.insert(value.name.clone(), value.clone()).is_some()
        {
            return Err(invalid(
                "Kollidierende bestätigte SMB-Verzeichnisstreamnamen",
            ));
        }
    }
    Ok(set)
}

struct DirectoryTransaction {
    runtime: Arc<Runtime>,
    tree: Arc<Tree>,
    connection: Connection,
    base: Option<FileId>,
    parents: Vec<FileId>,
    base_identity: u64,
    remote: String,
    directory: PathBuf,
    expected: StreamSet,
    report: DirectoryStreamReport,
    intents: Vec<RemoteTempIntent>,
    recovery_hints: Vec<String>,
    owner: StagingOwner,
    root_identity: crate::real_mode::RootIdentity,
    account: [u8; 32],
    cancel: crate::Cancel,
    check_namespace: Box<dyn Fn() -> io::Result<()> + Send>,
    max_read: u32,
    max_write: u32,
    blocked: bool,
}

impl SmbVfs {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_directory_stream_transaction(
        &self,
        path: &Path,
        root: &Path,
        proof: &crate::sync::SyncTargetProof,
        expected_streams: &[NamedBlobSummary],
        cancel: crate::Cancel,
        owner: StagingOwner,
        root_identity: crate::real_mode::RootIdentity,
        check_namespace: Box<dyn Fn() -> io::Result<()> + Send>,
    ) -> io::Result<Box<dyn DirectoryStreamTransaction>> {
        let expected = stream_set(expected_streams)?;
        let (remote, scope) = stream_binding_scope(&self.canonical_root(), path, Some(root))?;
        check_cancel(Some(&cancel))?;
        check_namespace()?;
        let (tree, mut connection, max_read, max_write, account) = {
            let state = lock(&self.state);
            let params = state.session.connection.params();
            (
                state.session.tree.clone(),
                state.session.connection.clone(),
                params
                    .map_or(64 * 1024, |p| p.max_read_size.min(MAX_IO_CHUNK))
                    .max(1),
                params
                    .map_or(64 * 1024, |p| p.max_write_size.min(MAX_IO_CHUNK))
                    .max(1),
                state.session.identity.account_digest,
            )
        };
        let (base, entry, parents) = self.runtime.block_on(open_bound_base(
            &mut connection,
            &tree,
            &remote,
            StreamBaseKind::Directory,
            true,
            &scope,
        ))?;
        let mut transaction = DirectoryTransaction {
            runtime: self.runtime.clone(),
            tree,
            connection,
            base: Some(base),
            parents,
            base_identity: entry.file_id,
            remote,
            directory: path.to_path_buf(),
            expected,
            report: DirectoryStreamReport::default(),
            intents: Vec::new(),
            recovery_hints: Vec::new(),
            owner,
            root_identity,
            account,
            cancel,
            check_namespace,
            max_read,
            max_write,
            blocked: false,
        };
        if !proof.matches_directory_identity(&entry.entry()) {
            return Err(crate::real_mode::plan_drift(
                "SMB-Verzeichnisstreambasis wurde ausgetauscht",
            ));
        }
        transaction.verify(None, &transaction.expected.clone())?;
        Ok(Box::new(transaction))
    }
}

impl DirectoryTransaction {
    fn check(&self) -> io::Result<()> {
        if self.blocked {
            return Err(invalid(
                "SMB-Verzeichnisstreamtransaktion ist nach einem Fehler gesperrt",
            ));
        }
        check_cancel(Some(&self.cancel))?;
        (self.check_namespace)()
    }

    fn snapshot(&mut self, live: Option<(&[u8], FileId)>) -> io::Result<StreamSet> {
        let base = self
            .base
            .ok_or_else(|| invalid("SMB-Verzeichnisbasis ist geschlossen"))?;
        self.runtime.block_on(async {
            let mut previous = None;
            for _ in 0..2 {
                let entry =
                    query_handle_entry(&mut self.connection, &self.tree, base, String::new())
                        .await?;
                if !entry.is_dir || entry.is_symlink || entry.file_id != self.base_identity {
                    return Err(crate::real_mode::plan_drift(
                        "SMB-Verzeichnisstreambasis änderte sich",
                    ));
                }
                let names = stream_list(
                    &mut self.connection,
                    &self.tree,
                    base,
                    StreamBaseKind::Directory,
                )
                .await?;
                let mut summaries = BTreeMap::new();
                for (name, size) in &names {
                    let value = if let Some((_, id)) =
                        live.filter(|(live_name, _)| *live_name == name.as_slice())
                    {
                        hash_stream_handle(
                            &mut self.connection,
                            &self.tree,
                            id,
                            *size,
                            self.max_read,
                            None,
                        )
                        .await?
                    } else {
                        hash_stream(
                            &mut self.connection,
                            &self.tree,
                            &stream_path(&self.remote, name)?,
                            *size,
                            self.max_read,
                            None,
                        )
                        .await?
                    };
                    summaries.insert(
                        name.clone(),
                        NamedBlobSummary {
                            name: name.clone(),
                            value,
                        },
                    );
                }
                if stream_list(
                    &mut self.connection,
                    &self.tree,
                    base,
                    StreamBaseKind::Directory,
                )
                .await?
                    != names
                {
                    return Err(crate::real_mode::plan_drift(
                        "SMB-Verzeichnisstreammenge änderte sich beim Readback",
                    ));
                }
                if previous.as_ref().is_some_and(|before| before != &summaries) {
                    return Err(crate::real_mode::plan_drift(
                        "SMB-Verzeichnisstreaminhalt änderte sich beim Readback",
                    ));
                }
                previous = Some(summaries);
            }
            Ok(previous.unwrap_or_default())
        })
    }

    fn verify(&mut self, live: Option<(&[u8], FileId)>, expected: &StreamSet) -> io::Result<()> {
        if &self.snapshot(live)? != expected {
            return Err(crate::real_mode::plan_drift(
                "SMB-Verzeichnisstreambestand weicht von der bestätigten vollständigen Menge ab",
            ));
        }
        Ok(())
    }

    fn outcome(&self, effect: DirectoryStreamEffect) -> DirectoryStreamOutcome {
        DirectoryStreamOutcome {
            directory: self.directory.to_string_lossy().into_owned(),
            effect,
        }
    }

    fn uncertain_error(&mut self, error: io::Error, intent: usize) -> io::Error {
        self.blocked = true;
        crate::error::with_recovery_context(
            crate::coded_error(
                error.kind(),
                crate::ErrorCode::OutcomeUnclear,
                format!("SMB-Verzeichnisstreamwirkung oder Abschluss unklar: {error}"),
            ),
            &[&error],
            &[self.intents[intent].path()],
        )
    }

    fn intent(
        &mut self,
        temp: Option<Vec<u8>>,
        effect: DirectoryStreamEffect,
    ) -> io::Result<usize> {
        let intent = RemoteTempIntent::create_named_stream_with_root(
            &self.owner,
            NamedStreamIntentBinding {
                base_directory: self.directory.clone(),
                temporary_name: temp.clone(),
                final_name: effect.name().to_vec(),
                effect: effect.clone(),
                phase: NamedStreamIntentPhase::Prepared,
            },
            &self.root_identity,
            self.account,
        )?;
        for name in temp.iter().chain(std::iter::once(&effect.name().to_vec())) {
            self.recovery_hints.push(
                crate::local_staging::StagingObject::NamedStream {
                    base_directory: self.directory.clone(),
                    name: name.clone(),
                }
                .recovery_hint(),
            );
        }
        self.intents.push(intent);
        Ok(self.intents.len() - 1)
    }

    fn close(&mut self) -> io::Result<()> {
        let base = self.base.take().map_or(Ok(()), |base| {
            self.runtime
                .block_on(self.tree.close_handle(&mut self.connection, base))
                .map_err(smb_error)
        });
        let parents = self.runtime.block_on(close_parents(
            &mut self.connection,
            &self.tree,
            &mut self.parents,
        ));
        base.and(parents)
    }
}

async fn exclusive_stream(
    connection: &mut Connection,
    tree: &Tree,
    path: &str,
    disposition: CreateDisposition,
) -> io::Result<FileId> {
    let request = CreateRequest {
        requested_oplock_level: OplockLevel::None,
        impersonation_level: ImpersonationLevel::Impersonation,
        desired_access: FileAccessMask::new(
            FileAccessMask::FILE_READ_DATA
                | if matches!(disposition, CreateDisposition::FileCreate) {
                    FileAccessMask::FILE_WRITE_DATA
                } else {
                    0
                }
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::DELETE
                | FileAccessMask::SYNCHRONIZE,
        ),
        file_attributes: FILE_ATTRIBUTE_NORMAL,
        share_access: ShareAccess(0),
        create_disposition: disposition,
        create_options: FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
        name: path.into(),
        create_contexts: Vec::new(),
    };
    let frame = connection
        .execute(Command::Create, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::Create)?;
    let opened = CreateResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
    if opened.file_attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        let _ = tree.close_handle(connection, opened.file_id).await;
        return Err(invalid("SMB-Streamcreator besitzt einen ungültigen Typ"));
    }
    Ok(opened.file_id)
}

async fn disposition(connection: &mut Connection, tree: &Tree, id: FileId) -> io::Result<()> {
    let request = SetInfoRequest {
        info_type: InfoType::File,
        file_info_class: 13,
        additional_information: 0,
        file_id: id,
        buffer: vec![1],
    };
    let frame = connection
        .execute(Command::SetInfo, &request, Some(tree.tree_id))
        .await
        .map_err(smb_error)?;
    require_status(&frame, Command::SetInfo)
}

impl DirectoryStreamTransaction for DirectoryTransaction {
    fn stage(
        &mut self,
        desired: &NamedBlobSummary,
    ) -> io::Result<Box<dyn DirectoryStreamTemp + '_>> {
        stream_name(&desired.name)?;
        self.check()?;
        self.verify(None, &self.expected.clone())?;
        self.check()?;
        let effect = match self.expected.get(&desired.name) {
            Some(before) if before == desired => {
                return Err(invalid(
                    "Identischer SMB-Verzeichnisstream benötigt keinen Schreibversuch",
                ))
            }
            Some(before) => DirectoryStreamEffect::Replace {
                before: before.clone(),
                after: desired.clone(),
            },
            None => DirectoryStreamEffect::Add {
                after: desired.clone(),
            },
        };
        let desired_key = lookup_key(stream_name(&desired.name)?);
        if self.expected.keys().any(|name| {
            name != &desired.name
                && stream_name(name).is_ok_and(|name| lookup_key(name) == desired_key)
        }) {
            return Err(invalid(
                "SMB-Verzeichnisstreamname kollidiert mit einem bestehenden Namen",
            ));
        }
        let temp = format!(
            ".doppeldecker-stream-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
        .into_bytes();
        let path = stream_path(&self.remote, &temp)?;
        let intent = self.intent(Some(temp.clone()), effect.clone())?;
        let id = self
            .runtime
            .block_on(exclusive_stream(
                &mut self.connection,
                &self.tree,
                &path,
                CreateDisposition::FileCreate,
            ))
            .map_err(|error| self.uncertain_error(error, intent))?;
        Ok(Box::new(DirectoryTemp {
            transaction: self,
            id: Some(id),
            temp,
            desired: desired.clone(),
            effect,
            intent,
            offset: 0,
            failed: false,
            renamed: false,
        }))
    }

    fn remove(&mut self, before: &NamedBlobSummary) -> io::Result<DirectoryStreamEffect> {
        self.check()?;
        stream_name(&before.name)?;
        if self.expected.get(&before.name) != Some(before) {
            return Err(crate::real_mode::plan_drift(
                "SMB-Streamlöschung entspricht nicht dem bestätigten Bestand",
            ));
        }
        self.verify(None, &self.expected.clone())?;
        let effect = DirectoryStreamEffect::Remove {
            before: before.clone(),
        };
        let intent = self.intent(None, effect.clone())?;
        let path = stream_path(&self.remote, &before.name)?;
        // Der frische Handle wird niemals umbenannt. Creator-Delete nach Rename
        // ist auf Samba kein Wirkungsbeleg und kommt hier absichtlich nicht vor.
        let id = self.runtime.block_on(exclusive_stream(
            &mut self.connection,
            &self.tree,
            &path,
            CreateDisposition::FileOpen,
        ))?;
        let result = (|| {
            self.verify(Some((&before.name, id)), &self.expected.clone())?;
            self.check()?;
            self.intents[intent].named_stream_phase(NamedStreamIntentPhase::Removing)?;
            let outcome = self.outcome(effect.clone());
            self.report.uncertain.push(outcome.clone());
            self.runtime
                .block_on(disposition(&mut self.connection, &self.tree, id))?;
            Ok(outcome)
        })();
        let close = self
            .runtime
            .block_on(self.tree.close_handle(&mut self.connection, id))
            .map_err(smb_error);
        let result = result.and_then(|outcome| close.map(|()| outcome));
        let outcome = result.map_err(|error| self.uncertain_error(error, intent))?;
        let mut expected = self.expected.clone();
        expected.remove(&before.name);
        self.verify(None, &expected)
            .map_err(|error| self.uncertain_error(error, intent))?;
        (self.check_namespace)().map_err(|error| self.uncertain_error(error, intent))?;
        self.expected = expected;
        self.report.uncertain.retain(|value| value != &outcome);
        self.report.completed.push(outcome);
        self.intents[intent]
            .named_stream_phase(NamedStreamIntentPhase::Removed)
            .map_err(|error| self.uncertain_error(error, intent))?;
        Ok(effect)
    }

    fn report(&self) -> DirectoryStreamReport {
        self.report.clone()
    }
    fn recovery_paths(&self) -> Vec<String> {
        let mut paths: Vec<_> = self
            .intents
            .iter()
            .map(|intent| intent.path().to_string_lossy().into_owned())
            .collect();
        paths.extend(self.recovery_hints.iter().cloned());
        paths.sort();
        paths.dedup();
        paths
    }

    fn finish(
        mut self: Box<Self>,
        expected_final: &[NamedBlobSummary],
    ) -> io::Result<DirectoryStreamReport> {
        self.check()?;
        let expected = stream_set(expected_final)?;
        if self.expected != expected {
            return Err(crate::real_mode::plan_drift(
                "SMB-Verzeichnisstreamabschluss entspricht nicht der gewünschten Menge",
            ));
        }
        self.verify(None, &expected)?;
        (self.check_namespace)()?;
        self.close().map_err(|error| {
            crate::error::with_recovery_paths(
                crate::coded_error(
                    error.kind(),
                    crate::ErrorCode::OutcomeUnclear,
                    format!("SMB-Verzeichnisstreamabschluss beim Schließen unklar: {error}"),
                ),
                &self.recovery_paths(),
            )
        })?;
        for intent in &self.intents {
            intent.finish().map_err(|error| {
                crate::error::with_recovery_paths(
                    crate::coded_error(
                        error.kind(),
                        crate::ErrorCode::OutcomeUnclear,
                        format!("SMB-Verzeichnisstreamabschluss im Intentindex unklar: {error}"),
                    ),
                    &self.recovery_paths(),
                )
            })?;
        }
        Ok(self.report.clone())
    }
}

impl Drop for DirectoryTransaction {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct DirectoryTemp<'a> {
    transaction: &'a mut DirectoryTransaction,
    id: Option<FileId>,
    temp: Vec<u8>,
    desired: NamedBlobSummary,
    effect: DirectoryStreamEffect,
    intent: usize,
    offset: u64,
    failed: bool,
    renamed: bool,
}

impl DirectoryTemp<'_> {
    fn close(&mut self) -> io::Result<()> {
        self.id.take().map_or(Ok(()), |id| {
            self.transaction
                .runtime
                .block_on(
                    self.transaction
                        .tree
                        .close_handle(&mut self.transaction.connection, id),
                )
                .map_err(smb_error)
        })
    }

    fn flush_live(&mut self) -> io::Result<()> {
        if self.failed || self.renamed {
            return Err(invalid("SMB-Streamcreator ist nicht mehr schreibbar"));
        }
        self.transaction.check()?;
        if self.offset != self.desired.value.size {
            return Err(invalid(
                "SMB-Verzeichnisstream wurde nicht vollständig geschrieben",
            ));
        }
        let id = self
            .id
            .ok_or_else(|| invalid("SMB-Streamcreator ist geschlossen"))?;
        self.transaction.runtime.block_on(async {
            flush_handle(&mut self.transaction.connection, &self.transaction.tree, id).await?;
            #[cfg(test)]
            test_fault("flush")?;
            if hash_stream_handle(
                &mut self.transaction.connection,
                &self.transaction.tree,
                id,
                self.desired.value.size,
                self.transaction.max_read,
                None,
            )
            .await?
                != self.desired.value
            {
                return Err(invalid("SMB-Verzeichnisstreamhash stimmt nicht überein"));
            }
            Ok(())
        })
    }
}

impl Write for DirectoryTemp<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed || self.renamed {
            return Err(invalid("SMB-Streamcreator ist nicht mehr schreibbar"));
        }
        self.transaction.check()?;
        if bytes.is_empty() {
            return Ok(0);
        }
        let remaining = self
            .desired
            .value
            .size
            .checked_sub(self.offset)
            .ok_or_else(|| invalid("SMB-Streamgröße überschritten"))?;
        if remaining == 0 {
            return Err(invalid("SMB-Streamgröße überschritten"));
        }
        let count = bytes
            .len()
            .min(self.transaction.max_write as usize)
            .min(remaining.min(usize::MAX as u64) as usize);
        let request = WriteRequest {
            data_offset: 0x70,
            offset: self.offset,
            file_id: self
                .id
                .ok_or_else(|| invalid("SMB-Streamcreator ist geschlossen"))?,
            channel: 0,
            remaining_bytes: 0,
            write_channel_info_offset: 0,
            write_channel_info_length: 0,
            flags: 0,
            data: bytes[..count].to_vec(),
        };
        let result = self.transaction.runtime.block_on(async {
            let frame = self
                .transaction
                .connection
                .execute_with_credits(
                    Command::Write,
                    &request,
                    Some(self.transaction.tree.tree_id),
                    CreditCharge((count as u64).div_ceil(65_536).max(1) as u16),
                )
                .await
                .map_err(smb_error)?;
            require_status(&frame, Command::Write)?;
            let response =
                WriteResponse::unpack(&mut ReadCursor::new(&frame.body)).map_err(smb_error)?;
            #[cfg(test)]
            test_fault("write")?;
            if response.count == 0 || response.count as usize > count {
                return Err(invalid("Ungültige SMB-Stream-WRITE-Bytezahl"));
            }
            Ok(response.count as usize)
        });
        match result {
            Ok(count) => {
                self.offset += count as u64;
                Ok(count)
            }
            Err(error) => {
                self.failed = true;
                Err(self.transaction.uncertain_error(error, self.intent))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_live().map_err(|error| {
            self.failed = true;
            self.transaction.uncertain_error(error, self.intent)
        })
    }
}

impl DirectoryStreamTemp for DirectoryTemp<'_> {
    fn commit(
        mut self: Box<Self>,
        before: Option<&NamedBlobSummary>,
    ) -> io::Result<DirectoryStreamEffect> {
        if self.effect.before() != before {
            return Err(invalid("SMB-Streamcommit besitzt einen anderen Zielbeleg"));
        }
        self.flush()?;
        let id = self
            .id
            .ok_or_else(|| invalid("SMB-Streamcreator ist geschlossen"))?;
        let mut staged = self.transaction.expected.clone();
        staged.insert(
            self.temp.clone(),
            NamedBlobSummary {
                name: self.temp.clone(),
                value: self.desired.value.clone(),
            },
        );
        self.transaction.verify(Some((&self.temp, id)), &staged)?;
        self.transaction.check()?;
        self.transaction.intents[self.intent]
            .named_stream_phase(NamedStreamIntentPhase::Committing)?;
        let outcome = self.transaction.outcome(self.effect.clone());
        self.transaction.report.uncertain.push(outcome.clone());
        // Ab hier darf auch bei einem Fehler niemals Creator-Delete aufgerufen
        // werden: Der Server kann den genau einmal gesendeten Rename ausgeführt haben.
        self.renamed = true;
        let request = SetInfoRequest {
            info_type: InfoType::File,
            file_info_class: FILE_RENAME_INFORMATION,
            additional_information: 0,
            file_id: id,
            buffer: rename_buffer(
                &format!(":{}:$DATA", stream_name(&self.desired.name)?),
                before.is_some(),
            ),
        };
        let result = self.transaction.runtime.block_on(async {
            let frame = self
                .transaction
                .connection
                .execute(
                    Command::SetInfo,
                    &request,
                    Some(self.transaction.tree.tree_id),
                )
                .await
                .map_err(smb_error)?;
            require_status(&frame, Command::SetInfo)
        });
        if let Err(error) = result {
            return Err(self.transaction.uncertain_error(error, self.intent));
        }
        #[cfg(test)]
        test_fault("rename")
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        let mut expected = self.transaction.expected.clone();
        expected.insert(self.desired.name.clone(), self.desired.clone());
        self.transaction
            .verify(Some((&self.desired.name, id)), &expected)
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        self.close()
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        #[cfg(test)]
        test_fault("close")
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        self.transaction
            .verify(None, &expected)
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        (self.transaction.check_namespace)()
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        self.transaction.expected = expected;
        self.transaction
            .report
            .uncertain
            .retain(|value| value != &outcome);
        self.transaction.report.completed.push(outcome);
        self.transaction.intents[self.intent]
            .named_stream_phase(NamedStreamIntentPhase::Committed)
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        Ok(self.effect.clone())
    }

    fn abort(mut self: Box<Self>) -> io::Result<()> {
        if self.renamed {
            return Err(invalid(
                "SMB-Creator nach Rename darf nicht als Temp gelöscht werden",
            ));
        }
        let id = self
            .id
            .ok_or_else(|| invalid("SMB-Streamcreator ist geschlossen"))?;
        // Cancel verhindert weitere Produktwirkungen, aber nicht die belegte
        // Entfernung dieses eigenen, ununterbrochen gehaltenen Creators.
        (self.transaction.check_namespace)()?;
        let result = self.transaction.runtime.block_on(disposition(
            &mut self.transaction.connection,
            &self.transaction.tree,
            id,
        ));
        let close = self.close();
        result
            .and(close)
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        self.transaction
            .verify(None, &self.transaction.expected.clone())
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        (self.transaction.check_namespace)()
            .map_err(|error| self.transaction.uncertain_error(error, self.intent))?;
        self.transaction.blocked = false;
        Ok(())
    }
}

impl Drop for DirectoryTemp<'_> {
    fn drop(&mut self) {
        // Drop kann weder Fehler melden noch Besitz nach Close beweisen. Der
        // konkrete Registrylocator bleibt deshalb für die Recovery erhalten.
        if self.id.is_some() {
            self.transaction.blocked = true;
        }
        let _ = self.close();
    }
}

#[cfg(test)]
#[path = "smb_directory_streams_tests.rs"]
mod tests;
