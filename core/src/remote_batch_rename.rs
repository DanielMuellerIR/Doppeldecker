//! Gemeinsame kurzlebige Remote-Stapel über den gewöhnlichen Renamekoordinator.
use crate::batch_rename::{
    chain_execution_plan, finish_report, plan_rows, plan_rows_with_chains, set_status,
    validate_batch_rename_request, BatchRenameOptions, BatchRenameReport, BatchRenameRow,
    BatchRenameStatus, PLAN_LIFETIME,
};
use crate::{
    Cancel, ErrorCode, ProviderEndpoint, ProviderKind, RenameExecutionClass, RenameOutcome,
};
use std::{io, path::PathBuf, time::Instant};
mod cycle;
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ParentProof {
    pub entries: Vec<(String, [u8; 32], [u8; 32])>,
    pub root: String,
}
impl ParentProof {
    fn own_rename_refresh(&self, current: &Self) -> bool {
        // Nur Größe/Zeit des tatsächlichen Pane-Elternordners dürfen aus der
        // geklärten eigenen Namenswirkung übernommen werden. Rechte und Owner
        // sowie sämtliche anderen Eltern bleiben an den alten Beleg gebunden.
        self.root == current.root
            && self.entries.len() == current.entries.len()
            && self.entries.iter().zip(&current.entries).all(|(old, new)| {
                old.0 == new.0 && old.2 == new.2 && (old.0 == self.root || old.1 == new.1)
            })
    }
}
pub(crate) struct RemoteBatchRefresh {
    pub identity: crate::RootIdentity,
    pub parents: ParentProof,
}
pub(crate) struct PreparedRemoteBatchRename {
    endpoint: ProviderEndpoint,
    root: PathBuf,
    identity: crate::RootIdentity,
    parents: Option<ParentProof>,
    proofs: Vec<Option<(crate::Entry, [u8; 32])>>,
    class: RenameExecutionClass,
    rows: Vec<BatchRenameRow>,
    limitations: Vec<String>,
    execution_order: Vec<usize>,
    dependencies: Vec<Vec<usize>>,
    spelling: Vec<Option<Vec<u8>>>,
    sftp_spelling: Vec<bool>,
    ftp_spelling: Vec<bool>,
    cycles: Vec<crate::remote_rename_cycle::RemoteRenameCycle>,
    created: Instant,
}
fn drift(message: &str) -> io::Error {
    crate::coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift, message)
}
fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(crate::coded_error(
            io::ErrorKind::Interrupted,
            ErrorCode::Interrupted,
            "Remote-Stapel abgebrochen",
        ))
    } else {
        Ok(())
    }
}
/// Prüft Auswahl und Optionen vor jedem Providerzugriff; Einzelprobleme bleiben Vorschauzeilen.
pub(crate) fn validate_remote_batch_rename_request(
    names: &[String],
    options: &BatchRenameOptions,
) -> io::Result<()> {
    validate_batch_rename_request(names, options)
}
impl PreparedRemoteBatchRename {
    /// Hält den gewählten Pane-Bestand und beobachtbare Metadaten, keine Inhalte.
    pub fn prepare(
        endpoint: &ProviderEndpoint,
        names: &[String],
        options: &BatchRenameOptions,
        cancel: &Cancel,
    ) -> io::Result<Self> {
        validate_remote_batch_rename_request(names, options)?;
        if !matches!(
            endpoint.kind(),
            ProviderKind::Sftp | ProviderKind::Smb | ProviderKind::Ftps | ProviderKind::WebDav
        ) {
            return Err(crate::coded_error(
                io::ErrorKind::Unsupported,
                ErrorCode::Unsupported,
                "Dieser Stapel unterstützt SFTP, SMB, FTP/FTPS und WebDAV",
            ));
        }
        check_cancel(cancel)?;
        let context = endpoint.remote_batch_context(cancel)?;
        let root = endpoint.root().to_path_buf();
        let identity = context.root_identity(&root)?;
        let parents = endpoint.remote_batch_parents(cancel)?;
        let listing = context.list_complete(&root)?;
        crate::fileops::require_complete_safe_listing(&listing, "Remote-Stapelbestand")?;
        if endpoint.kind() == ProviderKind::WebDav {
            crate::WebDavVfs::validate_batch_rename_listing(&listing.entries)?;
        }
        let allow_chains = matches!(
            endpoint.kind(),
            ProviderKind::Smb | ProviderKind::WebDav | ProviderKind::Sftp | ProviderKind::Ftps
        );
        let (mut rows, listed) = if allow_chains {
            plan_rows_with_chains(&listing, names, options, true)
        } else {
            plan_rows(&listing, names, options)
        };
        let spelling = smb_spelling_candidates(endpoint.kind(), &listing, &mut rows);
        let sftp_spelling = sftp_spelling_candidates(endpoint.kind(), &listing, &mut rows);
        let ftp_spelling = ftp_spelling_candidates(endpoint.kind(), &listing, &mut rows);
        let webdav_spelling = webdav_spelling_candidates(endpoint.kind(), &listing, &rows);
        let mut proofs = vec![None; rows.len()];
        for (index, (row, proof)) in rows.iter_mut().zip(&mut proofs).enumerate() {
            check_cancel(cancel)?;
            if row.status == BatchRenameStatus::Ready {
                let (entry, observed) =
                    endpoint.remote_batch_source(&root.join(&row.old_name), cancel)?;
                if !listed[index]
                    .as_ref()
                    .is_some_and(|expected| crate::batch_rename::same_source(expected, &entry))
                {
                    return Err(drift(
                        "Remote-Quelle änderte sich zwischen Listing und Vorbereitung",
                    ));
                }
                if entry.is_symlink || (!entry.is_file && !entry.is_dir) {
                    set_status(
                        row,
                        BatchRenameStatus::Blocked,
                        "Quelle ist kein belegter regulärer Eintrag",
                    );
                } else {
                    *proof = Some((entry, observed));
                }
            }
        }
        let cycles = crate::remote_rename_cycle::plan_cycles(&listing, &rows)?;
        let (mut execution_order, mut dependencies) = if allow_chains {
            chain_execution_plan(&listing, &mut rows)
        } else {
            ((0..rows.len()).collect(), vec![Vec::new(); rows.len()])
        };
        for cycle in &cycles {
            for &index in &cycle.members {
                if proofs[index].is_some() {
                    set_status(&mut rows[index], BatchRenameStatus::Ready, format!(
                        "Dateinamenzyklus über Zwischenname „{}“; {} einzelne Rename-Schritte, kein atomarer Gesamttausch. Teiländerungen bleiben zur manuellen Prüfung erhalten.", cycle.temp_name, cycle.members.len()+1));
                    dependencies[index].clear();
                    execution_order.push(index);
                }
            }
        }
        for (index, token) in spelling.iter().enumerate() {
            if token.is_some() && proofs[index].is_some() {
                set_status(
                    &mut rows[index],
                    BatchRenameStatus::Ready,
                    SMB_SPELLING_PREVIEW,
                );
                execution_order.push(index);
            }
        }
        for (index, &candidate) in sftp_spelling.iter().enumerate() {
            if candidate && proofs[index].is_some() {
                set_status(
                    &mut rows[index],
                    BatchRenameStatus::Ready,
                    SFTP_SPELLING_PREVIEW,
                );
                execution_order.push(index);
            }
        }
        for (index, &candidate) in ftp_spelling.iter().enumerate() {
            if candidate && proofs[index].is_some() {
                set_status(
                    &mut rows[index],
                    BatchRenameStatus::Ready,
                    FTP_SPELLING_PREVIEW,
                );
                execution_order.push(index);
            }
        }
        // Nur die eigene eindeutige Schreibweise aus dem Kettengate lösen.
        // WebDAV behält seinen gewöhnlichen bedingten MOVE und vollständigen
        // Namespace-Nachbeleg samt eigener Elternaktualisierung unverändert.
        for (index, &candidate) in webdav_spelling.iter().enumerate() {
            if candidate && proofs[index].is_some() {
                set_status(
                    &mut rows[index],
                    BatchRenameStatus::Ready,
                    "WebDAV-Schreibweise mit einem bedingten MOVE ändern, sofern der Server sie zulässt",
                );
                dependencies[index].clear();
                execution_order.push(index);
            }
        }
        if context.root_identity(&root)? != identity
            || endpoint.remote_batch_parents(cancel)? != parents
        {
            return Err(drift(
                "Remote-Eltern oder Sitzung änderten sich während der Vorbereitung",
            ));
        }
        let (class, mut limitations) = if endpoint.kind() == ProviderKind::WebDav {
            let crate::RenameInteractionContract::Confirmed {
                execution_class: class,
                limitations,
            } = crate::ordinary_rename_contract(ProviderKind::WebDav)
            else {
                unreachable!()
            };
            (class, limitations)
        } else {
            let capabilities = endpoint.probe_rename_capabilities()?;
            let class = match endpoint.kind() {
                ProviderKind::Sftp | ProviderKind::Ftps => RenameExecutionClass::BestEffort,
                ProviderKind::Smb => RenameExecutionClass::Assured,
                _ => unreachable!("Provider wurde vor Zugriff validiert"),
            };
            if capabilities.execution_class() != Some(class) {
                return Err(drift("Remote-Renameklasse änderte sich"));
            }
            (class, capabilities.limitations)
        };
        if spelling.iter().any(Option::is_some) {
            limitations.push(SMB_SPELLING_LIMITATION.into());
        }
        if sftp_spelling.iter().any(|candidate| *candidate) {
            limitations.push(SFTP_SPELLING_LIMITATION.into());
        }
        if ftp_spelling.iter().any(|candidate| *candidate) {
            limitations.push(FTP_SPELLING_LIMITATION.into());
        }
        Ok(Self {
            endpoint: endpoint.clone(),
            root,
            identity,
            parents,
            proofs,
            class,
            rows,
            limitations,
            execution_order,
            dependencies,
            spelling,
            sftp_spelling,
            ftp_spelling,
            cycles,
            created: Instant::now(),
        })
    }
    pub fn rows(&self) -> &[BatchRenameRow] {
        &self.rows
    }
    pub fn execution_class(&self) -> RenameExecutionClass {
        self.class
    }
    pub fn limitations(&self) -> &[String] {
        &self.limitations
    }
    pub fn is_expired(&self) -> bool {
        self.created.elapsed() > PLAN_LIFETIME
    }
    /// `before_each` prüft die ursprünglichen App-Profilbindungen vor jeder Einzelwirkung.
    pub fn execute(
        self,
        cancel: &Cancel,
        before_each: &mut dyn FnMut() -> io::Result<()>,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> BatchRenameReport {
        self.execute_with_queue(cancel, before_each, on_progress, None)
    }
    pub fn execute_with_queue(
        mut self,
        cancel: &Cancel,
        before_each: &mut dyn FnMut() -> io::Result<()>,
        on_progress: &mut dyn FnMut(usize, usize),
        queue: Option<&crate::OperationQueue>,
    ) -> BatchRenameReport {
        let cycles = std::mem::take(&mut self.cycles);
        let mut cycle_by_row = vec![None; self.rows.len()];
        for (index, cycle) in cycles.iter().enumerate() {
            for &member in &cycle.members {
                cycle_by_row[member] = Some(index);
            }
        }
        let mut stop: Option<String> = None;
        if self.is_expired() {
            stop = Some("Vorschau ist älter als zehn Minuten; erneut vorbereiten".into());
        }
        let smb = self.endpoint.kind() == ProviderKind::Smb;
        let total = self.execution_order.len();
        let execution_order = self.execution_order.clone();
        for (completed, &index) in execution_order.iter().enumerate() {
            if self.rows[index].status != BatchRenameStatus::Ready {
                continue;
            }
            if cancel.is_cancelled() {
                set_status(
                    &mut self.rows[index],
                    BatchRenameStatus::Cancelled,
                    "Vor dieser Namensänderung abgebrochen; unverändert",
                );
                continue;
            }
            if let Some(reason) = &stop {
                set_status(
                    &mut self.rows[index],
                    BatchRenameStatus::FailedUnchanged,
                    format!("Nicht begonnen: {reason}"),
                );
                continue;
            }
            if let Some(cycle) = cycle_by_row[index] {
                if let Some(reason) = self.execute_cycle(&cycles[cycle], cancel, before_each, queue)
                {
                    stop = Some(reason);
                }
                on_progress(completed + cycles[cycle].members.len(), total);
                continue;
            }
            if dependency_failed(&self.rows, &self.dependencies[index]) {
                set_status(&mut self.rows[index], BatchRenameStatus::FailedUnchanged, "Nicht ausgeführt: Eine vorausgehende Umbenennung hat den Zielnamen nicht nachweislich freigemacht");
                on_progress(completed + 1, total);
                continue;
            }
            let old = self.root.join(&self.rows[index].old_name);
            // Eine begonnene Einzelaktion erhält einen eigenen Token und wird
            // vollständig bilanziert; Benutzerabbruch stoppt die nächste Zeile.
            let attempt = Cancel::new();
            let preflight = (|| {
                before_each()?;
                let context = self.endpoint.remote_batch_context(&attempt)?;
                if context.root_identity(&self.root)? != self.identity
                    || self.endpoint.remote_batch_parents(&attempt)? != self.parents
                {
                    return Err(drift(
                        "Remote-Sitzung, Root oder Eltern änderten sich seit der Vorschau",
                    ));
                }
                Ok(context)
            })();
            let context = match preflight {
                Ok(context) => context,
                Err(error) => {
                    stop = Some(error.to_string());
                    set_status(
                        &mut self.rows[index],
                        BatchRenameStatus::FailedUnchanged,
                        error.to_string(),
                    );
                    self.rows[index].code = Some(crate::error_code(&error));
                    on_progress(completed + 1, total);
                    continue;
                }
            };
            // SMB bindet Quellen einzeln samt ChangeTime. Ein Fehler vor der
            // Mutation lässt nur diese Quelle und ihre Abhängigen unverändert;
            // jede spätere unabhängige Zeile durchläuft erneut die Bindungsprüfung.
            let source_check = (|| {
                let (_, fresh) = self.endpoint.remote_batch_source(&old, &attempt)?;
                if self.proofs[index].as_ref().map(|proof| proof.1) != Some(fresh) {
                    return Err(drift(
                        "Remote-Quellattribute änderten sich seit der Vorschau",
                    ));
                }
                Ok(())
            })();
            if let Err(error) = source_check {
                if !smb {
                    stop = Some(error.to_string());
                }
                set_status(
                    &mut self.rows[index],
                    BatchRenameStatus::FailedUnchanged,
                    error.to_string(),
                );
                self.rows[index].code = Some(crate::error_code(&error));
                on_progress(completed + 1, total);
                continue;
            }
            if cancel.is_cancelled() {
                set_status(
                    &mut self.rows[index],
                    BatchRenameStatus::Cancelled,
                    "Vor der Namensänderung abgebrochen; unverändert",
                );
                continue;
            }
            if let Some(token) = &self.spelling[index] {
                let row = &self.rows[index];
                let result = execute_smb_spelling(
                    context.as_ref(),
                    SmbSpellingProof {
                        root: &self.root,
                        identity: &self.identity,
                        source: &self.proofs[index].as_ref().expect("Bereite Quelle").0,
                        token,
                    },
                    cancel,
                    row,
                    &mut || {
                        self.endpoint.execute_smb_batch_spelling(
                            &old,
                            &row.new_name,
                            &self.identity,
                            self.proofs[index].as_ref().expect("Bereite Quelle").1,
                        )
                    },
                );
                match result {
                    Ok(()) => set_status(
                        &mut self.rows[index],
                        BatchRenameStatus::Renamed,
                        "Zielschreibweise und SMB-File-ID bestätigt; Best Effort",
                    ),
                    Err(error) => {
                        let code = crate::error_code(&error);
                        set_status(
                            &mut self.rows[index],
                            if code == ErrorCode::OutcomeUnclear {
                                BatchRenameStatus::OutcomeUnclear
                            } else if code == ErrorCode::Interrupted {
                                BatchRenameStatus::Cancelled
                            } else {
                                BatchRenameStatus::FailedUnchanged
                            },
                            error.to_string(),
                        );
                        self.rows[index].code = Some(code);
                    }
                }
                on_progress(completed + 1, total);
                continue;
            }
            if self.ftp_spelling[index] {
                let fresh = (|| {
                    let listing = context.list_complete(&self.root)?;
                    let item = ftp_spelling_member(&listing, &self.rows[index].old_name)?;
                    if !same_ftp_source_facts(
                        &self.proofs[index].as_ref().expect("Bereite Quelle").0,
                        &item.entry,
                    ) || context.root_identity(&self.root)? != self.identity
                    {
                        return Err(drift("FTP-Quelle, Root oder Sitzung änderten sich vor der Schreibweisenänderung"));
                    }
                    check_cancel(cancel)
                })();
                if let Err(error) = fresh {
                    let code = crate::error_code(&error);
                    stop = Some(error.to_string());
                    set_status(
                        &mut self.rows[index],
                        if code == ErrorCode::Interrupted {
                            BatchRenameStatus::Cancelled
                        } else {
                            BatchRenameStatus::FailedUnchanged
                        },
                        error.to_string(),
                    );
                    self.rows[index].code = Some(code);
                    on_progress(completed + 1, total);
                    continue;
                }
            }
            let result = if self.endpoint.kind() == ProviderKind::WebDav {
                self.endpoint
                    .execute_webdav_batch_rename(
                        &old,
                        &self.rows[index].new_name,
                        &self.identity,
                        self.parents.as_ref().expect("WebDAV-Eltern vorbereitet"),
                        self.proofs[index].as_ref().expect("Bereite Quelle").1,
                    )
                    .map(|(report, refresh)| (report, Some(refresh)))
            } else {
                self.endpoint
                    .execute_remote_batch_rename(
                        context.as_ref(),
                        &old,
                        &self.rows[index].new_name,
                        self.class,
                        self.limitations.clone(),
                    )
                    .map(|report| (report, None))
            };
            match result {
                Ok((report, _)) if report.outcome == RenameOutcome::Collision => {
                    set_status(
                        &mut self.rows[index],
                        BatchRenameStatus::FailedUnchanged,
                        "Zielname ist inzwischen belegt; unverändert",
                    );
                    self.rows[index].code = Some(ErrorCode::AlreadyExists);
                }
                Ok((_, refreshed)) => {
                    if self.ftp_spelling[index] {
                        let new_path = self.root.join(&self.rows[index].new_name);
                        let readback = verify_ftp_spelling(
                            context.as_ref(),
                            FtpSpellingProof {
                                root: &self.root,
                                identity: &self.identity,
                                parents: self.parents.as_ref().expect("FTP-Eltern vorbereitet"),
                                row: &self.rows[index],
                                source: &self.proofs[index].as_ref().expect("Bereite Quelle").0,
                            },
                            &mut || self.endpoint.remote_batch_parents(&attempt),
                        );
                        match readback {
                            Ok(refresh) => {
                                self.identity = refresh.identity;
                                self.parents = Some(refresh.parents);
                                set_status(&mut self.rows[index], BatchRenameStatus::Renamed,
                                    "Zielschreibweise und beobachtbare FTP-/FTPS-Quellattribute bestätigt; Best Effort");
                            }
                            Err(error) => {
                                let error = crate::error::with_recovery_context(crate::coded_error(
                                    io::ErrorKind::Other, ErrorCode::OutcomeUnclear,
                                    format!("FTP-/FTPS-Rename gemeldet, exakter Abschluss unklar: {error}")),
                                    &[&error], &[&old, &new_path]);
                                stop = Some(error.to_string());
                                set_status(
                                    &mut self.rows[index],
                                    BatchRenameStatus::OutcomeUnclear,
                                    error.to_string(),
                                );
                                self.rows[index].code = Some(ErrorCode::OutcomeUnclear);
                            }
                        }
                        on_progress(completed + 1, total);
                        continue;
                    }
                    if self.sftp_spelling[index] {
                        let new_path = self.root.join(&self.rows[index].new_name);
                        let readback = (|| {
                            let proof = self.proofs[index].as_ref().expect("Bereite Quelle");
                            verify_sftp_spelling(
                                context.as_ref(),
                                &self.root,
                                &self.identity,
                                &self.rows[index],
                                proof,
                                &mut || self.endpoint.remote_batch_source(&new_path, &attempt),
                            )?;
                            let current = self.endpoint.remote_batch_parents(&attempt)?;
                            match (&self.parents, &current) {
                                (Some(old), Some(new)) if old.own_rename_refresh(new) => {},
                                _ => return Err(drift("SFTP-Elternbeleg änderte sich außerhalb der eigenen Namenswirkung")),
                            }
                            if context.root_identity(&self.root)? != self.identity {
                                return Err(drift(
                                    "SFTP-Root oder Sitzung änderte sich beim Abschluss",
                                ));
                            }
                            Ok(current)
                        })();
                        match readback {
                            Ok(current) => {
                                self.parents = current;
                                set_status(&mut self.rows[index], BatchRenameStatus::Renamed,
                                    "Zielschreibweise und beobachtbare SFTP-Quellattribute bestätigt; Best Effort");
                            }
                            Err(error) => {
                                let error = crate::error::with_recovery_context(crate::coded_error(
                                    io::ErrorKind::Other, ErrorCode::OutcomeUnclear,
                                    format!("SFTP-Rename gemeldet, exakter Abschluss unklar: {error}")),
                                    &[&error], &[&old, &new_path]);
                                stop = Some(error.to_string());
                                set_status(
                                    &mut self.rows[index],
                                    BatchRenameStatus::OutcomeUnclear,
                                    error.to_string(),
                                );
                                self.rows[index].code = Some(ErrorCode::OutcomeUnclear);
                            }
                        }
                        on_progress(completed + 1, total);
                        continue;
                    }
                    if self.endpoint.kind() == ProviderKind::Smb {
                        let new_path = self.root.join(&self.rows[index].new_name);
                        let readback = (|| {
                            let (entry, _) =
                                self.endpoint.remote_batch_source(&new_path, &attempt)?;
                            if !self.proofs[index].as_ref().is_some_and(|proof| {
                                proof.0.inode.is_some()
                                    && crate::batch_rename::same_source(&proof.0, &entry)
                            }) {
                                return Err(drift("SMB-Ziel besitzt nach Rename nicht die bestätigte File-ID und Metadaten"));
                            }
                            Ok(())
                        })();
                        if let Err(error) = readback {
                            let error = crate::error::with_recovery_context(crate::coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear, format!("SMB-Renameantwort bestätigt, Zielidentität unklar: {error}")), &[&error], &[&old, &new_path]);
                            set_status(
                                &mut self.rows[index],
                                BatchRenameStatus::OutcomeUnclear,
                                error.to_string(),
                            );
                            self.rows[index].code = Some(ErrorCode::OutcomeUnclear);
                            // Abhängige bleiben gesperrt. Eine unabhängige Zeile
                            // prüft Profil, Root und Sitzung vor ihrem Beginn erneut.
                            on_progress(completed + 1, total);
                            continue;
                        }
                    }
                    set_status(
                        &mut self.rows[index],
                        BatchRenameStatus::Renamed,
                        if self.class == RenameExecutionClass::BestEffort {
                            "Umbenannt; Best-Effort-Readback bestätigt"
                        } else {
                            "Umbenannt; SMB-No-Replace und File-ID-Readback bestätigt"
                        },
                    );
                    if let Some(refreshed) = refreshed {
                        self.identity = refreshed.identity;
                        self.parents = Some(refreshed.parents);
                        on_progress(completed + 1, total);
                        continue;
                    }
                    let refresh = (|| {
                        if context.root_identity(&self.root)? != self.identity {
                            return Err(drift(
                                "Remote-Sitzung änderte sich nach der Namensänderung",
                            ));
                        }
                        let current = self.endpoint.remote_batch_parents(&attempt)?;
                        match (&self.parents, &current) {
                            (Some(old), Some(new)) if old.own_rename_refresh(new) => Ok(current),
                            (None, None) => Ok(current),
                            _ => Err(drift("Remote-Elternbeleg änderte sich außerhalb der eigenen Namenswirkung")),
                        }
                    })();
                    match refresh {
                        Ok(current) => self.parents = current,
                        Err(error) => stop = Some(error.to_string()),
                    }
                }
                Err(error) => {
                    let code = crate::error_code(&error);
                    let unclear = code == ErrorCode::OutcomeUnclear;
                    set_status(
                        &mut self.rows[index],
                        if unclear {
                            BatchRenameStatus::OutcomeUnclear
                        } else {
                            BatchRenameStatus::FailedUnchanged
                        },
                        error.to_string(),
                    );
                    self.rows[index].code = Some(code);
                    if !smb
                        && (unclear
                            || matches!(code, ErrorCode::PlanDrift | ErrorCode::ConnectionLost))
                    {
                        stop = Some(error.to_string());
                    }
                }
            }
            on_progress(completed + 1, total);
        }
        finish_report(self.rows, Some(self.class), self.limitations)
    }
}

