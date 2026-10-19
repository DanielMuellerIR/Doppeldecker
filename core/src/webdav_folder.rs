//! Eine gewöhnliche Ordneraddition verwendet keine künstliche Collection und
//! folglich auch kein rekursives DELETE zum Aufräumen einer Capability-Probe.

use super::interactive_namespace::{
    check_deadline, directory_chain, drift, read_children, PREPARATION_LIMIT,
};
use super::*;
use crate::directory_create::{
    directory_create_contract, validate_directory_create_request, DirectoryCreateExecutionClass,
    DirectoryCreateReport,
};
use std::time::Instant;

impl WebDavVfs {
    pub(crate) fn create_interactive_directory(
        &self,
        parent: &Path,
        name: &str,
    ) -> io::Result<DirectoryCreateReport> {
        let name = validate_directory_create_request(parent, name)?;
        validate_name(&name)?;
        let components = virtual_components(parent)?;
        let (binding, _) = self.bind_mutation(
            parent,
            |_| true,
            "WebDAV-Ordneranlage benötigt eine Sitzung",
        )?;
        let deadline = Instant::now() + PREPARATION_LIMIT;
        let parents = directory_chain(&binding, &components, deadline)?;
        self.create_bound_directory(parent, &name, &binding, &parents, deadline, &mut || Ok(()))
            .map(|(report, _)| report)
    }

