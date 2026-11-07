//! Enge Testbindung virtueller Remoteziele an künstliche Nonces der lokalen Sandbox.
use crate::AppError;
use doppeldecker_core::{Cancel, LocalVfs, ProviderEndpoint, ProviderKind, RootIdentity, Vfs};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    local_root: String,
    provider: String,
    remote_root: String,
}
#[derive(PartialEq)]
struct Proof {
    local_root: RootIdentity,
    remote_root: RootIdentity,
    local_stat: Vec<u8>,
    nonce: Vec<u8>,
}
pub(crate) struct Binding {
    sandbox: PathBuf,
    endpoint: ProviderEndpoint,
    expected: Proof,
}
const NONCE: &str = ".doppeldecker-remote-test-nonce";
fn denied(message: &str) -> AppError {
    AppError::new("sandbox_violation", message)
}
impl Binding {
    pub(crate) fn prepare(
        sandbox: Option<&Path>,
        endpoint: &ProviderEndpoint,
        cancel: &Cancel,
    ) -> Result<Option<Self>, AppError> {
        let Some(sandbox) = sandbox.filter(|_| endpoint.kind() != ProviderKind::Local) else {
            return Ok(None);
        };
        let expected = proof(sandbox, endpoint, cancel)?;
        Ok(Some(Self {
            sandbox: sandbox.to_owned(),
            endpoint: endpoint.clone(),
            expected,
        }))
    }
    pub(crate) fn validate(&self, cancel: &Cancel) -> Result<(), AppError> {
        if proof(&self.sandbox, &self.endpoint, cancel)? != self.expected {
            return Err(denied(
                "Die gebundene Remote-Testsandbox oder ihre Nonce änderte sich",
            ));
        }
        Ok(())
    }
}
fn regular_bytes(vfs: &dyn Vfs, path: &Path, limit: u64) -> Result<(Vec<u8>, Vec<u8>), AppError> {
    let before = vfs.stat(path).map_err(AppError::from)?;
    if !before.is_file || before.is_symlink || before.size > limit {
        return Err(denied(
            "Testbindung verlangt eine reguläre, begrenzte Datei ohne Symlink",
        ));
    }
    let stat =
        serde_json::to_vec(&before).map_err(|_| denied("Testdateiidentität ist nicht lesbar"))?;
    let mut bytes = Vec::new();
    vfs.open_read(path)
        .map_err(AppError::from)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(AppError::from)?;
    let after = serde_json::to_vec(&vfs.stat(path).map_err(AppError::from)?)
        .map_err(|_| denied("Testdateiidentität ist nicht lesbar"))?;
    if bytes.len() as u64 > limit
        || before.size != bytes.len() as u64
        || stat != after
        || !vfs.anchor_is_current().map_err(AppError::from)?
    {
        return Err(denied(
            "Testdatei oder Provideridentität änderte sich beim Lesen",
        ));
    }
    Ok((bytes, stat))
}
fn proof(sandbox: &Path, endpoint: &ProviderEndpoint, cancel: &Cancel) -> Result<Proof, AppError> {
    if cancel.is_cancelled() {
        return Err(AppError::new(
            "interrupted",
            "Testbindung wurde abgebrochen",
        ));
    }
    let sandbox = doppeldecker_core::validate_test_sandbox_root(sandbox).map_err(AppError::from)?;
    let local = LocalVfs::rooted(&sandbox).map_err(AppError::from)?;
    let (json, _) = regular_bytes(
        &local,
        &sandbox.join(".doppeldecker-remote-test-roots.json"),
        32768,
    )?;
    let declarations: Vec<Declaration> = serde_json::from_slice(&json)
        .map_err(|_| denied("Ungültige Remote-Testroot-Deklaration"))?;
    let provider = format!("{:?}", endpoint.kind()).to_ascii_lowercase();
    let declaration = declarations
        .iter()
        .find(|d| d.provider == provider && Path::new(&d.remote_root) == endpoint.root())
        .ok_or_else(|| {
            denied("Das konkrete Remote-Testziel ist nicht in der markierten Sandbox deklariert")
        })?;
    let relative = Path::new(&declaration.local_root);
    if relative.components().next().is_none()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(denied(
            "Der lokale Nonceordner muss ein echter Unterordner der Testsandbox sein",
        ));
    }
    let local_root = sandbox.join(relative);
    let local_identity = local.root_identity(&local_root).map_err(AppError::from)?;
    let (nonce, local_stat) = regular_bytes(&local, &local_root.join(NONCE), 32)?;
    if nonce.len() != 32 {
        return Err(denied(
            "Die lokale Testnonce muss exakt 32 Zufallsbytes enthalten",
        ));
    }
    let remote_path = endpoint.root().join(NONCE);
    let (remote, _root) = endpoint
        .interactive_file_read_context(std::slice::from_ref(&remote_path), cancel)
        .map_err(AppError::from)?;
    let remote_identity = endpoint_identity(endpoint, cancel)?;
    let (remote_nonce, _) = regular_bytes(remote.as_ref(), &remote_path, 32)?;
    if remote_nonce != nonce || endpoint_identity(endpoint, cancel)? != remote_identity {
        return Err(denied(
            "Die Remote-Testnonce stimmt nicht mit dem konkreten lokalen Wegwerfziel überein",
        ));
    }
    // Die 256 Zufallsbits werden direkt verglichen und nur im RAM gehalten; keine verkürzte Prüfsumme.
    Ok(Proof {
        local_root: local_identity,
        remote_root: remote_identity,
        local_stat,
        nonce,
    })
}

fn endpoint_identity(
    endpoint: &ProviderEndpoint,
    cancel: &Cancel,
) -> Result<RootIdentity, AppError> {
    endpoint
        .content_sync_root_identity(cancel)
        .map_err(AppError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn falsche_nonce_stoppt_ohne_aenderung_des_testziels() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(
            root.join(doppeldecker_core::TEST_SANDBOX_MARKER),
            doppeldecker_core::TEST_SANDBOX_MARKER_CONTENT,
        )
        .unwrap();
        let local = root.join("lokale-bindung");
        let server = root.join("server-testziel");
        std::fs::create_dir(&local).unwrap();
        std::fs::create_dir(&server).unwrap();
        let mut nonce = [0u8; 32];
        getrandom::fill(&mut nonce).unwrap();
        std::fs::write(local.join(NONCE), nonce).unwrap();
        std::fs::write(server.join(NONCE), nonce).unwrap();
        std::fs::write(
            root.join(".doppeldecker-remote-test-roots.json"),
            serde_json::to_vec(&serde_json::json!([
                {"local_root":"lokale-bindung","provider":"local","remote_root":server}
            ]))
            .unwrap(),
        )
        .unwrap();
        let endpoint = ProviderEndpoint::local(&server).unwrap();
        assert!(proof(&root, &endpoint, &Cancel::new()).is_ok());
        nonce[0] ^= 1;
        std::fs::write(server.join(NONCE), nonce).unwrap();
        let error = proof(&root, &endpoint, &Cancel::new())
            .err()
            .expect("Falsche Nonce muss scheitern");
        assert_eq!(error.code, "sandbox_violation");
        assert_eq!(std::fs::read(server.join(NONCE)).unwrap(), nonce);
        assert_eq!(std::fs::read_dir(&server).unwrap().count(), 1);
    }
}