const FTP_SPELLING_PREVIEW: &str = "FTP-/FTPS-Schreibweise über gewöhnliches Best-Effort-Rename ändern, sofern der Server sie zulässt";
const FTP_SPELLING_LIMITATION: &str = "FTP-/FTPS-Schreibweisenänderungen verwenden den gewöhnlichen Renameweg mit höchstens einem RNFR/RNTO-Paar. Erfolg verlangt den exakten neuen Rohnamen im vollständigen MLSD-Bestand und passende beobachtbare Quellattribute. Der Server kann die Schreibweise verweigern. FTP liefert hier keine stabile Objekt-ID oder atomare Quell-/Elternbindung; zwischen frischem Zielcheck und RNTO bleibt ein Kollisionsfenster. Kein Tempname, Wiederholversuch oder zusätzlicher überschreibender Ersatzweg.";

fn ftp_spelling_member<'a>(
    listing: &'a crate::vfs::CompleteDirectoryListing,
    name: &str,
) -> io::Result<&'a crate::vfs::ProofListEntry> {
    crate::fileops::require_complete_safe_listing(listing, "FTP-Schreibweisenbeleg")?;
    let folded = crate::remote_rename::folded_name(name);
    let mut group = listing
        .entries
        .iter()
        .filter(|item| crate::remote_rename::folded_name(&item.entry.name) == folded);
    let item = group.next().ok_or_else(|| drift("FTP-Rohname fehlt"))?;
    if group.next().is_some()
        || item.raw_name != name.as_bytes()
        || item.entry.name != name
        || !item.safely_decoded
        || item.entry.is_symlink
        || (!item.entry.is_file && !item.entry.is_dir)
    {
        return Err(drift(
            "FTP-Namensgruppe ist nicht exakt und eindeutig belegt",
        ));
    }
    Ok(item)
}
fn ftp_spelling_candidates(
    kind: ProviderKind,
    listing: &crate::vfs::CompleteDirectoryListing,
    rows: &mut [BatchRenameRow],
) -> Vec<bool> {
    rows.iter_mut()
        .map(|row| {
            if kind != ProviderKind::Ftps
                || row.status != BatchRenameStatus::Ready
                || row.old_name == row.new_name
                || crate::remote_rename::folded_name(&row.old_name)
                    != crate::remote_rename::folded_name(&row.new_name)
            {
                return false;
            }
            match ftp_spelling_member(listing, &row.old_name) {
                Ok(_) => true,
                Err(error) => {
                    set_status(row, BatchRenameStatus::Blocked, error.to_string());
                    false
                }
            }
        })
        .collect()
}
fn same_ftp_source_facts(left: &crate::Entry, right: &crate::Entry) -> bool {
    // FTPs bisheriger Quelldigest enthält Name und Endung. Nach Rename werden
    // deshalb ausschließlich die tatsächlich angebotenen namensfreien Fakten
    // verglichen; daraus entsteht weder eine stabile Objekt-ID noch ein unique-Beleg.
    left.is_dir == right.is_dir
        && left.is_file == right.is_file
        && left.is_symlink == right.is_symlink
        && left.size == right.size
        && left.mtime == right.mtime
        && left.created == right.created
        && left.owner == right.owner
        && left.group == right.group
        && left.mode == right.mode
        && left.device == right.device
        && left.inode == right.inode
        && left.hard_links == right.hard_links
}
struct FtpSpellingProof<'a> {
    root: &'a std::path::Path,
    identity: &'a crate::RootIdentity,
    parents: &'a ParentProof,
    row: &'a BatchRenameRow,
    source: &'a crate::Entry,
}
fn verify_ftp_spelling(
    context: &dyn crate::Vfs,
    proof: FtpSpellingProof<'_>,
    read_parents: &mut dyn FnMut() -> io::Result<Option<ParentProof>>,
) -> io::Result<RemoteBatchRefresh> {
    // Nur ein bereits gewöhnlich bestätigter Erfolg erreicht diesen Abschluss.
    // Vorherige unklare Antworten werden niemals durch passende Namen geheilt.
    let listing = context.list_complete(proof.root)?;
    let item = ftp_spelling_member(&listing, &proof.row.new_name)?;
    if !same_ftp_source_facts(proof.source, &item.entry) {
        return Err(drift(
            "FTP-Abschluss besitzt nicht die erwarteten Quellattribute",
        ));
    }
    let parents = read_parents()?.ok_or_else(|| drift("FTP-Elternbeleg fehlt beim Abschluss"))?;
    if !proof.parents.own_rename_refresh(&parents) {
        return Err(drift(
            "FTP-Elternbeleg änderte sich außerhalb der eigenen Namenswirkung",
        ));
    }
    let identity = context.root_identity(proof.root)?;
    if identity != *proof.identity {
        return Err(drift("FTP-Root oder Sitzung änderte sich beim Abschluss"));
    }
    // Der bestätigte eigene Rename kann Pane-Größe/Zeit ändern. Den neuen Beleg
    // vollständig übernehmen, statt ihn nochmals mit den alten Vollwerten zu prüfen.
    Ok(RemoteBatchRefresh { identity, parents })
}