    pub(super) fn create_bound_directory(
        &self,
        parent: &Path,
        name: &str,
        binding: &BoundWebDavMutation,
        parents: &[WebDavResourceIdentity],
        deadline: Instant,
        before_mutation: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<(DirectoryCreateReport, Option<WebDavResourceIdentity>)> {
        let name = validate_directory_create_request(parent, name)?;
        validate_name(&name)?;
        let destination = parent.join(&name);
        let components = virtual_components(parent)?;
        let remote_parent = map_virtual_path(&binding.session.identity.canonical_root, parent)?;
        let remote = join_server_path(&remote_parent, &name);
        if directory_chain(binding, &components, deadline)? != parents {
            return Err(drift(
                "WebDAV-Elternkette änderte sich vor der Ordneranlage",
            ));
        }
        let children = read_children(binding, &remote_parent, deadline)?;
        if children
            .iter()
            .any(|entry| crate::remote_rename::names_may_collide(&entry.entry.name, &name))
        {
            return Err(coded_error(io::ErrorKind::AlreadyExists, ErrorCode::AlreadyExists, "Ein möglicherweise gleichwertiger Ordnername ist bereits vorhanden; nichts angelegt"));
        }
        if directory_chain(binding, &components, deadline)? != parents {
            return Err(drift(
                "WebDAV-Elternkette änderte sich während der Vorbereitung",
            ));
        }
        self.ensure_mutation_binding_current(binding)?;
        check_deadline(deadline)?;
        before_mutation()?;
        check_deadline(deadline)?;
        self.ensure_mutation_binding_current(binding)?;
        // Der eine gewünschte MKCOL bindet nur die unmittelbare Elternversion. Es gibt
        // keine Gegenprobe und keinen zweiten mutierenden Versuch bei Antwortverlust.
        let response = with_parent_condition(
            authenticated(
                binding.session.http.mutation_client.request(
                    custom_method(b"MKCOL")?,
                    resource_url(&binding.session.identity.origin, &remote)?,
                ),
                &binding.session.http,
            ),
            &binding.session.identity.origin,
            parents,
            &remote,
        )?
        .header(IF_NONE_MATCH, "*")
        .timeout(TLS_IO_TIMEOUT)
        .send()
        .map_err(|error| unclear(&destination, http_error(error)))?;
        let status = response.status();
        if status != StatusCode::CREATED {
            // Die fachliche Ablehnung wird nur nach einer lesenden Zustandsprüfung
            // als solche gemeldet. Andere Antworten könnten eine Wirkung verdecken.
            if matches!(
                status,
                StatusCode::PRECONDITION_FAILED
                    | StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::CONFLICT
            ) {
                let deadline = Instant::now() + PREPARATION_LIMIT;
                self.ensure_mutation_binding_current(binding)
                    .map_err(|error| unclear(&destination, error))?;
                let fresh = directory_chain(binding, &components, deadline)
                    .map_err(|error| unclear(&destination, error))?;
                let children = read_children(binding, &remote_parent, deadline)
                    .map_err(|error| unclear(&destination, error))?;
                if fresh != parents {
                    return Err(drift("WebDAV-Elternbedingungen wurden beim Anlegen abgelehnt; Elternbestand neu laden"));
                }
                if children
                    .iter()
                    .any(|entry| crate::remote_rename::names_may_collide(&entry.entry.name, &name))
                {
                    return Err(coded_error(
                        io::ErrorKind::AlreadyExists,
                        ErrorCode::AlreadyExists,
                        "Späte WebDAV-Namenskollision; vorhandenen Eintrag erhalten",
                    ));
                }
                return Err(coded_error(io::ErrorKind::Unsupported, ErrorCode::Unsupported, format!("WebDAV-Server lehnte die bedingte Ordneranlage mit HTTP {} ab; kein neuer Name sichtbar", status.as_u16())));
            }
            return Err(unclear(
                &destination,
                format!("Anlegeantwort HTTP {}", status.as_u16()),
            ));
        }
        let created_etag = if response.headers().contains_key(ETAG) {
            Some(
                require_strong_etag(response.headers(), "WebDAV-MKCOL")
                    .map_err(|error| unclear(&destination, error))?,
            )
        } else {
            None
        };
        let readback = (|| {
            let deadline = Instant::now() + PREPARATION_LIMIT;
            self.ensure_mutation_binding_current(binding)?;
            let proof = stat_proof_in_session(&binding.session, &remote)?;
            if !proof.entry.is_dir || proof.entry.is_file || proof.entry.is_symlink {
                return Err(invalid("WebDAV-MKCOL-Readback ist keine echte Collection"));
            }
            if let Some(expected) = &created_etag {
                if proof.provider_token.as_deref() != Some(expected.as_bytes()) {
                    return Err(invalid(
                        "WebDAV-MKCOL-Readback besitzt eine andere ETag als die Anlegeantwort",
                    ));
                }
            }
            if !read_children(binding, &remote, deadline)?.is_empty() {
                return Err(invalid(
                    "Die angelegte WebDAV-Collection ist beim Readback nicht leer",
                ));
            }
            let children = read_children(binding, &remote_parent, deadline)?;
            let matching = children
                .iter()
                .filter(|child| crate::remote_rename::names_may_collide(&child.entry.name, &name))
                .collect::<Vec<_>>();
            if matching.len() != 1
                || matching[0].entry.name != name
                || !matching[0].entry.is_dir
                || matching[0].entry.is_symlink
                || matching[0].provider_token != proof.provider_token
            {
                return Err(invalid(
                    "WebDAV-Elternreadback bestätigt den neuen Namen und Typ nicht eindeutig",
                ));
            }
            if directory_chain(binding, &components, deadline)? != parents {
                return Err(drift(
                    "WebDAV-Elternkette änderte sich nach der Ordneranlage",
                ));
            }
            let fresh = stat_proof_in_session(&binding.session, &remote)?;
            if !fresh.entry.is_dir
                || fresh.entry.is_symlink
                || fresh.provider_token != proof.provider_token
            {
                return Err(drift(
                    "WebDAV-Collection änderte sich im abschließenden Readback",
                ));
            }
            let identity = proof
                .provider_token
                .map(|token| {
                    let etag = String::from_utf8(token)
                        .map_err(|_| invalid("Ungültiger Collectiontoken"))?;
                    validate_strong_etag(&etag)?;
                    Ok::<_, io::Error>(WebDavResourceIdentity {
                        remote: remote.clone(),
                        etag,
                    })
                })
                .transpose()?;
            self.ensure_mutation_binding_current(binding)?;
            Ok(identity)
        })();
        let identity = readback.map_err(|error| unclear(&destination, error))?;
        let mut limitations = directory_create_contract(crate::ProviderKind::WebDav).limitations;
        if created_etag.is_none() {
            limitations.push("Die Anlegeantwort enthielt keine ETag; Name, leere Collection und Elternbestand wurden unabhängig zurückgelesen, ohne direkten Identitätsvergleich mit der Antwort.".into());
        }
        Ok((
            DirectoryCreateReport {
                path: destination,
                execution_class: DirectoryCreateExecutionClass::BestEffort,
                limitations,
            },
            identity,
        ))
    }
}

fn unclear(path: &Path, error: impl std::fmt::Display) -> io::Error {
    let error = coded_error(io::ErrorKind::Other, ErrorCode::OutcomeUnclear, format!("WebDAV-Ordneranlage hat einen unklaren Ausgang: {error}. Nicht automatisch wiederholen oder entfernen; möglichen neuen Ordner prüfen: {}", path.display()));
    crate::error::with_recovery_context(error, &[], &[path])
}
