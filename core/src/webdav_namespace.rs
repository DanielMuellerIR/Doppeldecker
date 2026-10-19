//! Gebundene Elternkette und begrenzte Lesephasen gewöhnlicher Namespaceaktionen.

use super::*;
use std::time::Instant;

pub(super) const PREPARATION_LIMIT: Duration = Duration::from_secs(60);

pub(super) fn validate_path(path: &Path) -> io::Result<()> {
    let normalized =
        normalize_virtual_path(path).map_err(|_| input("Unsicherer WebDAV-Namespacepfad"))?;
    if normalized.len() > 4096 || virtual_components(path)?.len() > 64 {
        return Err(input(
            "WebDAV-Namespacepfad überschreitet 4096 UTF-8-Bytes oder 64 Komponenten",
        ));
    }
    Ok(())
}

pub(super) fn read_children(
    binding: &BoundWebDavMutation,
    remote: &str,
    deadline: Instant,
) -> io::Result<Vec<ProofListEntry>> {
    check_deadline(deadline)?;
    let body = propfind(&binding.session, remote, "1")?;
    check_deadline(deadline)?;
    parse_collection_listing(&body, &binding.session.identity.origin, remote, true)
}

pub(super) fn directory_chain(
    binding: &BoundWebDavMutation,
    components: &[String],
    deadline: Instant,
) -> io::Result<Vec<WebDavResourceIdentity>> {
    let mut remote = binding.session.identity.canonical_root.clone();
    let mut paths = vec![remote.clone()];
    for component in components {
        remote = join_server_path(&remote, component);
        paths.push(remote.clone());
    }
    paths
        .into_iter()
        .map(|remote| {
            check_deadline(deadline)?;
            let identity = WebDavVfs::resource_identity_bound(binding, &remote, true)?;
            check_deadline(deadline)?;
            Ok(identity)
        })
        .collect()
}

pub(super) fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(coded_error(
            io::ErrorKind::TimedOut,
            ErrorCode::ConnectionLost,
            "WebDAV-Namespaceprüfung überschritt 60 Sekunden",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn drift(message: &str) -> io::Error {
    coded_error(io::ErrorKind::Interrupted, ErrorCode::PlanDrift, message)
}

fn input(message: &str) -> io::Error {
    coded_error(
        io::ErrorKind::InvalidInput,
        ErrorCode::InvalidInput,
        message,
    )
}