fn webdav_spelling_candidates(
    kind: ProviderKind,
    listing: &crate::vfs::CompleteDirectoryListing,
    rows: &[BatchRenameRow],
) -> Vec<bool> {
    rows.iter()
        .map(|row| {
            if kind != ProviderKind::WebDav
                || !listing.complete
                || row.status != BatchRenameStatus::Ready
                || row.old_name == row.new_name
                || crate::remote_rename::folded_name(&row.old_name)
                    != crate::remote_rename::folded_name(&row.new_name)
            {
                return false;
            }
            let folded = crate::remote_rename::folded_name(&row.old_name);
            let mut group = listing
                .entries
                .iter()
                .filter(|item| crate::remote_rename::folded_name(&item.entry.name) == folded);
            group.next().is_some_and(|item| {
                item.entry.name == row.old_name
                    && item.raw_name == row.old_name.as_bytes()
                    && item.safely_decoded
                    && !item.entry.is_symlink
                    && (item.entry.is_file || item.entry.is_dir)
                    && group.next().is_none()
            })
        })
        .collect()
}

const SFTP_SPELLING_PREVIEW: &str =
    "SFTP-Schreibweise über gewöhnliches Best-Effort-Rename ändern, sofern der Server sie zulässt";
const SFTP_SPELLING_LIMITATION: &str = "SFTP-Schreibweisenänderungen verwenden den gewöhnlichen Renameweg mit höchstens einem Standard-RENAME. Ein bereits aufgelöster Zielalias kann die Aktion vor dem Request blockieren; der Server darf sie ablehnen. Erfolg verlangt den exakten neuen Rohnamen und passende beobachtbare Quellattribute. SFTP v3 liefert keine stabile Objekt-ID oder atomare Quell-/Elternbindung. Kein Tempname, Wiederholversuch oder überschreibender Ersatzweg.";

