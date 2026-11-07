//! Vor dem DACL-SET gewährter FLUSH-Zugriff eigener Temps; keine neue Schreibfreigabe.
use super::{windows_dacl as wire, *};
use crate::windows_dacl::WindowsDaclWriteContext;
use crate::windows_security::WindowsDacl;

#[derive(Clone)]
pub(super) struct Proof {
    root: crate::RootIdentity,
    path: PathBuf,
    inode: u64,
    acl: WindowsDacl,
    context: WindowsDaclWriteContext,
}

pub(super) enum TempDaclFlush {
    Pending(Box<GrantedFlush>),
    // Daten-FLUSH über den Sparse-Guard; dessen CLOSE folgt erst nach Rename.
    SparsePending(Box<Proof>),
    Complete(Box<Proof>),
    Failed(Box<Proof>),
}

pub(super) struct GrantedFlush {
    proof: Box<Proof>,
    provider: Arc<SmbVfs>,
    tree: Arc<Tree>,
    connection: Connection,
    id: Option<FileId>,
}

impl TempDaclFlush {
    fn proof(&self) -> &Proof {
        match self {
            Self::Pending(granted) => &granted.proof,
            Self::SparsePending(proof) | Self::Complete(proof) | Self::Failed(proof) => proof,
        }
    }

    pub(super) fn grant(
        scope: &OrdinarySmb,
        path: &Path,
        acl: &WindowsDacl,
        context: &WindowsDaclWriteContext,
    ) -> io::Result<Self> {
        #[cfg(test)]
        FAULT.with(|fault| {
            if fault.get() == Some("grant_denied") {
                fault.set(None);
                Err(crate::windows_dacl::write_omission(
                    crate::windows_dacl::WindowsDaclTargetIssue::RequiredAccessDenied,
                ))
            } else {
                Ok(())
            }
        })?;
        let (temp, inode) = scope
            .file
            .as_ref()
            .and_then(|f| lock(&f.temp).clone())
            .ok_or_else(|| invalid("FLUSH benötigt eine eigene Temp-ID"))?;
        if temp != path {
            return Err(invalid("FLUSH-Temp liegt außerhalb seiner Besitzbindung"));
        }
        if scope.sparse_dacl_handle(path)?.is_some() {
            return Ok(Self::SparsePending(Box::new(Proof {
                root: scope.identity.clone(),
                path: path.into(),
                inode,
                acl: acl.clone(),
                context: context.clone(),
            })));
        }
        let remote = scope.provider.remote_path(path)?;
        let (tree, mut connection) = {
            let state = lock(&scope.provider.state);
            (state.session.tree.clone(), state.session.connection.clone())
        };
        // Dieses Handle erhält Daten-Schreiben vor dem restriktiven SET, wird
        // aber ausschließlich für Identitätsprüfung, FLUSH und CLOSE verwendet.
        // Ohne DELETE und mit Share-Delete bleibt der eigene exklusive SET- und
        // spätere Commitzugriff unverändert möglich.
        let id = scope.provider.runtime.block_on(wire::open(
            &mut connection,
            &tree,
            &remote,
            FileAccessMask::FILE_WRITE_DATA
                | FileAccessMask::FILE_READ_ATTRIBUTES
                | FileAccessMask::SYNCHRONIZE,
            false,
            true,
        ))?;
        let mut granted = GrantedFlush {
            proof: Box::new(Proof {
                root: scope.identity.clone(),
                path: path.to_path_buf(),
                inode,
                acl: acl.clone(),
                context: context.clone(),
            }),
            provider: scope.provider.clone(),
            tree,
            connection,
            id: Some(id),
        };
        // Der Guard existiert bereits, falls dieser zweite Identitätsbeleg scheitert.
        granted.provider.runtime.block_on(wire::identity(
            &mut granted.connection,
            &granted.tree,
            id,
            inode,
        ))?;
        Ok(Self::Pending(Box::new(granted)))
    }
}

impl GrantedFlush {
    fn close(&mut self) -> io::Result<()> {
        let Some(id) = self.id.take() else {
            return Ok(());
        };
        #[cfg(test)]
        CLOSES.with(|count| count.set(count.get() + 1));
        // Auch ein verlorener CLOSE-Abschluss legitimiert keinen zweiten Versuch.
        self.provider
            .runtime
            .block_on(self.tree.close_handle(&mut self.connection, id))
            .map_err(smb_error)
    }

