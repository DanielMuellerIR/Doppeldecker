//! Gemeinsame Sitzungsbindung für unabhängige lesende Pane-Aufträge.
use crate::bidirectional_commands::BidirectionalEndpointRequest;
use crate::{require_absolute_remote_parent, AppError, AppState, MutexExt};
use doppeldecker_core::{LocalVfs, Vfs};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Bindet einen Remote-Endpunkt an genau die Sitzung, die das sichtbare Listing
/// erzeugt hat.
///
/// Für alle vier Provider ist der Ablauf derselbe: absoluter Elternpfad, gecachte
/// Verbindung mit geprüfter Profilrevision und dieselbe Sitzungsgeneration wie in
/// der Anfrage. Nur das Providermodul und die Identitätsvariante unterscheiden
/// sich. Als vier Kopien hätte eine davon still eine dieser Prüfungen verlieren
/// können — deshalb steht der Ablauf hier einmal.
macro_rules! bind_remote_endpoint {
    (
        $app:expr,
        $state:expr,
        $session_generation:expr,
        $module:ident,
        $identity:ident,
        $profile_id:expr,
        $profile_revision:expr,
        $root:expr,
        $sftp:expr $(,)?
    ) => {{
        require_absolute_remote_parent($root)?;
        let cached =
            crate::$module::cached_connection_bound($app, $state, $profile_id, $profile_revision)?;
        crate::queue_runtime::ensure_requested_profile_revision(
            $profile_revision,
            &cached.binding,
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
            state,
            session_generation,
            sftp_commands,
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
            state,
            session_generation,
            ftps_commands,
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
            state,
            session_generation,
            webdav_commands,
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
            state,
            session_generation,
            smb_commands,
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