fn sftp_spelling_member<'a>(
    listing: &'a crate::vfs::CompleteDirectoryListing,
    name: &str,
) -> io::Result<&'a crate::vfs::ProofListEntry> {
    crate::fileops::require_complete_safe_listing(listing, "SFTP-Schreibweisenbeleg")?;
    let folded = crate::remote_rename::folded_name(name);
    let mut group = listing
        .entries
        .iter()
        .filter(|item| crate::remote_rename::folded_name(&item.entry.name) == folded);
    let item = group.next().ok_or_else(|| drift("SFTP-Rohname fehlt"))?;
    // Auch gleiche Attribute machen zwei echte Namen nicht zu einer Quelle.
    // Andere Hardlinks außerhalb dieser Gruppe sind hierfür unerheblich.
    if group.next().is_some()
        || item.raw_name != name.as_bytes()
        || item.entry.name != name
        || !item.safely_decoded
        || item.entry.is_symlink
        || (!item.entry.is_file && !item.entry.is_dir)
    {
        return Err(drift(
            "SFTP-Namensgruppe ist nicht exakt und eindeutig belegt",
        ));
    }
    Ok(item)
}

fn sftp_spelling_candidates(
    kind: ProviderKind,
    listing: &crate::vfs::CompleteDirectoryListing,
    rows: &mut [BatchRenameRow],
) -> Vec<bool> {
    rows.iter_mut()
        .map(|row| {
            if kind != ProviderKind::Sftp
                || row.status != BatchRenameStatus::Ready
                || row.old_name == row.new_name
                || crate::remote_rename::folded_name(&row.old_name)
                    != crate::remote_rename::folded_name(&row.new_name)
            {
                return false;
            }
            match sftp_spelling_member(listing, &row.old_name) {
                Ok(_) => true,
                Err(error) => {
                    set_status(row, BatchRenameStatus::Blocked, error.to_string());
                    false
                }
            }
        })
        .collect()
}

