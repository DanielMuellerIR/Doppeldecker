use super::*;
use crate::local_staging::RemoteTempIntent;
use crate::remote_rename_cycle::{RemoteRenameCycle, RenameCycleMove, RenameCycleRecord};

impl PreparedRemoteBatchRename {
    pub(super) fn execute_cycle(
        &mut self,
        cycle: &RemoteRenameCycle,
        cancel: &Cancel,
        before_each: &mut dyn FnMut() -> io::Result<()>,
        queue: Option<&crate::OperationQueue>,
    ) -> Option<String> {
        let Some(queue) = queue else {
            for &member in &cycle.members {
                set_status(&mut self.rows[member], BatchRenameStatus::FailedUnchanged,
                    "Der entfernte Dateinamenzyklus benötigt ein dauerhaftes Fensterjournal; nichts geändert");
            }
            return Some("Kein dauerhaftes Journal für den Namenstausch".into());
        };
        let mut record = RenameCycleRecord {
            id: cycle.id.clone(),
            root: self.root.to_string_lossy().into_owned(),
            identity: self.identity.clone(),
            old_names: cycle
                .members
                .iter()
                .map(|&index| self.rows[index].old_name.clone())
                .collect(),
            new_names: cycle
                .members
                .iter()
                .map(|&index| self.rows[index].new_name.clone())
                .collect(),
            current_names: cycle
                .members
                .iter()
                .map(|&index| self.rows[index].old_name.clone())
                .collect(),
            temp_name: cycle.temp_name.clone(),
            pending: None,
            finished: false,
        };
        let mut intents: Vec<RemoteTempIntent> = Vec::new();
        let mut uncertain = None;
        let mut effects = 0;
        let result = (|| -> io::Result<()> {
            before_each()?;
            check_cancel(cancel)?;
            let (mut owner, _process_lock, _) = queue.ftps_owner(None)?;
            owner.operation = cycle.id.clone();
            let context = self.endpoint.remote_batch_context(cancel)?;
            let binding = self
                .endpoint
                .content_sync_endpoint_binding(&self.identity)?;
            let account = match &binding {
                crate::EndpointBinding::Sftp(value)
                | crate::EndpointBinding::Ftps(value)
                | crate::EndpointBinding::WebDav(value)
                | crate::EndpointBinding::Smb(value) => value.account_digest,
                _ => {
                    return Err(io::Error::other(
                        "Namenstausch ohne gebundenes Remote-Konto",
                    ))
                }
            };
            let registry = crate::staging_guard::KnownStagingRegistry::for_queue(queue)?;
            registry.check_both(
                &[crate::staging_guard::StagingEndpoint {
                    vfs: context.clone(),
                    root: self.root.clone(),
                    binding: binding.clone(),
                }],
                None,
            )?;
            queue.checkpoint_rename_cycle(&record)?;
            for name in record
                .old_names
                .iter()
                .chain(std::iter::once(&record.temp_name))
            {
                let intent = RemoteTempIntent::create_bound_with_root(
                    &owner,
                    &self.root.join(name),
                    &self.root.join(&record.temp_name),
                    &self.identity,
                    account,
                )?;
                intent.rename_cycle_pending()?;
                intents.push(intent);
            }
            registry.check_both(
                &[crate::staging_guard::StagingEndpoint {
                    vfs: context,
                    root: self.root.clone(),
                    binding,
                }],
                Some(&owner),
            )?;
            let steps = crate::remote_rename_cycle::steps(cycle.members.len());
            for (step_index, position) in steps.into_iter().enumerate() {
                check_cancel(cancel)?;
                before_each()?;
                // Sämtliche bekannten Positionen vor dem nächsten Schritt prüfen,
                // auch die bereits endgültig umbenannten Mitglieder des Zyklus.
                for (member, name) in cycle.members.iter().zip(&record.current_names) {
                    let (_, fresh) = self
                        .endpoint
                        .remote_batch_source(&self.root.join(name), cancel)?;
                    if self.proofs[*member].as_ref().map(|proof| proof.1) != Some(fresh) {
                        return Err(drift("Ein Mitglied des Dateinamenzyklus änderte sich"));
                    }
                }
                let to = if step_index == 0 {
                    record.temp_name.clone()
                } else {
                    record.new_names[position].clone()
                };
                let from = record.current_names[position].clone();
                record.pending = Some(RenameCycleMove {
                    member: position,
                    from: from.clone(),
                    to: to.clone(),
                });
                queue.checkpoint_rename_cycle(&record)?;
                let result = self.move_cycle_member(
                    cycle.members[position],
                    &from,
                    &to,
                    cancel,
                    before_each,
                );
                if let Err(error) = result {
                    if crate::error_code(&error) == ErrorCode::OutcomeUnclear {
                        uncertain = Some(position);
                    } else {
                        record.pending = None;
                        queue.checkpoint_rename_cycle(&record)?;
                    }
                    return Err(error);
                }
                effects += 1;
                record.current_names[position] = to;
                record.pending = None;
                if let Err(error) = queue.checkpoint_rename_cycle(&record) {
                    return Err(crate::coded_error(
                        io::ErrorKind::Other,
                        ErrorCode::OutcomeUnclear,
                        format!("Namensänderung bestätigt, dauerhafte Teilbilanz unklar: {error}"),
                    ));
                }
            }
            for intent in &intents {
                intent.finish()?;
            }
            intents.clear();
            record.finished = true;
            queue.checkpoint_rename_cycle(&record)?;
            Ok(())
        })();
        let error = result.err();
        // Vor jeder tatsächlichen Mutation dürfen eigene reine Prüfhinweise wieder
        // geschlossen werden. Nach einer Wirkung bleiben alle offenen Intents erhalten.
        let mut recovery_needed = effects > 0 || uncertain.is_some();
        if !recovery_needed {
            for intent in &intents {
                if let Err(error) = intent.finish() {
                    recovery_needed = true;
                    self.limitations.push(format!(
                        "Keine Namensänderung begonnen; lokaler Prüfhinweis blieb offen: {error}"
                    ));
                }
            }
            if error.is_some() && !recovery_needed {
                if let Err(error) = queue.discard_unstarted_rename_cycle(&record) {
                    recovery_needed = true;
                    self.limitations.push(format!(
                        "Keine Namensänderung begonnen; Queueabschluss blieb offen: {error}"
                    ));
                }
            }
        }
        if let Some(error) = error.as_ref().filter(|_| recovery_needed) {
            self.limitations.push(format!("Dateinamenzyklus {} nicht vollständig abgeschlossen: {error}. Dauerhafte Queuebilanz und Zwischenname {} prüfen; keine automatische Wiederholung.", cycle.id, cycle.temp_name));
        }
        for (position, &member) in cycle.members.iter().enumerate() {
            let row = &mut self.rows[member];
            if uncertain == Some(position) {
                set_status(row, BatchRenameStatus::OutcomeUnclear,
                    format!("Mögliche Namensänderung unklar; {} und Zwischenname {} anhand der Queuebilanz prüfen", record.root, record.temp_name));
                row.current_name = None;
            } else if record.current_names[position] == record.new_names[position] {
                set_status(
                    row,
                    BatchRenameStatus::Renamed,
                    "Zielname des Dateinamenzyklus bestätigt",
                );
            } else if record.current_names[position] != record.old_names[position] {
                set_status(row, BatchRenameStatus::PartiallyRenamed,
                    format!("Teiländerung bestätigt; aktueller Name „{}“. Kein automatischer Rücktausch", record.current_names[position]));
                row.current_name = Some(record.current_names[position].clone());
            } else {
                set_status(
                    row,
                    if cancel.is_cancelled() {
                        BatchRenameStatus::Cancelled
                    } else {
                        BatchRenameStatus::FailedUnchanged
                    },
                    error
                        .as_ref()
                        .map_or_else(|| "Nicht geändert".into(), |error| error.to_string()),
                );
                if !cancel.is_cancelled() {
                    row.code = error.as_ref().map(crate::error_code);
                }
            }
        }
        error.map(|error| error.to_string())
    }

