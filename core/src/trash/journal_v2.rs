//! Striktes persistiertes Belegformat. Ein gültiges Dokument erteilt noch keine
//! Berechtigung: Recovery muss Mount, Marker und Rechte frisch über Handles binden.
use std::io;

use serde::{Deserialize, Serialize};

pub(super) mod claim;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
struct ByteName(Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
struct BytePath(Vec<u8>);

impl ByteName {
    fn valid(&self) -> bool {
        !self.0.is_empty()
            && self.0.len() <= 255
            && !self.0.contains(&0)
            && !self.0.contains(&b'/')
            && self.0 != b"."
            && self.0 != b".."
    }
}

impl BytePath {
    fn valid(&self) -> bool {
        self.0.first() == Some(&b'/')
            && self.0.len() <= 4096
            && !self.0.contains(&0)
            && (self.0 == b"/"
                || self.0[1..]
                    .split(|byte| *byte == b'/')
                    .all(|part| ByteName(part.to_vec()).valid()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ObjectIdentity {
    device: u64,
    inode: u64,
    file_type: u32,
}

impl ObjectIdentity {
    fn valid(&self) -> bool {
        self.inode != 0
            && matches!(
                self.file_type,
                0o010000 | 0o020000 | 0o040000 | 0o060000 | 0o100000 | 0o120000 | 0o140000
            )
    }
}

// Mount-IDs sind laufzeitgebunden. Der gespeicherte Boot-/Volume-Beleg verhindert,
// dass ein später gleich nummerierter Mount schon als derselbe gelten kann.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "os", rename_all = "snake_case", deny_unknown_fields)]
enum MountEvidence {
    Macos {
        fsid: [u8; 8],
        volume_id: [u8; 16],
        fs_type: String,
        mount_flags: u64,
    },
    Linux {
        boot_id: [u8; 16],
        mount_id: u64,
        unique_mount_id: bool,
        fsid: Vec<u8>,
        fs_type: String,
        mount_flags: u64,
    },
}

impl MountEvidence {
    fn valid(&self) -> bool {
        let fs_type = match self {
            Self::Macos {
                fsid,
                volume_id,
                fs_type,
                ..
            } => {
                if fsid.iter().all(|byte| *byte == 0) || volume_id.iter().all(|byte| *byte == 0) {
                    return false;
                }
                fs_type
            }
            Self::Linux {
                boot_id,
                mount_id,
                fsid,
                fs_type,
                ..
            } => {
                if boot_id.iter().all(|byte| *byte == 0)
                    || *mount_id == 0
                    || fsid.is_empty()
                    || fsid.len() > 32
                {
                    return false;
                }
                fs_type
            }
        };
        !fs_type.is_empty()
            && fs_type.len() <= 32
            && fs_type
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "digest",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum AclEvidence {
    Absent,
    Digest([u8; 32]),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DirectoryEvidence {
    path: BytePath,
    identity: ObjectIdentity,
    mount: MountEvidence,
    owner: u32,
    mode: u32,
    acl: AclEvidence,
}

impl DirectoryEvidence {
    fn valid(&self) -> bool {
        self.path.valid()
            && self.identity.valid()
            && self.identity.file_type == 0o040000
            && self.mount.valid()
            && self.mode <= 0o7777
            && !matches!(self.acl, AclEvidence::Digest(digest) if digest.iter().all(|byte| *byte == 0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PrivateArea {
    anchor: DirectoryEvidence,
    name: ByteName,
    identity: ObjectIdentity,
    owner: u32,
    mode: u32,
    acl: AclEvidence,
    marker_name: ByteName,
    marker_identity: ObjectIdentity,
    marker_nonce: [u8; 16],
    marker_digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AreaIntent {
    anchor: DirectoryEvidence,
    name: ByteName,
    marker_name: ByteName,
    marker_nonce: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Platform {
    Macos,
    Xdg,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrashEvidence {
    platform: Platform,
    files: DirectoryEvidence,
    info: Option<DirectoryEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InfoEvidence {
    private_name: ByteName,
    published_name: ByteName,
    identity: ObjectIdentity,
    bytes: u64,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum EntryPhase {
    Planned,
    Untouched,
    ClaimIntent,
    Claimed,
    InfoPublishIntent,
    InfoReady,
    TransferIntent,
    Trashed,
    RestoreIntent,
    Restored,
    Drifted,
    Unclear,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    source_parent: DirectoryEvidence,
    source_name: ByteName,
    private_name: ByteName,
    expected: ObjectIdentity,
    claimed: Option<ObjectIdentity>,
    trash_name: ByteName,
    info: Option<InfoEvidence>,
    phase: EntryPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BatchPhase {
    Planned,
    AreaReady,
    CleanupIntent,
    Settled,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    version: u32,
    run_id: String,
    effective_uid: u32,
    journal_parent: DirectoryEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox: Option<DirectoryEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    area_intent: Option<AreaIntent>,
    area: Option<PrivateArea>,
    trash: TrashEvidence,
    entries: Vec<Entry>,
    phase: BatchPhase,
}

pub(super) fn decode(data: &[u8]) -> io::Result<Journal> {
    if data.len() as u64 > super::MAX_JOURNAL_BYTES {
        return Err(invalid(
            "Papierkorb-Journal v2 überschreitet die Größenbegrenzung",
        ));
    }
    let journal: Journal =
        serde_json::from_slice(data).map_err(|error| invalid(error.to_string()))?;
    journal.validate()?;
    Ok(journal)
}

impl Journal {
    fn validate(&self) -> io::Result<()> {
        if self.version != 2
            || self.run_id.len() != 32
            || !self
                .run_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.run_id.bytes().all(|byte| byte == b'0')
            || self.entries.is_empty()
            || self.entries.len() > 10_000
            || !self.journal_parent.valid()
            || self.journal_parent.owner != self.effective_uid
            || self.journal_parent.mode & 0o022 != 0
            || matches!(self.journal_parent.acl, AclEvidence::Unknown)
            || self.sandbox.as_ref().is_some_and(|root| !root.valid())
            || !self.trash.files.valid()
            || self.trash.info.as_ref().is_some_and(|info| !info.valid())
            || (self.trash.platform == Platform::Xdg) != self.trash.info.is_some()
        {
            return Err(invalid("Ungültige Lauf-/Verzeichnisbindung in Journal v2"));
        }
        let data_mount = &self.trash.files.mount;
        if let Some(intent) = &self.area_intent {
            let nonce: String = intent
                .marker_nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            if !intent.anchor.valid()
                || intent.anchor.owner != self.effective_uid
                || intent.anchor.mode & 0o022 != 0
                || matches!(intent.anchor.acl, AclEvidence::Unknown)
                || &intent.anchor.mount != data_mount
                || intent.name.0 != format!(".doppeldecker-trash-{}", self.run_id).as_bytes()
                || !intent.marker_name.valid()
                || nonce != self.run_id
                || self.area.as_ref().is_some_and(|area| {
                    area.anchor != intent.anchor
                        || area.name != intent.name
                        || area.marker_name != intent.marker_name
                        || area.marker_nonce != intent.marker_nonce
                })
            {
                return Err(invalid("Ungültiger privater Anlage-Intent in Journal v2"));
            }
        }
        if (self.trash.platform == Platform::Macos)
            != matches!(data_mount, MountEvidence::Macos { .. })
        {
            return Err(invalid("Plattform und Mountbeleg widersprechen sich"));
        }
        if self
            .trash
            .info
            .as_ref()
            .is_some_and(|info| &info.mount != data_mount)
        {
            return Err(invalid(
                "XDG-Daten und Info liegen auf verschiedenen Mounts",
            ));
        }
        match &self.area {
            Some(area) => {
                let nonce: String = area
                    .marker_nonce
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                if !area.anchor.valid()
                    || area.anchor.owner != self.effective_uid
                    || area.anchor.mode & 0o022 != 0
                    || matches!(area.anchor.acl, AclEvidence::Unknown)
                    || &area.anchor.mount != data_mount
                    || area.name.0 != format!(".doppeldecker-trash-{}", self.run_id).as_bytes()
                    || !area.identity.valid()
                    || area.identity.device != self.trash.files.identity.device
                    || area.anchor.identity.device != area.identity.device
                    || area.marker_identity.device != area.identity.device
                    || area.identity.file_type != 0o040000
                    || area.owner != self.effective_uid
                    || area.mode != 0o700
                    || matches!(area.acl, AclEvidence::Unknown)
                    || matches!(area.acl, AclEvidence::Digest(digest) if digest.iter().all(|byte| *byte == 0))
                    || !area.marker_name.valid()
                    || !area.marker_identity.valid()
                    || area.marker_identity.file_type != 0o100000
                    || nonce != self.run_id
                    || area.marker_digest.iter().all(|byte| *byte == 0)
                {
                    return Err(invalid(
                        "Ungültige private Bereichs-/Markerbindung in Journal v2",
                    ));
                }
            }
            None if self.phase != BatchPhase::Planned => {
                return Err(invalid("Privater Bereich fehlt nach planned"))
            }
            None => {}
        }
        let mut private_names = std::collections::BTreeSet::new();
        if let Some(intent) = &self.area_intent {
            private_names.insert(&intent.marker_name.0);
        }
        if let Some(area) = &self.area {
            private_names.insert(&area.marker_name.0);
        }
        let mut trash_names = std::collections::BTreeSet::new();
        let mut source_names = std::collections::BTreeSet::new();
        let mut info_names = std::collections::BTreeSet::new();
        for entry in &self.entries {
            if !entry.source_parent.valid()
                || &entry.source_parent.mount != data_mount
                || !entry.source_name.valid()
                || !entry.private_name.valid()
                || !entry.trash_name.valid()
                || !entry.expected.valid()
                || entry.expected.device != entry.source_parent.identity.device
                || entry.expected.device != self.trash.files.identity.device
                || entry
                    .claimed
                    .as_ref()
                    .is_some_and(|identity| !identity.valid())
                || !private_names.insert(&entry.private_name.0)
                || !trash_names.insert(&entry.trash_name.0)
                || !source_names.insert((&entry.source_parent.path.0, &entry.source_name.0))
                || (self.phase == BatchPhase::Planned && entry.phase != EntryPhase::Planned)
                || (entry.phase != EntryPhase::Planned && self.area.is_none())
            {
                return Err(invalid(
                    "Ungültiger oder doppelt gebundener Journaleintrag v2",
                ));
            }
            if matches!(
                entry.phase,
                EntryPhase::Planned | EntryPhase::Untouched | EntryPhase::ClaimIntent
            ) && entry.claimed.is_some()
            {
                return Err(invalid("Claim-Identität ohne beobachteten Claim"));
            }
            if entry.phase == EntryPhase::Untouched && entry.info.is_some() {
                return Err(invalid(
                    "Unangetasteter Eintrag darf keine Info-Datei besitzen",
                ));
            }
            let claimed = !matches!(
                entry.phase,
                EntryPhase::Planned
                    | EntryPhase::Untouched
                    | EntryPhase::ClaimIntent
                    | EntryPhase::Drifted
                    | EntryPhase::Unclear
            );
            if claimed && entry.claimed.is_none() {
                return Err(invalid("Dauerhafte Claim-Identität fehlt"));
            }
            if !matches!(
                entry.phase,
                EntryPhase::Drifted
                    | EntryPhase::Unclear
                    | EntryPhase::RestoreIntent
                    | EntryPhase::Restored
            ) && entry
                .claimed
                .as_ref()
                .is_some_and(|identity| identity != &entry.expected)
            {
                return Err(invalid(
                    "Abweichender Claim darf nicht in den Transfer gelangen",
                ));
            }
            if matches!(self.phase, BatchPhase::CleanupIntent | BatchPhase::Settled)
                && !matches!(
                    entry.phase,
                    EntryPhase::Trashed | EntryPhase::Restored | EntryPhase::Untouched
                )
            {
                return Err(invalid("Cleanup benötigt terminale Datenaktionen"));
            }
            let needs_info = matches!(
                entry.phase,
                EntryPhase::InfoPublishIntent
                    | EntryPhase::InfoReady
                    | EntryPhase::TransferIntent
                    | EntryPhase::Trashed
            );
            if self.trash.platform == Platform::Macos
                && (entry.info.is_some()
                    || matches!(
                        entry.phase,
                        EntryPhase::InfoPublishIntent | EntryPhase::InfoReady
                    ))
            {
                return Err(invalid("macOS-Journal darf keine XDG-Infophase enthalten"));
            }
            if self.trash.platform == Platform::Xdg && needs_info && entry.info.is_none() {
                return Err(invalid("XDG-Transfer benötigt seine gebundene Info-Datei"));
            }
            if let Some(info) = &entry.info {
                let mut expected_name = entry.trash_name.0.clone();
                expected_name.extend_from_slice(b".trashinfo");
                if !info.private_name.valid()
                    || !info.published_name.valid()
                    || info.published_name.0 != expected_name
                    || !info.identity.valid()
                    || info.identity.device != self.trash.files.identity.device
                    || info.identity.file_type != 0o100000
                    || info.bytes == 0
                    || info.bytes > 16 * 1024
                    || info.digest.iter().all(|byte| *byte == 0)
                    || !private_names.insert(&info.private_name.0)
                    || !info_names.insert(&info.published_name.0)
                {
                    return Err(invalid("Ungültige oder kollidierende XDG-Info-Bindung"));
                }
            }
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
pub(super) fn fixture() -> serde_json::Value {
    let mount = serde_json::json!({"os":"macos", "fsid":vec![3;8],
        "volume_id":vec![7;16], "fs_type":"apfs", "mount_flags":0});
    let directory = serde_json::json!({"path":[47,116,109,112],
        "identity":{"device":5,"inode":12,"file_type":0o040000},
        "mount":mount, "owner":501, "mode":0o700, "acl":{"kind":"absent"}});
    let run_id = format!("{}ab", "01".repeat(15));
    serde_json::json!({"version":2,"run_id":run_id,"effective_uid":501,"journal_parent":directory,
        "area":{"anchor":directory,"name":format!(".doppeldecker-trash-{run_id}").as_bytes(),
            "identity":{"device":5,"inode":13,"file_type":0o040000},"owner":501,"mode":0o700,
            "acl":{"kind":"absent"},"marker_name":b".doppeldecker-trash-marker".to_vec(),"marker_identity":{"device":5,"inode":14,"file_type":0o100000},
            "marker_nonce":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,171],"marker_digest":vec![9;32]},
        "trash":{"platform":"macos","files":directory,"info":null},
        "entries":[{"source_parent":directory,"source_name":[255,37,50,70],"private_name":[101,110,116,114,121,45,49],
            "expected":{"device":5,"inode":21,"file_type":0o100000},
            "claimed":{"device":5,"inode":21,"file_type":0o100000},"trash_name":[255,37,50,70],"info":null,"phase":"claimed"}],
        "phase":"area_ready"})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn read(value: &serde_json::Value) -> io::Result<Journal> {
        decode(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn raw_namen_und_prozentsequenzen_bleiben_bytegenau() {
        let value = fixture();
        let journal = read(&value).unwrap();
        assert_eq!(
            journal.entries[0].source_name.0,
            vec![255, b'%', b'2', b'F']
        );
        let encoded = serde_json::to_vec(&journal).unwrap();
        assert_eq!(decode(&encoded).unwrap(), journal);
        assert_eq!(serde_json::to_value(&journal).unwrap(), value);
    }

    #[test]
    fn raw_pfadinhalte_und_maximale_namen_bleiben_lesbar_aber_nul_nicht() {
        let mut value = fixture();
        value["entries"][0]["source_parent"]["path"] = serde_json::json!([47, 120, 255, 37]);
        value["entries"][0]["source_name"] = vec![255_u8; 255].into();
        read(&value).unwrap();
        value["entries"][0]["source_name"] = vec![255_u8; 256].into();
        assert!(read(&value).is_err());
        assert_eq!(
            decode(&vec![b' '; super::super::MAX_JOURNAL_BYTES as usize + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn version_unknown_felder_und_ungueltige_pfadinhalte_sind_fehler() {
        let base = fixture();
        read(&base).unwrap();
        for (pointer, replacement) in [
            ("/version", serde_json::json!(3)),
            ("/run_id", serde_json::json!("kurz")),
            ("/area/marker_nonce", serde_json::json!(vec![8; 16])),
            ("/area/owner", serde_json::json!(502)),
            ("/journal_parent/owner", serde_json::json!(502)),
            ("/journal_parent/mode", serde_json::json!(0o777)),
            ("/area/mode", serde_json::json!(0o777)),
            ("/area/anchor/acl", serde_json::json!({"kind":"unknown"})),
            ("/entries/0/source_name", serde_json::json!([46, 46])),
            ("/entries/0/source_name", serde_json::json!([47])),
            ("/entries/0/source_name", serde_json::json!([0])),
            (
                "/entries/0/source_parent/path",
                serde_json::json!([47, 46, 46, 47, 120]),
            ),
            (
                "/entries/0/source_parent/mount/mount_flags",
                serde_json::json!(1),
            ),
            ("/entries/0/claimed/inode", serde_json::json!(22)),
            ("/phase", serde_json::json!("cleanup_intent")),
            ("/entries/0/phase", serde_json::json!("info_ready")),
        ] {
            let mut value = base.clone();
            *value.pointer_mut(pointer).unwrap() = replacement;
            assert_eq!(
                read(&value).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{pointer}"
            );
        }
        for pointer in [
            "",
            "/area",
            "/area/anchor",
            "/area/anchor/mount",
            "/entries/0",
            "/entries/0/expected",
        ] {
            let mut value = base.clone();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unbekannt".into(), true.into());
            assert!(read(&value).is_err(), "{pointer}");
        }
    }

    #[test]
    fn doppelte_und_markergebundene_claimnamen_werden_nicht_uebernommen() {
        let mut value = fixture();
        let duplicate = value["entries"][0].clone();
        value["entries"].as_array_mut().unwrap().push(duplicate);
        assert!(read(&value).is_err());
        let mut value = fixture();
        value["entries"][0]["private_name"] = value["area"]["marker_name"].clone();
        assert!(read(&value).is_err());
    }

    #[test]
    fn anlageintent_bleibt_vor_marker_lesbar_und_muss_spaeter_zum_bereich_passen() {
        let mut value = fixture();
        let area = value["area"].clone();
        value["area_intent"] = serde_json::json!({
            "anchor":area["anchor"], "name":area["name"],
            "marker_name":area["marker_name"], "marker_nonce":area["marker_nonce"]
        });
        read(&value).unwrap();
        for pointer in [
            "/area_intent/anchor/owner",
            "/area_intent/anchor/mount/mount_flags",
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).unwrap() = 999.into();
            assert!(read(&changed).is_err(), "{pointer}");
        }
        let mut changed = value.clone();
        changed["area_intent"]["marker_name"] = b"fremder-marker".to_vec().into();
        assert!(read(&changed).is_err());
        value["area"] = serde_json::Value::Null;
        value["phase"] = "planned".into();
        value["entries"][0]["phase"] = "planned".into();
        value["entries"][0]["claimed"] = serde_json::Value::Null;
        let journal = read(&value).unwrap();
        assert_eq!(
            decode(&serde_json::to_vec(&journal).unwrap()).unwrap(),
            journal
        );
        value["entries"][0]["private_name"] = value["area_intent"]["marker_name"].clone();
        assert!(read(&value).is_err());
    }

    #[test]
    fn xdg_transfer_braucht_seine_vorbereitete_info_paarung() {
        let mut value = fixture();
        let mount = serde_json::json!({"os":"linux", "boot_id":vec![7;16], "mount_id":19,
            "unique_mount_id":true, "fsid":[3,4,5,6], "fs_type":"ext4", "mount_flags":0});
        for pointer in [
            "/journal_parent/mount",
            "/area/anchor/mount",
            "/trash/files/mount",
            "/entries/0/source_parent/mount",
        ] {
            *value.pointer_mut(pointer).unwrap() = mount.clone();
        }
        value["trash"]["platform"] = "xdg".into();
        value["trash"]["info"] = value["trash"]["files"].clone();
        value["entries"][0]["phase"] = "transfer_intent".into();
        assert!(read(&value).is_err());
        let mut name = value["entries"][0]["trash_name"]
            .as_array()
            .unwrap()
            .clone();
        name.extend(b".trashinfo".iter().map(|byte| serde_json::json!(byte)));
        value["entries"][0]["info"] = serde_json::json!({
            "private_name":b"info-1".to_vec(), "published_name":name,
            "identity":{"device":5,"inode":25,"file_type":0o100000},"bytes":101,"digest":vec![7;32]
        });
        read(&value).unwrap();
        value["entries"][0]["info"]["published_name"] = b"fremd.trashinfo".to_vec().into();
        assert!(read(&value).is_err());
    }

    #[test]
    fn drift_darf_nur_zum_tatsaechlichen_claim_restore_fuehren_und_nie_in_cleanup() {
        let mut value = fixture();
        value["entries"][0]["claimed"]["inode"] = 99.into();
        value["entries"][0]["phase"] = "restore_intent".into();
        read(&value).unwrap();
        value["entries"][0]["phase"] = "drifted".into();
        read(&value).unwrap();
        value["phase"] = "cleanup_intent".into();
        assert!(read(&value).is_err());
        value["entries"][0]["phase"] = "restored".into();
        read(&value).unwrap();
    }
}