fn verify_sftp_spelling(
    context: &dyn crate::Vfs,
    root: &std::path::Path,
    identity: &crate::RootIdentity,
    row: &BatchRenameRow,
    source: &(crate::Entry, [u8; 32]),
    read_source: &mut dyn FnMut() -> io::Result<(crate::Entry, [u8; 32])>,
) -> io::Result<()> {
    // Dieser Abschluss wird ausschließlich nach gewöhnlich bestätigtem Erfolg
    // aufgerufen. Ein vorheriger unklarer Ausgang darf dadurch nie geheilt werden.
    let listing = context.list_complete(root)?;
    let item = sftp_spelling_member(&listing, &row.new_name)?;
    let (entry, digest) = read_source()?;
    if !crate::batch_rename::same_source(&source.0, &item.entry)
        || !crate::batch_rename::same_source(&source.0, &entry)
        || source.1 != digest
        || context.root_identity(root)? != *identity
    {
        return Err(drift(
            "SFTP-Abschluss besitzt nicht die erwarteten Quellattribute, Root oder Sitzung",
        ));
    }
    Ok(())
}

const SMB_SPELLING_PREVIEW: &str =
    "SMB-Schreibweise mit einem No-Replace-Versuch ändern, sofern der Server sie zulässt";
const SMB_SPELLING_LIMITATION: &str = "SMB-Schreibweisenänderungen laufen als Best Effort: genau ein serverseitiger No-Replace-Versuch und vollständiger Rohname-/File-ID-Readback. Der Server kann die Schreibweise verweigern. Das Öffnen der Quelle bleibt pfadbasiert; keine atomare Quell- oder Elternbindung und keine lückenlose Namespacehistorie. Kein Tempname, Wiederholversuch oder automatischer Rücktausch. Pro versuchter Schreibweisenzeile werden zwei vollständige Elternlistings gelesen; ohne Directory-FLUSH ist keine dauerhafte Speicherung zugesichert.";

fn spelling_member<'a>(
    listing: &'a crate::vfs::CompleteDirectoryListing,
    name: &str,
) -> io::Result<&'a crate::vfs::ProofListEntry> {
    crate::fileops::require_complete_safe_listing(listing, "SMB-Schreibweisenbeleg")?;
    let folded = crate::remote_rename::folded_name(name);
    let mut group = listing
        .entries
        .iter()
        .filter(|item| crate::remote_rename::folded_name(&item.entry.name) == folded);
    let item = group.next().ok_or_else(|| drift("SMB-Rohname fehlt"))?;
    if group.next().is_some()
        || item.raw_name != name.as_bytes()
        || item.entry.name != name
        || !item.safely_decoded
        || item.entry.inode.is_none_or(|id| id == 0)
        || !item.provider_token.as_ref().is_some_and(|token| {
            token.len() == 16 && token[..8] == item.entry.inode.unwrap_or(0).to_le_bytes()
        })
        || item.entry.is_symlink
        || (!item.entry.is_file && !item.entry.is_dir)
    {
        return Err(drift(
            "SMB-Namensgruppe ist nicht exakt und eindeutig mit File-ID belegt",
        ));
    }
    Ok(item)
}

fn smb_spelling_candidates(
    kind: ProviderKind,
    listing: &crate::vfs::CompleteDirectoryListing,
    rows: &mut [BatchRenameRow],
) -> Vec<Option<Vec<u8>>> {
    rows.iter_mut()
        .map(|row| {
            if kind != ProviderKind::Smb
                || row.status != BatchRenameStatus::Ready
                || row.old_name == row.new_name
                || crate::remote_rename::folded_name(&row.old_name)
                    != crate::remote_rename::folded_name(&row.new_name)
            {
                return None;
            }
            match spelling_member(listing, &row.old_name).and_then(|item| {
                item.provider_token
                    .clone()
                    .filter(|token| {
                        token.len() == 16 && token[..8] == item.entry.inode.unwrap().to_le_bytes()
                    })
                    .ok_or_else(|| drift("SMB-Quellversion fehlt"))
            }) {
                Ok(token) => Some(token),
                Err(error) => {
                    set_status(row, BatchRenameStatus::Blocked, error.to_string());
                    None
                }
            }
        })
        .collect()
}

struct SmbSpellingProof<'a> {
    root: &'a std::path::Path,
    identity: &'a crate::RootIdentity,
    source: &'a crate::Entry,
    token: &'a [u8],
}

fn execute_smb_spelling(
    context: &dyn crate::Vfs,
    proof: SmbSpellingProof<'_>,
    cancel: &Cancel,
    row: &BatchRenameRow,
    mutate: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let SmbSpellingProof {
        root,
        identity,
        source,
        token,
    } = proof;
    let listing = context.list_complete(root)?;
    let old = spelling_member(&listing, &row.old_name)?;
    if !crate::batch_rename::same_source(source, &old.entry)
        || old.provider_token.as_deref() != Some(token)
        || context.root_identity(root)? != *identity
    {
        return Err(drift(
            "SMB-Quelle, Root oder Sitzung änderte sich vor der Wirkung",
        ));
    }
    let to = root.join(&row.new_name);
    let unclear = |message: String| {
        crate::coded_error(
            io::ErrorKind::Other,
            ErrorCode::OutcomeUnclear,
            format!(
                "{message}; Namen „{}“ und „{}“ prüfen; kein automatischer Wiederholversuch",
                row.old_name, row.new_name
            ),
        )
    };
    // Nur diese Primitive liefert einen bestätigten SET_INFO-Ack. Ein Fehler nach
    // möglicher Wirkung bleibt endgültig unklar, auch bei später passendem Listing.
    check_cancel(cancel)?;
    let mutation = mutate();
    if mutation
        .as_ref()
        .is_err_and(|error| crate::error_code(error) == ErrorCode::OutcomeUnclear)
    {
        return Err(unclear(format!(
            "SMB-Mutationsausgang unklar: {}",
            mutation.unwrap_err()
        )));
    }
    if mutation.is_ok() {
        let current = context
            .stat(&to)
            .map_err(|error| unclear(format!("SMB-Zielreadback fehlgeschlagen: {error}")))?;
        if !crate::batch_rename::same_source(source, &current) {
            return Err(unclear("SMB-Zielidentität stimmt nicht überein".into()));
        }
        context
            .sync_dir(root)
            .map_err(|error| unclear(format!("SMB-Elternsync fehlgeschlagen: {error}")))?;
    }
    let final_state = (|| {
        let listing = context.list_complete(root)?;
        if context.root_identity(root)? != *identity {
            return Err(drift("SMB-Root oder Sitzung änderte sich nach dem Versuch"));
        }
        let expected_name = if mutation.is_ok() {
            &row.new_name
        } else {
            &row.old_name
        };
        let item = spelling_member(&listing, expected_name)?;
        if !crate::batch_rename::same_source(source, &item.entry)
            || (mutation.is_err() && item.provider_token.as_deref() != Some(token))
        {
            return Err(drift(
                "SMB-Abschluss besitzt nicht die erwartete Identität und Quellversion",
            ));
        }
        Ok(())
    })();
    final_state.map_err(|error| unclear(format!("SMB-Abschlussbeleg unklar: {error}")))?;
    mutation
}