    fn finish(&mut self, scope: &OrdinarySmb) -> io::Result<()> {
        let mut held = Vec::new();
        let result = (|| {
            scope.check(&self.proof.path, true)?;
            scope.check_flush_proof(&self.proof.path, &self.proof)?;
            let remote = self.provider.remote_path(&self.proof.path)?;
            let root = self.provider.remote_path(&scope.root)?;
            let id = self
                .id
                .ok_or_else(|| invalid("FLUSH-Handle wurde bereits verbraucht"))?;
            self.provider.runtime.block_on(async {
                wire::parents(&mut self.connection, &self.tree, &remote, &root, &mut held).await?;
                // Ein frisches DELETE-Handle bindet den Namen während FLUSH,
                // ohne erneut ein inzwischen verweigertes Datenrecht anzufragen.
                let current = wire::open(
                    &mut self.connection,
                    &self.tree,
                    &remote,
                    FileAccessMask::DELETE
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::READ_CONTROL
                        | FileAccessMask::SYNCHRONIZE,
                    false,
                    false,
                )
                .await?;
                held.push(current);
                wire::identity(&mut self.connection, &self.tree, current, self.proof.inode).await?;
                wire::identity(&mut self.connection, &self.tree, id, self.proof.inode).await?;
                wire::exact(
                    &wire::query(
                        &mut self.connection,
                        &self.tree,
                        current,
                        self.proof.context.namespace,
                    )
                    .await?,
                    &self.proof.acl,
                    &self.proof.context,
                )?;
                #[cfg(test)]
                fault("before_flush")?;
                #[cfg(test)]
                FLUSHES.with(|count| count.set(count.get() + 1));
                flush_handle(&mut self.connection, &self.tree, id).await?;
                #[cfg(test)]
                fault("after_flush")?;
                wire::identity(&mut self.connection, &self.tree, current, self.proof.inode).await?;
                wire::exact(
                    &wire::query(
                        &mut self.connection,
                        &self.tree,
                        current,
                        self.proof.context.namespace,
                    )
                    .await?,
                    &self.proof.acl,
                    &self.proof.context,
                )
            })?;
            scope.check(&self.proof.path, true)
        })();
        let closed = self.close();
        #[cfg(test)]
        let closed = closed.and_then(|()| fault("close_reply"));
        let parents_closed = self.provider.runtime.block_on(wire::close_all(
            &mut self.connection,
            &self.tree,
            &mut held,
        ));
        result.and(closed).and(parents_closed)
    }
}

impl Drop for GrantedFlush {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl OrdinarySmb {
    fn check_flush_proof(&self, path: &Path, proof: &Proof) -> io::Result<()> {
        let owned = self.file.as_ref().and_then(|file| lock(&file.temp).clone());
        if self.identity != proof.root
            || path != proof.path
            || owned != Some((proof.path.clone(), proof.inode))
        {
            return Err(crate::real_mode::plan_drift(
                "SMB-Temp-FLUSH-Bindung änderte sich",
            ));
        }
        Ok(())
    }

    pub(super) fn dacl_temp_mtime_identity(&self, path: &Path) -> io::Result<Option<u64>> {
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let completion = lock(&file.dacl_flush);
        let Some(completion) = completion.as_ref() else {
            return Ok(None);
        };
        self.check_flush_proof(path, completion.proof())?;
        match completion {
            TempDaclFlush::Pending(granted) => Ok(Some(granted.proof.inode)),
            TempDaclFlush::SparsePending(proof) => Ok(Some(proof.inode)),
            _ => Err(self.dacl_unclear(path, self.staging_recovery_path(), invalid(
                "SMB-Tempmetadaten dürfen nach verbrauchtem FLUSH nicht erneut geschrieben werden",
            ))),
        }
    }

    pub(super) fn check_dacl_temp_flushed(&self, path: &Path) -> io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let completion = lock(&file.dacl_flush);
        let Some(completion) = completion.as_ref() else {
            return Ok(());
        };
        self.check_flush_proof(path, completion.proof())?;
        if matches!(completion, TempDaclFlush::Complete(_)) {
            Ok(())
        } else {
            Err(self.dacl_unclear(
                path,
                self.staging_recovery_path(),
                invalid("SMB-DACL-Temp besitzt noch keinen geprüften FLUSH-/CLOSE-Abschluss"),
            ))
        }
    }

