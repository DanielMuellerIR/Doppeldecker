//! Gemeinsame Sitzungsbindung für unabhängige lesende Pane-Aufträge.
use crate::bidirectional_commands::BidirectionalEndpointRequest;
use crate::{require_absolute_remote_parent, AppError, AppState, MutexExt};
use doppeldecker_core::{LocalVfs, Vfs};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::queue_runtime::CachedRemoteConnection;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) enum EndpointIdentity {
    Local,
    Sftp { profile_id: String, generation: u64 },
    Ftps { profile_id: String, generation: u64 },
    Webdav { profile_id: String, generation: u64 },
    Smb { profile_id: String, generation: u64 },
}

pub(crate) struct EndpointContext {
    pub(crate) identity: EndpointIdentity,
    pub(crate) vfs: Arc<dyn Vfs>,
    pub(crate) parent: PathBuf,
    pub(crate) sftp: Option<Arc<doppeldecker_core::SftpVfs>>,
}

pub(crate) fn require_generation(expected: Option<u64>, actual: u64) -> Result<(), AppError> {
    if expected == Some(actual) && actual > 0 {
        Ok(())
    } else {
        Err(AppError::new(
            "plan_drift",
            "Die Remote-Sitzung hat seit dem sichtbaren Listing gewechselt",
        ))
    }
}

fn cached_read_connection<T>(
    pool: &Mutex<HashMap<String, CachedRemoteConnection<T>>>,
    profile_id: &str,
) -> Result<CachedRemoteConnection<T>, AppError> {
    pool.lock_recover().get(profile_id).cloned().ok_or_else(|| {
        AppError::new(
            "plan_drift",
            "Die sichtbare Remote-Sitzung ist nicht mehr verbunden",
        )
    })
}

/// Bindet einen Remote-Endpunkt an genau die Sitzung, die das sichtbare Listing
/// erzeugt hat.
///
/// Ein fehlender Pool-Eintrag bedeutet Drift. Ein verspäteter Leseauftrag darf
/// einen vom Benutzer getrennten Server nicht wieder verbinden.
macro_rules! bind_remote_endpoint {
    (
        $app:expr,
        $pool:expr,
        $session_generation:expr,
        $identity:ident,
        $profile_id:expr,
        $profile_revision:expr,
        $root:expr,
        $sftp:expr $(,)?
    ) => {{
        require_absolute_remote_parent($root)?;
        let cached = cached_read_connection(&$pool, $profile_id)?;
        crate::queue_runtime::ensure_requested_profile_revision(
            $profile_revision,
            &cached.binding,
        )?;
        crate::queue_runtime::ensure_profile_bindings_current(
            $app,
            std::slice::from_ref(&cached.binding),
        )?;
        let connection = cached.connection;
        let generation = connection.session_generation();
        require_generation($session_generation, generation)?;
        let sftp: Option<Arc<doppeldecker_core::SftpVfs>> = ($sftp)(&connection);
        Ok(EndpointContext {
            identity: EndpointIdentity::$identity {
                profile_id: $profile_id.clone(),
                generation,
            },
            vfs: connection,
            parent: PathBuf::from($root),
            sftp,
        })
    }};
}

#[cfg(test)]
#[test]
fn getrennte_sitzung_wird_beim_gebundenen_lesen_nicht_neu_angelegt() {
    let connection = CachedRemoteConnection {
        binding: doppeldecker_core::RemoteProfileBinding::new(
            doppeldecker_core::RemoteProfileProtocol::Sftp,
            "profil",
            1,
        )
        .unwrap(),
        connection: Arc::new(LocalVfs::new()),
    };
    let pool = Mutex::new(HashMap::from([("profil".to_owned(), connection)]));
    assert!(cached_read_connection(&pool, "profil").is_ok());
    pool.lock_recover().remove("profil");
    assert_eq!(
        cached_read_connection(&pool, "profil").err().unwrap().code,
        "plan_drift"
    );
    assert!(pool.lock_recover().is_empty());
}

