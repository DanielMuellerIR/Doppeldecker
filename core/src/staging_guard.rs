//! Gemeinsamer lesender Schutz vor registrierten Zwischenständen aller Fenster.
use crate::local_staging::StagingOwner;
use crate::{EndpointBinding, OperationQueue, Vfs};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct StagingEndpoint {
    pub(crate) vfs: Arc<dyn Vfs>,
    pub(crate) root: PathBuf,
    pub(crate) binding: EndpointBinding,
}
#[derive(Clone)]
pub(crate) struct KnownStagingRegistry {
    root: crate::local_fs::RootHandle,
    _process_lock: Option<Arc<crate::OperationQueueProcessLock>>,
}
impl KnownStagingRegistry {
    pub(crate) fn for_queue(queue: &OperationQueue) -> io::Result<Self> {
        let (_, lock, root) = queue.ftps_owner(None)?;
        Ok(Self {
            root,
            _process_lock: Some(lock),
        })
    }
    pub(crate) fn for_owner(owner: &StagingOwner) -> io::Result<Self> {
        Self::at_private_root(
            owner
                .queue
                .parent()
                .ok_or_else(|| io::Error::other("Stagingowner ohne privaten Queueordner"))?,
        )
    }
    pub(crate) fn at_private_root(path: &std::path::Path) -> io::Result<Self> {
        let root = crate::local_fs::RootHandle::open_nofollow(path)?;
        if !root.has_exclusive_namespace()? {
            return Err(blocked(
                "Der Stagingbereich ist nicht mehr privat gebunden.",
                &[],
            ));
        }
        Ok(Self {
            root,
            _process_lock: None,
        })
    }
    pub(crate) fn check(
        &self,
        endpoints: &[StagingEndpoint],
        ignore_target_owner: Option<&StagingOwner>,
    ) -> io::Result<[u8; 32]> {
        self.check_scoped(endpoints, ignore_target_owner, false)
    }
    pub(crate) fn check_both(
        &self,
        endpoints: &[StagingEndpoint],
        owner: Option<&StagingOwner>,
    ) -> io::Result<[u8; 32]> {
        self.check_scoped(endpoints, owner, true)
    }
    fn check_scoped(
        &self,
        endpoints: &[StagingEndpoint],
        ignore_target_owner: Option<&StagingOwner>,
        both: bool,
    ) -> io::Result<[u8; 32]> {
        if !self.root.is_current()? {
            return Err(blocked("Der Stagingbereich wurde ausgetauscht.", &[]));
        }
        let mut identities = Vec::new();
        for endpoint in endpoints {
            let identity = endpoint.vfs.root_identity(&endpoint.root)?;
            let current = match &endpoint.binding {
                EndpointBinding::Local(_) => EndpointBinding::local(&identity)?,
                EndpointBinding::Sftp(value)
                | EndpointBinding::Ftps(value)
                | EndpointBinding::WebDav(value)
                | EndpointBinding::Smb(value) => {
                    EndpointBinding::remote(&identity, value.account_digest)?
                }
            };
            if current != endpoint.binding {
                return Err(crate::real_mode::plan_drift(
                    "Endpunktidentität änderte sich vor der Zwischenstandsprüfung",
                ));
            }
            identities.push(identity);
        }
        let file_dacl_epoch = crate::file_dacl_registry::check_registry(
            &self.root,
            &identities,
            ignore_target_owner,
        )?;
        let created_epoch = crate::created_directory_dacl_registry::check_registry_observed(
            &self.root,
            &identities,
            ignore_target_owner,
            |index| {
                endpoints[index]
                    .vfs
                    .created_directory_root_scope(&endpoints[index].root, &identities[index])
            },
            |root| {
                for endpoint in endpoints {
                    if let Ok(Some(scope)) = endpoint.vfs.created_directory_foreign_root_scope(root)
                    {
                        return Ok(Some(scope));
                    }
                }
                Ok(None)
            },
        )?;
        let scopes = endpoints
            .iter()
            .zip(&identities)
            .map(|(endpoint, identity)| {
                (&endpoint.binding, Some(identity), endpoint.root.as_path())
            })
            .collect::<Vec<_>>();
        let read_view = if both {
            crate::local_staging::namespace_staging_view_excluding_both
        } else {
            crate::local_staging::namespace_staging_view_excluding
        };
        let view = read_view(self.root.path(), &scopes, ignore_target_owner).map_err(|error| {
            blocked(
                &format!("Bekannte Zwischenstände sind nicht vollständig prüfbar: {error}."),
                &crate::error::recovery_paths(&error),
            )
        })?;
        for candidate in view.candidates {
            let paths = [candidate.object.recovery_hint()];
            if matches!(
                candidate.object,
                crate::local_staging::StagingObject::DirectoryDacl(_)
            ) {
                return Err(crate::error::with_recovery_paths(crate::coded_error(
                    io::ErrorKind::WouldBlock, crate::ErrorCode::OutcomeUnclear,
                    "Eine offene Verzeichnis-DACL benötigt den bewussten Recoveryabschluss mit frischer Basis-, Owner-/Group- und DACL-Prüfung. Keine automatische Wiederholung oder Änderung des Ordners."), &paths));
            }
            let side = if candidate.side == 0 {
                "Quelle/links"
            } else {
                "Ziel/rechts"
            };
            if candidate.active {
                return Err(blocked(&format!("Ein offener Transfer auf {side} ist noch nicht als beendet belegt. Laufende Übertragung abwarten; abgebrochene Reste prüfen."), &paths));
            }
            if candidate.unknown {
                return Err(blocked(&format!("Ein bekannter Zwischenstand auf {side} ist nicht eindeutig prüfbar; seine Abwesenheit ist nicht belegt."), &paths));
            }
            if !candidate.requires_absence {
                continue;
            }
            match candidate
                .object
                .is_absent(endpoints[candidate.side].vfs.as_ref())
            {
                Ok(true) => {}
                Ok(false) => {
                    return Err(blocked(
                        &format!(
                        "Ein bekannter offener oder abgebrochener Transfer liegt noch auf {side}."
                    ),
                        &paths,
                    ))
                }
                Err(error) => {
                    return Err(blocked(
                        &format!(
                            "Bekannter Zwischenstand auf {side} ist nicht sicher prüfbar: {error}."
                        ),
                        &paths,
                    ))
                }
            }
        }
        if !self.root.is_current()? {
            return Err(blocked(
                "Der Stagingbereich änderte sich während der Prüfung.",
                &[],
            ));
        }
        let epoch = if let Some(created_epoch) = created_epoch {
            let mut hash = blake3::Hasher::new();
            hash.update(b"doppeldecker-staging-created-directory-v7\0");
            hash.update(&view.epoch);
            hash.update(&created_epoch);
            *hash.finalize().as_bytes()
        } else {
            view.epoch
        };
        if let Some(file_epoch) = file_dacl_epoch {
            let mut hash = blake3::Hasher::new();
            hash.update(b"doppeldecker-staging-with-file-dacl-v6\0");
            hash.update(&epoch);
            hash.update(&file_epoch);
            Ok(*hash.finalize().as_bytes())
        } else {
            Ok(epoch)
        }
    }
}
fn blocked(message: &str, paths: &[String]) -> io::Error {
    crate::error::with_recovery_paths(crate::coded_error(io::ErrorKind::WouldBlock, crate::ErrorCode::OutcomeUnclear,
        format!("{message} Reste am angegebenen Provider/Konto prüfen und über normale Dateiaktionen bewusst außerhalb beider Syncwurzeln sichern; danach frisch planen. Keine automatische Löschung oder Wiederholung.")), paths)
}