// Ausschließlich ein bestätigter Rename gibt den ursprünglichen Namen frei.
fn dependency_failed(rows: &[BatchRenameRow], dependencies: &[usize]) -> bool {
    dependencies
        .iter()
        .any(|&source| rows[source].status != BatchRenameStatus::Renamed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vfs;

    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SpellingFs {
        local: crate::LocalVfs,
        fault: &'static str,
        listings: AtomicUsize,
        roots: AtomicUsize,
    }
    fn versioned(
        mut listing: crate::vfs::CompleteDirectoryListing,
    ) -> crate::vfs::CompleteDirectoryListing {
        for item in &mut listing.entries {
            let mut token = item.entry.inode.unwrap_or(0).to_le_bytes().to_vec();
            token.extend_from_slice(&7_u64.to_le_bytes());
            item.provider_token = Some(token);
        }
        listing
    }
    impl Vfs for SpellingFs {
        fn list(&self, p: &Path) -> io::Result<Vec<crate::Entry>> {
            self.local.list(p)
        }
        fn list_complete(&self, p: &Path) -> io::Result<crate::vfs::CompleteDirectoryListing> {
            let call = self.listings.fetch_add(1, Ordering::SeqCst);
            if call == 1 && self.fault == "scan_error" {
                return Err(io::Error::other("Scanfehler"));
            }
            let mut listing = versioned(self.local.list_complete(p)?);
            let source_index = listing
                .entries
                .iter()
                .position(|item| item.entry.name.eq_ignore_ascii_case("Foo"))
                .unwrap();
            if call == 0 {
                match self.fault {
                    "pre_alias" => {
                        let mut item = listing.entries[source_index].clone();
                        item.raw_name = b"FOO".to_vec();
                        item.entry.name = "FOO".into();
                        listing.entries.push(item);
                    }
                    "pre_source" => listing.entries[source_index].entry.size += 1,
                    "pre_version" => {
                        listing.entries[source_index]
                            .provider_token
                            .as_mut()
                            .unwrap()[8] += 1
                    }
                    "pre_raw" => listing.entries[source_index].raw_name = b"FOO".to_vec(),
                    _ => {}
                }
            }
            if call == 1 {
                match self.fault {
                    "ftp_owner" => {
                        listing.entries[source_index].entry.owner = Some("changed-owner".into())
                    }
                    "ftp_group" => {
                        listing.entries[source_index].entry.group = Some("changed-group".into())
                    }
                    "ftp_created" => listing.entries[source_index].entry.created = Some(1),
                    "ftp_mode" => listing.entries[source_index].entry.mode = Some(0),
                    "ftp_size" => listing.entries[source_index].entry.size += 1,
                    "ftp_mtime" => listing.entries[source_index].entry.mtime = Some(1),
                    "incomplete" => listing.complete = false,
                    "late_alias" => {
                        let mut item = listing.entries[source_index].clone();
                        item.raw_name = b"FOO".to_vec();
                        item.entry.name = "FOO".into();
                        listing.entries.push(item);
                    }
                    "wrong_raw" => listing.entries[source_index].raw_name = b"FOO".to_vec(),
                    "version_changed" | "refused_version" => {
                        listing.entries[source_index]
                            .provider_token
                            .as_mut()
                            .unwrap()[8] += 1
                    }
                    _ => {}
                }
            }
            Ok(listing)
        }
        fn root_identity(&self, p: &Path) -> io::Result<crate::RootIdentity> {
            let call = self.roots.fetch_add(1, Ordering::SeqCst);
            if (call == 1 && self.fault == "session") || (call == 0 && self.fault == "pre_root") {
                return Err(drift("Sitzungswechsel"));
            }
            self.local.root_identity(p)
        }
        fn stat(&self, p: &Path) -> io::Result<crate::Entry> {
            if self.fault == "readback" {
                return Err(io::Error::other("Readbackfehler"));
            }
            if p.file_name().is_some_and(|name| name == "foo") {
                match self.fault {
                    "noop_alias" => return self.local.stat(&p.with_file_name("Foo")),
                    "noop_not_found" => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            "Ziel fehlt nach No-op",
                        ))
                    }
                    _ => {}
                }
            }
            let mut entry = self.local.stat(p)?;
            if self.fault == "identity" {
                entry.inode = Some(0);
            }
            Ok(entry)
        }
        fn sync_dir(&self, p: &Path) -> io::Result<()> {
            if self.fault == "sync" {
                return Err(io::Error::other("Syncfehler"));
            }
            if self.fault == "foreign_revert" {
                self.local
                    .rename_noreplace(&p.join("foo"), &p.join("Foo"))?;
            }
            self.local.sync_dir(p)
        }
        fn exists(&self, p: &Path) -> bool {
            self.local.exists(p)
        }
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            self.local.create_dir_all(p)
        }
        fn create_dir(&self, p: &Path) -> io::Result<()> {
            self.local.create_dir(p)
        }
        fn open_read(&self, p: &Path) -> io::Result<Box<dyn std::io::Read + Send>> {
            self.local.open_read(p)
        }
        fn open_write(&self, p: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.local.open_write(p)
        }
        fn open_write_new(&self, p: &Path) -> io::Result<Box<dyn std::io::Write + Send>> {
            self.local.open_write_new(p)
        }
        fn sync_file(&self, p: &Path) -> io::Result<()> {
            self.local.sync_file(p)
        }
        fn set_mtime(&self, p: &Path, t: i64) -> io::Result<()> {
            self.local.set_mtime(p, t)
        }
        fn set_mode(&self, p: &Path, m: u32) -> io::Result<()> {
            self.local.set_mode(p, m)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            self.local.remove_file(p)
        }
        fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
            self.local.remove_dir_all(p)
        }
        fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
            self.local.rename(a, b)
        }
        fn rename_noreplace(&self, a: &Path, b: &Path) -> io::Result<()> {
            self.local.rename_noreplace(a, b)
        }
    }
    fn spelling_options(old: &str, new: &str) -> BatchRenameOptions {
        BatchRenameOptions {
            find: old.into(),
            replacement: new.into(),
            prefix: String::new(),
            suffix: String::new(),
            preserve_extension: false,
            trim_outer_spaces: false,
            numbering: None,
        }
    }
    #[test]
    fn webdav_schreibweise_braucht_urspruenglich_bereite_eindeutige_rohnamen() {
        let area = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(area.path()).unwrap();
        std::fs::write(root.join("Foo"), b"original").unwrap();
        let listing = crate::LocalVfs::rooted(&root)
            .unwrap()
            .list_complete(&root)
            .unwrap();
        for fault in [
            "valid",
            "alias",
            "raw",
            "unsafe",
            "incomplete",
            "symlink",
            "special",
            "preblocked",
            "unchanged",
        ] {
            let mut current = listing.clone();
            let (mut rows, _) = plan_rows_with_chains(
                &listing,
                &["Foo".into()],
                &spelling_options("Foo", "foo"),
                true,
            );
            match fault {
                "alias" => {
                    let mut alias = current.entries[0].clone();
                    alias.entry.name = "FOO".into();
                    alias.raw_name = b"FOO".to_vec();
                    current.entries.push(alias);
                }
                "raw" => current.entries[0].raw_name = b"FOO".to_vec(),
                "unsafe" => current.entries[0].safely_decoded = false,
                "incomplete" => current.complete = false,
                "symlink" => current.entries[0].entry.is_symlink = true,
                "special" => {
                    current.entries[0].entry.is_file = false;
                    current.entries[0].entry.is_dir = false;
                }
                "preblocked" => rows[0].status = BatchRenameStatus::Blocked,
                "unchanged" => rows[0].new_name = rows[0].old_name.clone(),
                _ => {}
            }
            for kind in [
                ProviderKind::WebDav,
                ProviderKind::Sftp,
                ProviderKind::Ftps,
                ProviderKind::Smb,
            ] {
                assert_eq!(
                    webdav_spelling_candidates(kind, &current, &rows),
                    [kind == ProviderKind::WebDav && fault == "valid"],
                    "{kind:?}/{fault}"
                );
            }
        }
    }
    #[test]
    fn sftp_schreibweise_vollstaendige_exakte_gruppe_ohne_objektid() {
        let area = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(area.path()).unwrap();
        std::fs::write(root.join("Foo"), b"original").unwrap();
        std::fs::hard_link(root.join("Foo"), root.join("unrelated")).unwrap();
        let mut listing = crate::LocalVfs::rooted(&root)
            .unwrap()
            .list_complete(&root)
            .unwrap();
        for item in &mut listing.entries {
            item.entry.device = None;
            item.entry.inode = None;
            item.provider_token = None;
        }
        for fault in [
            "valid",
            "alias_same_attrs",
            "alias_other_attrs",
            "raw",
            "unsafe",
            "incomplete",
            "symlink",
            "special",
        ] {
            let mut current = listing.clone();
            let source = current
                .entries
                .iter_mut()
                .find(|item| item.entry.name == "Foo")
                .unwrap();
            match fault {
                "alias_same_attrs" | "alias_other_attrs" => {
                    let mut alias = source.clone();
                    alias.entry.name = "FOO".into();
                    alias.raw_name = b"FOO".to_vec();
                    if fault == "alias_other_attrs" {
                        alias.entry.size += 1;
                    }
                    current.entries.push(alias);
                }
                "raw" => source.raw_name = b"FOO".to_vec(),
                "unsafe" => source.safely_decoded = false,
                "incomplete" => current.complete = false,
                "symlink" => source.entry.is_symlink = true,
                "special" => {
                    source.entry.is_file = false;
                    source.entry.is_dir = false;
                }
                _ => {}
            }
            for kind in [
                ProviderKind::Sftp,
                ProviderKind::Smb,
                ProviderKind::Ftps,
                ProviderKind::WebDav,
            ] {
                let (mut rows, _) = plan_rows_with_chains(
                    &current,
                    &["Foo".into()],
                    &spelling_options("Foo", "foo"),
                    true,
                );
                let candidates = sftp_spelling_candidates(kind, &current, &mut rows);
                assert_eq!(
                    candidates,
                    [kind == ProviderKind::Sftp && fault == "valid"],
                    "{kind:?}/{fault}"
                );
            }
        }
        // Mehrere äquivalente neue Ziele und unveränderte Quellen bleiben gesperrt.
        for (names, options) in [
            (vec!["Foo".into()], spelling_options("Foo", "Foo")),
            (
                vec!["Foo".into(), "unrelated".into()],
                spelling_options("unrelated", "foo"),
            ),
        ] {
            let (mut rows, _) = plan_rows_with_chains(&listing, &names, &options, true);
            assert!(
                sftp_spelling_candidates(ProviderKind::Sftp, &listing, &mut rows)
                    .iter()
                    .all(|candidate| !candidate)
            );
        }
    }

    #[test]
    fn sftp_schreibweise_abschluss_verlangt_rohnamen_attribute_und_sitzung() {
        let faults = [
            "success",
            "noop",
            "scan_error",
            "incomplete",
            "late_alias",
            "wrong_raw",
            "session",
            "source",
            "digest",
            "readback",
        ];
        for fault in faults {
            let area = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(area.path()).unwrap();
            std::fs::write(root.join("Foo"), b"original").unwrap();
            std::fs::hard_link(root.join("Foo"), root.join("unrelated")).unwrap();
            let local = crate::LocalVfs::rooted(&root).unwrap();
            let identity = local.root_identity(&root).unwrap();
            let source = local.stat(&root.join("Foo")).unwrap();
            let listing = local.list_complete(&root).unwrap();
            let (rows, _) = plan_rows_with_chains(
                &listing,
                &["Foo".into()],
                &spelling_options("Foo", "foo"),
                true,
            );
            if fault != "noop" {
                std::fs::rename(root.join("Foo"), root.join("foo")).unwrap();
            }
            let fs = SpellingFs {
                local,
                fault,
                listings: AtomicUsize::new(1),
                roots: AtomicUsize::new(1),
            };
            let result = verify_sftp_spelling(
                &fs,
                &root,
                &identity,
                &rows[0],
                &(source.clone(), [7; 32]),
                &mut || {
                    if fault == "readback" {
                        return Err(io::Error::other("Attributreadbackfehler"));
                    }
                    let mut fresh = source.clone();
                    if fault == "source" {
                        fresh.mode = Some(0);
                    }
                    Ok((fresh, if fault == "digest" { [8; 32] } else { [7; 32] }))
                },
            );
            assert_eq!(result.is_ok(), fault == "success", "{fault}: {result:?}");
            assert_eq!(std::fs::read(root.join("unrelated")).unwrap(), b"original");
        }
    }

    #[test]
    fn smb_schreibweise_ein_versuch_und_sticky_fehler() {
        let faults = [
            "success",
            "noop_alias",
            "noop_not_found",
            "refused",
            "refused_mutated",
            "lost_response",
            "readback",
            "identity",
            "sync",
            "scan_error",
            "incomplete",
            "late_alias",
            "wrong_raw",
            "version_changed",
            "session",
            "foreign_revert",
            "cancel_before",
            "cancel_during",
            "pre_alias",
            "pre_source",
            "pre_version",
            "pre_raw",
            "pre_root",
            "refused_version",
            "close_error",
        ];
        for fault in faults {
            let area = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(area.path()).unwrap();
            std::fs::write(root.join("Foo"), b"original").unwrap();
            std::fs::hard_link(root.join("Foo"), root.join("unrelated")).unwrap();
            let local = crate::LocalVfs::rooted(&root).unwrap();
            let identity = local.root_identity(&root).unwrap();
            let listing = versioned(local.list_complete(&root).unwrap());
            let (rows, _) = plan_rows_with_chains(
                &listing,
                &["Foo".into()],
                &spelling_options("Foo", "foo"),
                true,
            );
            let source = spelling_member(&listing, "Foo").unwrap();
            let fs = SpellingFs {
                local,
                fault,
                listings: AtomicUsize::new(0),
                roots: AtomicUsize::new(0),
            };
            let cancel = Cancel::new();
            if fault == "cancel_before" {
                cancel.cancel();
            }
            let mut calls = 0;
            let result = execute_smb_spelling(
                &fs,
                SmbSpellingProof {
                    root: &root,
                    identity: &identity,
                    source: &source.entry,
                    token: source.provider_token.as_ref().unwrap(),
                },
                &cancel,
                &rows[0],
                &mut || {
                    calls += 1;
                    if matches!(fault, "refused" | "refused_version") {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "Server lehnt ab",
                        ));
                    }
                    if !matches!(fault, "noop_alias" | "noop_not_found") {
                        fs.local
                            .rename_noreplace(&root.join("Foo"), &root.join("foo"))?;
                    }
                    if fault == "refused_mutated" {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "Falsche Ablehnung",
                        ));
                    }
                    if matches!(fault, "lost_response" | "close_error") {
                        return Err(crate::coded_error(
                            io::ErrorKind::Other,
                            ErrorCode::OutcomeUnclear,
                            "Antwort verloren",
                        ));
                    }
                    if fault == "cancel_during" {
                        cancel.cancel();
                    }
                    Ok(())
                },
            );
            let before_effect = fault == "cancel_before" || fault.starts_with("pre_");
            assert_eq!(calls, usize::from(!before_effect), "{fault}");
            match fault {
                "success" | "cancel_during" | "version_changed" => {
                    assert!(result.is_ok(), "{fault}: {result:?}")
                }
                "refused" => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists),
                "cancel_before" => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::Interrupted
                ),
                "pre_raw" => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::InvalidData
                ),
                fault if fault.starts_with("pre_") => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::PlanDrift
                ),
                _ => assert_eq!(
                    crate::error_code(&result.unwrap_err()),
                    ErrorCode::OutcomeUnclear,
                    "{fault}"
                ),
            }
            let expected_listings = if before_effect
                || matches!(
                    fault,
                    "lost_response"
                        | "close_error"
                        | "readback"
                        | "identity"
                        | "sync"
                        | "noop_not_found"
                ) {
                1
            } else {
                2
            };
            assert_eq!(
                fs.listings.load(Ordering::SeqCst),
                expected_listings,
                "{fault}"
            );
            assert_eq!(std::fs::read(root.join("unrelated")).unwrap(), b"original");
            let actual = fs.local.list_complete(&root).unwrap();
            assert_eq!(actual.entries.len(), 2);
            if matches!(fault, "noop_alias" | "noop_not_found") {
                assert_eq!(std::fs::read(root.join("Foo")).unwrap(), b"original");
                assert!(actual.entries.iter().any(|item| item.raw_name == b"Foo"));
                assert!(!actual.entries.iter().any(|item| item.raw_name == b"foo"));
            }
            assert!(actual
                .entries
                .iter()
                .all(|item| item.entry.inode == source.entry.inode));
        }
        eprintln!(
            "SMB-Schreibweisenkoordinator: {} echte Dateisystem-/Fehlerinjektionsfälle",
            faults.len()
        );
    }

    #[test]
    fn smb_schreibweise_gruppen_und_provider_bleiben_begrenzt() {
        let area = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(area.path()).unwrap();
        std::fs::write(root.join("Foo"), b"original").unwrap();
        let initial = versioned(
            crate::LocalVfs::rooted(&root)
                .unwrap()
                .list_complete(&root)
                .unwrap(),
        );
        let mut cases = 0;
        for fault in [
            "valid",
            "alias_same_id",
            "alias_other_id",
            "no_id",
            "zero_id",
            "no_token",
            "zero_token",
            "raw",
            "incomplete",
            "symlink",
            "special",
        ] {
            let mut listing = initial.clone();
            match fault {
                "alias_same_id" | "alias_other_id" => {
                    let mut alias = listing.entries[0].clone();
                    alias.entry.name = "FOO".into();
                    alias.raw_name = b"FOO".to_vec();
                    if fault == "alias_other_id" {
                        alias.entry.inode = Some(1);
                    }
                    listing.entries.push(alias);
                }
                "no_id" => listing.entries[0].entry.inode = None,
                "zero_id" => listing.entries[0].entry.inode = Some(0),
                "no_token" => listing.entries[0].provider_token = None,
                "zero_token" => listing.entries[0].provider_token = Some(vec![0; 16]),
                "raw" => listing.entries[0].raw_name = b"FOO".to_vec(),
                "incomplete" => listing.complete = false,
                "symlink" => listing.entries[0].entry.is_symlink = true,
                "special" => {
                    listing.entries[0].entry.is_file = false;
                    listing.entries[0].entry.is_dir = false;
                }
                _ => {}
            }
            for kind in [
                ProviderKind::Smb,
                ProviderKind::Sftp,
                ProviderKind::Ftps,
                ProviderKind::WebDav,
            ] {
                let (mut rows, _) = plan_rows_with_chains(
                    &listing,
                    &["Foo".into()],
                    &spelling_options("Foo", "foo"),
                    true,
                );
                let candidates = smb_spelling_candidates(kind, &listing, &mut rows);
                assert_eq!(
                    candidates[0].is_some(),
                    kind == ProviderKind::Smb && fault == "valid",
                    "{kind:?}/{fault}"
                );
                cases += 1;
            }
        }
        std::fs::write(root.join("Foo-x"), b"other").unwrap();
        let listing = versioned(
            crate::LocalVfs::rooted(&root)
                .unwrap()
                .list_complete(&root)
                .unwrap(),
        );
        let (mut rows, _) = plan_rows_with_chains(
            &listing,
            &["Foo".into(), "Foo-x".into()],
            &spelling_options("-x", ""),
            true,
        );
        assert!(
            smb_spelling_candidates(ProviderKind::Smb, &listing, &mut rows)
                .iter()
                .all(Option::is_none)
        );
        let (order, _) = chain_execution_plan(&listing, &mut rows);
        assert!(order.is_empty());
        assert_eq!(rows[0].status, BatchRenameStatus::Unchanged);
        assert_eq!(rows[1].status, BatchRenameStatus::Blocked);
        eprintln!("SMB-Schreibweisenplanung: {cases} Gruppen-/Providerfälle");
    }

    #[test]
    fn smb_kette_gibt_nach_unklarem_ausgang_keine_abhaengigen_namen_frei() {
        let area = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(area.path()).unwrap();
        for name in ["a", "aa", "aaa", "b"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let listing = crate::LocalVfs::rooted(&root)
            .unwrap()
            .list_complete(&root)
            .unwrap();
        let names = ["a", "aa", "aaa", "b"].map(str::to_string);
        let options = BatchRenameOptions {
            find: String::new(),
            replacement: String::new(),
            prefix: "a".into(),
            suffix: String::new(),
            preserve_extension: true,
            trim_outer_spaces: false,
            numbering: None,
        };
        let (mut rows, _) = plan_rows_with_chains(&listing, &names, &options, true);
        let (order, dependencies) = chain_execution_plan(&listing, &mut rows);
        assert_eq!(order, [2, 3, 1, 0]);
        // Der Vorgänger kann bereits umbenannt sein; ein unklarer Readback darf
        // trotzdem keine folgende Zeile zur Wiederverwendung seines Namens befähigen.
        std::fs::rename(root.join("aaa"), root.join("aaaa")).unwrap();
        rows[2].status = BatchRenameStatus::OutcomeUnclear;
        assert!(dependency_failed(&rows, &dependencies[1]));
        assert!(dependency_failed(&rows, &dependencies[0]));
        assert!(!dependency_failed(&rows, &dependencies[3]));
        rows[1].status = BatchRenameStatus::FailedUnchanged;
        assert!(dependency_failed(&rows, &dependencies[0]));
        rows[2].status = BatchRenameStatus::Renamed;
        assert!(!dependency_failed(&rows, &dependencies[1]));
        assert!(dependency_failed(&rows, &dependencies[0]));
        rows[1].status = BatchRenameStatus::Renamed;
        assert!(!dependency_failed(&rows, &dependencies[0]));
        assert_eq!(std::fs::read(root.join("a")).unwrap(), b"a");
        assert_eq!(std::fs::read(root.join("aa")).unwrap(), b"aa");
        assert_eq!(std::fs::read(root.join("aaaa")).unwrap(), b"aaa");
    }
    #[test]
    fn ftp_schreibweise_abschluss_prueft_rohnamen_fakten_und_refresh() {
        let mut scenarios = 0;
        for fault in [
            "success",
            "noop",
            "incomplete",
            "scan_error",
            "late_alias",
            "wrong_raw",
            "session",
            "parent_error",
            "parent_missing",
            "parent_drift",
            "ftp_owner",
            "ftp_group",
            "ftp_created",
            "ftp_mode",
            "ftp_size",
            "ftp_mtime",
        ] {
            let area = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(area.path()).unwrap();
            std::fs::write(root.join("Foo"), b"source").unwrap();
            let fs = SpellingFs {
                local: crate::LocalVfs::rooted(&root).unwrap(),
                fault,
                listings: AtomicUsize::new(0),
                roots: AtomicUsize::new(0),
            };
            let listing = fs.list_complete(&root).unwrap();
            let source = listing.entries[0].entry.clone();
            let identity = fs.root_identity(&root).unwrap();
            let root_text = root.to_string_lossy().into_owned();
            let parents = ParentProof {
                root: root_text.clone(),
                entries: vec![(root_text.clone(), [1; 32], [2; 32])],
            };
            let mut fresh = parents.clone();
            fresh.entries[0].1 = [3; 32];
            if fault == "parent_drift" {
                fresh.entries[0].2 = [4; 32];
            }
            if fault != "noop" {
                std::fs::rename(root.join("Foo"), root.join("foo")).unwrap();
            }
            let row = BatchRenameRow {
                old_name: "Foo".into(),
                new_name: "foo".into(),
                current_name: None,
                status: BatchRenameStatus::Ready,
                message: String::new(),
                code: None,
            };
            let result = verify_ftp_spelling(
                &fs,
                FtpSpellingProof {
                    root: &root,
                    identity: &identity,
                    parents: &parents,
                    row: &row,
                    source: &source,
                },
                &mut || match fault {
                    "parent_error" => Err(io::Error::other("Elternreadback verloren")),
                    "parent_missing" => Ok(None),
                    _ => Ok(Some(fresh.clone())),
                },
            );
            if fault == "success" {
                let refresh = result.unwrap();
                assert!(refresh.parents == fresh);
                assert_eq!(refresh.identity, identity);
                assert!(
                    refresh.parents != parents,
                    "Der neue eigene Vollbeleg muss übernommen werden"
                );
            } else {
                assert!(result.is_err(), "{fault}");
            }
            assert_eq!(fs.listings.load(Ordering::SeqCst), 2, "{fault}");
            scenarios += 1;
        }
        eprintln!("FTP-Abschluss: {scenarios} exakte Rohgruppen-/Metadaten-/Refreshgegenfälle");
    }

    #[test]
    fn ftp_schreibweise_nur_vorab_bereite_eindeutige_ftps_quellen() {
        let area = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(area.path()).unwrap();
        std::fs::write(root.join("Foo"), b"source").unwrap();
        let listing = crate::LocalVfs::rooted(&root)
            .unwrap()
            .list_complete(&root)
            .unwrap();
        let mut scenarios = 0;
        for fault in [
            "valid",
            "raw",
            "incomplete",
            "alias_same_id",
            "alias_other_id",
            "symlink",
            "special",
            "preblocked",
        ] {
            let mut current = listing.clone();
            match fault {
                "raw" => current.entries[0].raw_name = b"FOO".to_vec(),
                "incomplete" => current.complete = false,
                "alias_same_id" | "alias_other_id" => {
                    let mut other = current.entries[0].clone();
                    other.entry.name = "FOO".into();
                    other.raw_name = b"FOO".to_vec();
                    if fault == "alias_other_id" {
                        other.entry.inode = Some(0);
                    }
                    current.entries.push(other);
                }
                "symlink" => current.entries[0].entry.is_symlink = true,
                "special" => current.entries[0].entry.is_file = false,
                _ => {}
            }
            for kind in [
                ProviderKind::Ftps,
                ProviderKind::Sftp,
                ProviderKind::Smb,
                ProviderKind::WebDav,
            ] {
                let mut rows = vec![BatchRenameRow {
                    old_name: "Foo".into(),
                    new_name: "foo".into(),
                    current_name: None,
                    status: if fault == "preblocked" {
                        BatchRenameStatus::Blocked
                    } else {
                        BatchRenameStatus::Ready
                    },
                    message: String::new(),
                    code: None,
                }];
                let candidates = ftp_spelling_candidates(kind, &current, &mut rows);
                assert_eq!(
                    candidates[0],
                    kind == ProviderKind::Ftps && fault == "valid",
                    "{kind:?}/{fault}"
                );
                scenarios += 1;
            }
        }
        eprintln!("FTP-Planung: {scenarios} Gruppen-/Providerfälle");
    }
}