pub(crate) fn resolve_read_endpoint(
    app: &tauri::AppHandle,
    state: &AppState,
    endpoint: &BidirectionalEndpointRequest,
    session_generation: Option<u64>,
) -> Result<EndpointContext, AppError> {
    match endpoint {
        BidirectionalEndpointRequest::Local { root } => {
            let endpoint = doppeldecker_core::ProviderEndpoint::local(Path::new(root))
                .map_err(AppError::from)?;
            Ok(EndpointContext {
                identity: EndpointIdentity::Local,
                vfs: Arc::new(LocalVfs::new()),
                parent: endpoint.root().to_path_buf(),
                sftp: None,
            })
        }
        BidirectionalEndpointRequest::Sftp {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state.sftp_connections,
            session_generation,
            Sftp,
            profile_id,
            *profile_revision,
            root,
            |connection: &Arc<doppeldecker_core::SftpVfs>| Some(connection.clone()),
        ),
        BidirectionalEndpointRequest::Ftps {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state.ftps_connections,
            session_generation,
            Ftps,
            profile_id,
            *profile_revision,
            root,
            |_| None,
        ),
        BidirectionalEndpointRequest::Webdav {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state.webdav_connections,
            session_generation,
            Webdav,
            profile_id,
            *profile_revision,
            root,
            |_| None,
        ),
        BidirectionalEndpointRequest::Smb {
            profile_id,
            profile_revision,
            root,
        } => bind_remote_endpoint!(
            app,
            state.smb_connections,
            session_generation,
            Smb,
            profile_id,
            *profile_revision,
            root,
            |_| None,
        ),
    }
}

/// Vergleicht neben der Sitzungsgeneration auch die konkrete `Arc` im gemeinsamen
/// Verbindungspool. Die alte `Arc` wird vom Größenlauf gehalten; ihre Adresse kann
/// deshalb während des Vergleichs nicht für eine neue Verbindung wiederverwendet
/// werden. So bleibt auch eine Ersatzverbindung mit erneutem Zählerstand 1 eindeutig.
pub(crate) fn provider_connection_is_current<T>(
    current: Option<Arc<T>>,
    expected_vfs: &Arc<dyn Vfs>,
    expected_generation: u64,
    generation_of: impl FnOnce(&T) -> u64,
) -> bool
where
    T: Vfs + 'static,
{
    current.is_some_and(|connection| {
        if generation_of(&connection) != expected_generation {
            return false;
        }
        let current_vfs: Arc<dyn Vfs> = connection;
        Arc::ptr_eq(&current_vfs, expected_vfs)
    })
}

/// Liest den bestehenden Verbindungs-Slot ohne einen Reconnect oder einen neuen
/// Profilaufbau auszulösen. Ein Wechsel während der Rekursion entwertet den Lauf,
/// bevor ein Resultat als verlässlich an die Oberfläche gelangt.
pub(crate) fn endpoint_is_current(
    state: &AppState,
    identity: &EndpointIdentity,
    expected_vfs: &Arc<dyn Vfs>,
) -> bool {
    match identity {
        EndpointIdentity::Local => true,
        EndpointIdentity::Sftp {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .sftp_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
        EndpointIdentity::Ftps {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .ftps_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
        EndpointIdentity::Webdav {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .webdav_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
        EndpointIdentity::Smb {
            profile_id,
            generation,
        } => provider_connection_is_current(
            state
                .smb_connections
                .lock_recover()
                .get(profile_id)
                .map(|cached| cached.connection.clone()),
            expected_vfs,
            *generation,
            |connection| connection.session_generation(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fehlende_alte_und_null_generationen_binden_keine_lesesitzung() {
        assert!(require_generation(Some(3), 3).is_ok());
        for (expected, actual) in [(None, 3), (Some(2), 3), (Some(0), 0)] {
            assert!(require_generation(expected, actual).is_err());
        }
    }
}