    fn move_cycle_member(
        &mut self,
        member: usize,
        from: &str,
        to: &str,
        cancel: &Cancel,
        before_each: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        check_cancel(cancel)?;
        before_each()?;
        let attempt = Cancel::new();
        let context = self.endpoint.remote_batch_context(&attempt)?;
        if context.root_identity(&self.root)? != self.identity
            || self.endpoint.remote_batch_parents(&attempt)? != self.parents
        {
            return Err(drift(
                "Remote-Root oder Eltern änderten sich vor dem Zyklusschritt",
            ));
        }
        let old = self.root.join(from);
        let (_, fresh) = self.endpoint.remote_batch_source(&old, &attempt)?;
        if self.proofs[member].as_ref().map(|proof| proof.1) != Some(fresh) {
            return Err(drift("Zyklusquelle änderte sich vor Rename"));
        }
        check_cancel(cancel)?;
        let (report, refresh) = if self.endpoint.kind() == ProviderKind::WebDav {
            self.endpoint
                .execute_webdav_batch_rename(
                    &old,
                    to,
                    &self.identity,
                    self.parents.as_ref().expect("WebDAV-Eltern"),
                    fresh,
                )
                .map(|(report, refresh)| (report, Some(refresh)))?
        } else {
            (
                self.endpoint.execute_remote_batch_rename(
                    context.as_ref(),
                    &old,
                    to,
                    self.class,
                    self.limitations.clone(),
                )?,
                None,
            )
        };
        if report.outcome == RenameOutcome::Collision {
            return Err(crate::coded_error(
                io::ErrorKind::AlreadyExists,
                ErrorCode::AlreadyExists,
                "Zyklusschritt trifft einen inzwischen belegten Zielnamen",
            ));
        }
        let new = self.root.join(to);
        let readback = (|| {
            let proof = self.endpoint.remote_batch_source(&new, &attempt)?;
            if !self.proofs[member]
                .as_ref()
                .is_some_and(|expected| crate::batch_rename::same_source(&expected.0, &proof.0))
            {
                return Err(drift("Zyklusziel besitzt nicht das bestätigte Objekt"));
            }
            if let Some(refresh) = refresh {
                self.identity = refresh.identity;
                self.parents = Some(refresh.parents);
            } else {
                let parents = self.endpoint.remote_batch_parents(&attempt)?;
                match (&self.parents, &parents) {
                    (Some(old), Some(new)) if old.own_rename_refresh(new) => {}
                    (None, None) => {}
                    _ => {
                        return Err(drift(
                            "Remote-Eltern änderten sich außerhalb der eigenen Zykluswirkung",
                        ))
                    }
                }
                if context.root_identity(&self.root)? != self.identity {
                    return Err(drift(
                        "Remote-Identität änderte sich nach dem Zyklusschritt",
                    ));
                }
                self.parents = parents;
            }
            self.proofs[member] = Some(proof);
            Ok(())
        })();
        readback.map_err(|error| {
            crate::error::with_recovery_paths(
                crate::coded_error(
                    io::ErrorKind::Other,
                    ErrorCode::OutcomeUnclear,
                    format!("Rename gemeldet, Zyklusschritt nicht abschließend bestätigt: {error}"),
                ),
                &[
                    old.to_string_lossy().into_owned(),
                    new.to_string_lossy().into_owned(),
                ],
            )
        })
    }
}
