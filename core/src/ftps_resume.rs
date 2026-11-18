//! Fortsetzbarer Download mit über TLS gebundenem Vollinhalt und FTP REST STREAM.
use super::*;
use crate::{FtpSecurityStatus, RemoteProfileBinding, RemoteProfileProtocol};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FtpsFileVersion {
    pub(crate) size: u64,
    pub(crate) mtime: Option<i64>,
    pub(crate) sha256: [u8; 32],
}

pub(crate) struct BoundFtpsResume {
    session: FtpsSession,
    remote: String,
}

pub(super) fn supports_resume(features: &suppaftp::types::Features) -> bool {
    features
        .get("REST")
        .and_then(Option::as_deref)
        .is_some_and(|mode| mode.eq_ignore_ascii_case("STREAM"))
        && features
            .get("HASH")
            .and_then(Option::as_deref)
            .is_some_and(|algorithms| {
                algorithms.split(';').any(|algorithm| {
                    algorithm
                        .trim_end_matches('*')
                        .eq_ignore_ascii_case("SHA-256")
                })
            })
}

fn unsupported_resume() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Fortsetzen braucht verifiziertes FTPS mit REST STREAM und HASH SHA-256. Regulär kopieren bleibt verfügbar.",
    )
}

fn drift(message: &str) -> io::Error {
    coded_error(io::ErrorKind::InvalidData, ErrorCode::PlanDrift, message)
}

fn parse_hash_response(response: &str, path: &str, size: u64) -> io::Result<[u8; 32]> {
    let fields = response
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("213 "))
        .map(|line| line.splitn(4, ' ').collect::<Vec<_>>());
    let Some(fields) = fields else {
        return Err(drift("FTPS lieferte keinen vollständigen SHA-256-Beleg."));
    };
    let ["SHA-256", range, hash, filename] = fields.as_slice() else {
        return Err(drift("FTPS lieferte keinen vollständigen SHA-256-Beleg."));
    };
    let full_size = range
        .strip_prefix("0-")
        .and_then(|end| end.parse::<u64>().ok());
    if full_size != Some(size) || *filename != path || hash.len() != 64 {
        return Err(drift(
            "FTPS-SHA-256 gilt nicht für den vollständigen gewählten Dateipfad.",
        ));
    }
    let mut digest = [0; 32];
    for (offset, byte) in digest.iter_mut().enumerate() {
        let digits = hash
            .get(offset * 2..offset * 2 + 2)
            .ok_or_else(|| drift("FTPS lieferte ungültige SHA-256-Ziffern."))?;
        *byte = u8::from_str_radix(digits, 16)
            .map_err(|_| drift("FTPS lieferte ungültige SHA-256-Ziffern."))?;
    }
    Ok(digest)
}

impl FtpsVfs {
    pub(crate) fn bind_resume_file(
        &self,
        path: &Path,
    ) -> io::Result<(FtpsFileVersion, BoundFtpsResume)> {
        if !matches!(
            self.connection_config().security_status(),
            FtpSecurityStatus::TlsIdentityExternallyVerified
                | FtpSecurityStatus::TlsIdentityVerified
        ) {
            return Err(unsupported_resume());
        }
        let remote = self.remote_path(path)?;
        let mut session = self.fresh_session()?;
        if !session.resume_supported {
            return Err(unsupported_resume());
        }
        // Die vollständige MLSD-Sicht bindet die Familie der ausgewählten Namen.
        let mut folder = session.identity.canonical_root.clone();
        for component in virtual_components(path)? {
            let child = join_server_path(&folder, component);
            let item = list_remote_directory(&mut session.stream, &folder)?
                .into_iter()
                .find(|item| item.entry.name == component)
                .ok_or_else(|| drift("FTPS-Quellpfad fehlt im Elternlisting."))?;
            let correct_kind = if child == remote {
                item.entry.is_file
            } else {
                item.entry.is_dir
            };
            if item.entry.is_symlink || !correct_kind {
                return Err(drift(
                    "FTPS-Quelle benötigt echte Ordner und eine reguläre Datei.",
                ));
            }
            folder = child;
        }
        let before = stat_remote(&mut session.stream, &remote)?;
        if !before.is_file || before.is_symlink {
            return Err(drift("FTPS-Quelle ist keine reguläre Datei."));
        }
        session
            .stream
            .opts("HASH", Some("SHA-256"))
            .map_err(ftp_error)?;
        let response = session
            .stream
            .custom_command(format!("HASH {remote}"), &[Status::File])
            .map_err(ftp_error)?;
        let response = std::str::from_utf8(&response.body)
            .map_err(|_| drift("FTPS-SHA-256-Antwort ist kein gültiges UTF-8."))?;
        let sha256 = parse_hash_response(response, &remote, before.size)?;
        let after = stat_remote(&mut session.stream, &remote)?;
        if !after.is_file
            || after.is_symlink
            || before.size != after.size
            || before.mtime != after.mtime
        {
            return Err(drift(
                "FTPS-Quelle änderte sich während der Prüfsummenabfrage.",
            ));
        }
        Ok((
            FtpsFileVersion {
                size: before.size,
                mtime: before.mtime,
                sha256,
            },
            BoundFtpsResume { session, remote },
        ))
    }
}

impl BoundFtpsResume {
    pub(crate) fn open_at(mut self, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        if offset > 0 {
            self.session
                .stream
                .resume_transfer(usize::try_from(offset).map_err(|_| unsupported_resume())?)
                .map_err(ftp_error)?;
        }
        let data = self
            .session
            .stream
            .retr_as_stream(self.remote)
            .map_err(ftp_error)?;
        data.get_ref().set_read_timeout(Some(IO_TIMEOUT))?;
        Ok(Box::new(FtpsRead {
            session: Some(self.session.stream),
            data: Some(Box::new(data)),
            finished: false,
        }))
    }
}

pub(crate) fn ensure_profile_current(
    queue: &Path,
    bindings: &[RemoteProfileBinding],
    config: &FtpsConnectionConfig,
) -> io::Result<()> {
    let [binding] = bindings else {
        return Err(drift(
            "FTPS-Auftrag benötigt genau ein gespeichertes Profil.",
        ));
    };
    let directory = queue
        .parent()
        .ok_or_else(|| drift("FTPS-Profilbindung besitzt keinen Konfigurationsordner."))?;
    let profiles = crate::load_ftps_profiles_versioned(&directory.join("ftps-profiles.json"))?;
    if binding.protocol != RemoteProfileProtocol::Ftps
        || !profiles.iter().any(|profile| {
            profile.profile.id == binding.profile_id
                && profile.revision == binding.revision
                && &profile.profile.connection == config
        })
    {
        return Err(drift(
            "FTPS-Profil hat sich geändert. Teilkopie bleibt erhalten.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proftpd_full_sha256_requires_bound_length_and_absolute_path() {
        let digest = "12".repeat(32);
        let reply =
            format!("213-Computing SHA-256 digest\r\n213 SHA-256 0-37 {digest} /probe ñ.txt");
        assert_eq!(
            parse_hash_response(&reply, "/probe ñ.txt", 37).unwrap(),
            [0x12; 32]
        );
        for response in [
            reply.replace("0-37", "0-36"),
            reply.replace("SHA-256 0", "SHA-1 0"),
            reply.replace("/probe", "/andere"),
            reply.replace(&digest, &"zz".repeat(32)),
        ] {
            assert!(
                parse_hash_response(&response, "/probe ñ.txt", 37).is_err(),
                "{response}"
            );
        }
    }
}