    fn finish_sparse_dacl(&self, proof: &Proof) -> io::Result<()> {
        self.check(&proof.path, true)?;
        self.check_flush_proof(&proof.path, proof)?;
        let (tree, mut connection, id) = self
            .sparse_dacl_handle(&proof.path)?
            .ok_or_else(|| invalid("Gemeinsamer Sparse-/DACL-Abschluss fehlt"))?;
        let remote = self.provider.remote_path(&proof.path)?;
        let root = self.provider.remote_path(&self.root)?;
        let mut held = Vec::new();
        let result = (|| {
            self.provider.runtime.block_on(async {
                wire::parents(&mut connection, &tree, &remote, &root, &mut held).await?;
                let current = wire::open(
                    &mut connection,
                    &tree,
                    &remote,
                    FileAccessMask::DELETE
                        | FileAccessMask::FILE_READ_ATTRIBUTES
                        | FileAccessMask::READ_CONTROL
                        | FileAccessMask::SYNCHRONIZE,
                    false,
                    false,
                )
                .await?;
                held.push(current);
                wire::identity(&mut connection, &tree, current, proof.inode).await?;
                wire::identity(&mut connection, &tree, id, proof.inode).await?;
                wire::exact(
                    &wire::query(&mut connection, &tree, current, proof.context.namespace).await?,
                    &proof.acl,
                    &proof.context,
                )
            })?;
            #[cfg(test)]
            fault("before_flush")?;
            #[cfg(test)]
            FLUSHES.with(|count| count.set(count.get() + 1));
            self.sync_sparse_metadata(&proof.path)?;
            #[cfg(test)]
            fault("after_flush")?;
            self.provider.runtime.block_on(async {
                wire::identity(&mut connection, &tree, id, proof.inode).await?;
                wire::exact(
                    &wire::query(&mut connection, &tree, id, proof.context.namespace).await?,
                    &proof.acl,
                    &proof.context,
                )
            })?;
            self.check(&proof.path, true)
        })();
        let closed =
            self.provider
                .runtime
                .block_on(wire::close_all(&mut connection, &tree, &mut held));
        super::sparse_write::combine(result, closed, &proof.path)
    }

    pub(super) fn sync_dacl_temp(&self, path: &Path) -> io::Result<bool> {
        let Some(file) = &self.file else {
            return Ok(false);
        };
        let pending = {
            let mut completion = lock(&file.dacl_flush);
            let Some(current) = completion.as_ref() else {
                return Ok(false);
            };
            if file.attempted.load(Ordering::SeqCst) {
                return Err(self.dacl_unclear(
                    path,
                    self.staging_recovery_path(),
                    invalid("SMB-DACL-Temp mit unklarem Effekt wird nicht weiter ausgeführt"),
                ));
            }
            self.check_flush_proof(path, current.proof())?;
            if !matches!(
                current,
                TempDaclFlush::Pending(_) | TempDaclFlush::SparsePending(_)
            ) {
                return Err(self.dacl_unclear(
                    path,
                    self.staging_recovery_path(),
                    invalid("SMB-Temp-FLUSH ist bereits verbraucht und wird nicht wiederholt"),
                ));
            }
            let failed = TempDaclFlush::Failed(Box::new(current.proof().clone()));
            completion.replace(failed).expect("Pending geprüft")
        };
        #[cfg(test)]
        BEFORE_FLUSH.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let (proof, result) = match pending {
            TempDaclFlush::Pending(mut granted) => (granted.proof.clone(), granted.finish(self)),
            TempDaclFlush::SparsePending(proof) => {
                let result = self.finish_sparse_dacl(&proof);
                (proof, result)
            }
            _ => unreachable!(),
        };
        match result {
            Ok(()) => {
                *lock(&file.dacl_flush) = Some(TempDaclFlush::Complete(proof));
                Ok(true)
            }
            Err(error) => {
                file.attempted.store(true, Ordering::SeqCst);
                Err(self.dacl_unclear(path, self.staging_recovery_path(), error))
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
    pub(super) static BEFORE_FLUSH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static FLUSHES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static CLOSES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
fn fault(point: &str) -> io::Result<()> {
    FAULT.with(|fault| {
        if fault.get() == Some(point) {
            fault.set(None);
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Künstlicher Temp-FLUSH-Abschlussfehler",
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
#[path = "smb_temp_dacl_flush_tests.rs"]
pub(super) mod tests;
