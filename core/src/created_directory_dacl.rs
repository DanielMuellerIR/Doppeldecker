//! Getrennte V7-/V8-Rechtewirkungen an selbst exklusiv neu angelegten SMB-Ordnern.
use crate::windows_dacl::WindowsDaclWriteContext;
use crate::windows_security::WindowsDacl;
use serde::ser::SerializeStruct;
use std::io;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryDaclSelection {
    pub after: WindowsDacl,
    pub probe_context: WindowsDaclWriteContext,
}
impl CreatedDirectoryDaclSelection {
    pub fn validate(&self) -> io::Result<()> {
        self.probe_context.validate()?;
        if let Some(issue) = self
            .after
            .directory_write_issue(self.probe_context.namespace, &self.probe_context.owner)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, issue.reason()));
        }
        Ok(())
    }
}
/// Direkter ungefilterter Quellunterbestand; Inhalte bleiben den normalen
/// Kindplänen unterstellt. Der Digest bindet Rohbytes und Typen, keine Anzeige-Namen.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryChildrenProof {
    pub entries: u64,
    pub digest: [u8; 32],
}
impl CreatedDirectoryChildrenProof {
    pub(crate) fn from_listing(listing: &crate::vfs::CompleteDirectoryListing) -> io::Result<Self> {
        crate::real_mode::require_complete_safe_listing(listing, "V8-Quellunterbestand")?;
        if listing.entries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "V8 benötigt einen nichtleeren rohen Quellunterbestand",
            ));
        }
        let mut entries = listing.entries.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
        let mut hash = blake3::Hasher::new();
        hash.update(b"doppeldecker-created-directory-children-v8\0");
        hash.update(&(entries.len() as u64).to_le_bytes());
        for item in entries {
            hash.update(&(item.raw_name.len() as u64).to_le_bytes());
            hash.update(&item.raw_name);
            hash.update(&[
                u8::from(item.entry.is_dir),
                u8::from(item.entry.is_file),
                u8::from(item.entry.is_symlink),
                u8::from(item.safely_decoded),
            ]);
        }
        Ok(Self {
            entries: listing.entries.len() as u64,
            digest: *hash.finalize().as_bytes(),
        })
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.entries == 0 || self.digest == [0; 32] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Ungültiger V8-Quellunterbestandsbeleg",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryBeforeChildrenSelection {
    pub after: WindowsDacl,
    pub probe_context: WindowsDaclWriteContext,
    pub source_children: CreatedDirectoryChildrenProof,
}
impl CreatedDirectoryBeforeChildrenSelection {
    pub fn validate(&self) -> io::Result<()> {
        self.probe_context.validate()?;
        self.source_children.validate()?;
        if let Some(issue) = self.after.directory_before_children_write_issue(
            self.probe_context.namespace,
            &self.probe_context.owner,
        ) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, issue.reason()));
        }
        Ok(())
    }
}

/// V7-spezifischer lesender Objektbeleg; alte RootIdentity-Formate bleiben unverändert.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryRootScope {
    pub object_namespace: [u8; 32],
    pub root_file_id: u64,
    pub ancestor_file_ids: Vec<u64>,
}
impl CreatedDirectoryRootScope {
    pub fn validate(&self, identity: &crate::RootIdentity) -> io::Result<()> {
        let crate::RootIdentity::Smb(root) = identity else {
            return Err(io::Error::other("V7-Scope benötigt SMB"));
        };
        if self.object_namespace
            != crate::created_directory_dacl_registry::object_namespace(identity)?
            || self.root_file_id == 0
            || self.root_file_id != root.root_file_id
            || self.ancestor_file_ids.contains(&0)
            || self.ancestor_file_ids.contains(&self.root_file_id)
            || self
                .ancestor_file_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.ancestor_file_ids.len()
        {
            return Err(io::Error::other(
                "V7-Root-/Vorfahrenbeleg ist nicht vollständig an die beobachtete Root-ID gebunden",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CreatedDirectoryOrigin {
    ExclusiveMinimalCreate,
    ExclusiveMinimalCreateBeforeChildren,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryDaclPlan {
    pub origin: CreatedDirectoryOrigin,
    pub file_id: u64,
    pub object_namespace: [u8; 32],
    pub before: WindowsDacl,
    pub after: WindowsDacl,
    pub context: WindowsDaclWriteContext,
}
impl CreatedDirectoryDaclPlan {
    pub fn validate(&self) -> io::Result<()> {
        self.before.validate()?;
        self.context.validate()?;
        if self.file_id == 0
            || self.object_namespace == [0; 32]
            || self.before.namespace != self.context.namespace
            || self.before == self.after
        {
            return Err(io::Error::other("V7 benötigt die original erzeugte Objekt-ID und beobachtete unterschiedliche Rechtezustände"));
        }
        // Before ist nur Beobachtung: geerbte und ungeschützte Anfangsrechte bleiben zulässig.
        if self.origin == CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren {
            if let Some(issue) = self
                .after
                .directory_before_children_write_issue(self.context.namespace, &self.context.owner)
            {
                return Err(io::Error::new(io::ErrorKind::InvalidData, issue.reason()));
            }
            return Ok(());
        }
        CreatedDirectoryDaclSelection {
            after: self.after.clone(),
            probe_context: self.context.clone(),
        }
        .validate()
    }
    pub fn outcome(&self, directory: &str) -> io::Result<CreatedDirectoryDaclOutcome> {
        self.validate()?;
        Ok(CreatedDirectoryDaclOutcome {
            origin: self.origin,
            directory: directory.into(),
            file_id: self.file_id,
            object_namespace: self.object_namespace,
            before_digest: self.effect_digest(&self.before)?,
            after_digest: self.effect_digest(&self.after)?,
        })
    }
    fn effect_digest(&self, acl: &WindowsDacl) -> io::Result<[u8; 32]> {
        if self.origin == CreatedDirectoryOrigin::ExclusiveMinimalCreate {
            return dacl_digest(acl);
        }
        acl.validate()?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"doppeldecker-created-directory-dacl-v8\0");
        hash.update(&serde_json::to_vec(acl).map_err(io::Error::other)?);
        Ok(*hash.finalize().as_bytes())
    }
}
pub fn dacl_digest(acl: &WindowsDacl) -> io::Result<[u8; 32]> {
    acl.validate()?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"doppeldecker-created-directory-dacl-v7\0");
    hash.update(&serde_json::to_vec(acl).map_err(io::Error::other)?);
    Ok(*hash.finalize().as_bytes())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedDirectoryDaclOutcome {
    pub origin: CreatedDirectoryOrigin,
    pub directory: String,
    pub file_id: u64,
    pub object_namespace: [u8; 32],
    pub before_digest: [u8; 32],
    pub after_digest: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedDirectoryEffectKey {
    pub origin: CreatedDirectoryOrigin,
    pub directory: String,
    pub file_id: u64,
    pub object_namespace: [u8; 32],
}
// V7 bleibt bytegleich numerisch. Nur die neue V8-Wireform nutzt dezimale
// Zeichenfolgen: JSON-Zahlen verlieren in JavaScript oberhalb 2^53 Identität.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ObjectIdWire {
    Number(u64),
    Decimal(String),
}
fn decode_object_id(
    origin: CreatedDirectoryOrigin,
    value: ObjectIdWire,
) -> Result<u64, &'static str> {
    match (origin, value) {
        (CreatedDirectoryOrigin::ExclusiveMinimalCreate, ObjectIdWire::Number(value)) => Ok(value),
        (
            CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren,
            ObjectIdWire::Decimal(text),
        ) => {
            let value = text
                .parse::<u64>()
                .map_err(|_| "V8 benötigt eine kanonische Dezimal-ID im u64-Bereich")?;
            if value == 0 || value.to_string() != text {
                return Err("V8 benötigt eine positive kanonische Dezimal-ID");
            }
            Ok(value)
        }
        _ => Err("Objekt-ID-Wireform passt nicht zur Herkunft: V7 Zahl, V8 Dezimalzeichenfolge"),
    }
}
fn serialize_object_id<S: SerializeStruct>(
    state: &mut S,
    origin: CreatedDirectoryOrigin,
    value: u64,
) -> Result<(), S::Error> {
    match origin {
        CreatedDirectoryOrigin::ExclusiveMinimalCreate => state.serialize_field("file_id", &value),
        CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren => {
            if value == 0 {
                return Err(serde::ser::Error::custom(
                    "V8 benötigt eine positive Objekt-ID",
                ));
            }
            state.serialize_field("file_id", &value.to_string())
        }
    }
}
impl serde::Serialize for CreatedDirectoryEffectKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CreatedDirectoryEffectKey", 4)?;
        state.serialize_field("origin", &self.origin)?;
        state.serialize_field("directory", &self.directory)?;
        serialize_object_id(&mut state, self.origin, self.file_id)?;
        state.serialize_field("object_namespace", &self.object_namespace)?;
        state.end()
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectKeyWire {
    origin: CreatedDirectoryOrigin,
    directory: String,
    file_id: ObjectIdWire,
    object_namespace: [u8; 32],
}
impl<'de> serde::Deserialize<'de> for CreatedDirectoryEffectKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = EffectKeyWire::deserialize(deserializer)?;
        let file_id =
            decode_object_id(wire.origin, wire.file_id).map_err(serde::de::Error::custom)?;
        Ok(Self {
            origin: wire.origin,
            directory: wire.directory,
            file_id,
            object_namespace: wire.object_namespace,
        })
    }
}
impl serde::Serialize for CreatedDirectoryDaclOutcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("CreatedDirectoryDaclOutcome", 6)?;
        state.serialize_field("origin", &self.origin)?;
        state.serialize_field("directory", &self.directory)?;
        serialize_object_id(&mut state, self.origin, self.file_id)?;
        state.serialize_field("object_namespace", &self.object_namespace)?;
        state.serialize_field("before_digest", &self.before_digest)?;
        state.serialize_field("after_digest", &self.after_digest)?;
        state.end()
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeWire {
    origin: CreatedDirectoryOrigin,
    directory: String,
    file_id: ObjectIdWire,
    object_namespace: [u8; 32],
    before_digest: [u8; 32],
    after_digest: [u8; 32],
}
impl<'de> serde::Deserialize<'de> for CreatedDirectoryDaclOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OutcomeWire::deserialize(deserializer)?;
        let file_id =
            decode_object_id(wire.origin, wire.file_id).map_err(serde::de::Error::custom)?;
        Ok(Self {
            origin: wire.origin,
            directory: wire.directory,
            file_id,
            object_namespace: wire.object_namespace,
            before_digest: wire.before_digest,
            after_digest: wire.after_digest,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryObservation {
    pub effect_key: CreatedDirectoryEffectKey,
    /// true bestätigt vollständige gewöhnliche Metadaten, keine DACL-Wirkung.
    /// Bewusst ausgelassene Zeit oder normale Modusbits bleiben false; die Auslassung steht im Metadatenbericht.
    pub ordinary_metadata_applied: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryDaclReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<CreatedDirectoryObservation>,
    pub completed: Vec<CreatedDirectoryDaclOutcome>,
    pub uncertain: Vec<CreatedDirectoryDaclOutcome>,
}
impl CreatedDirectoryDaclReport {
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.completed.is_empty() && self.uncertain.is_empty()
    }
    pub(crate) fn reconcile(&mut self, resolved: &[CreatedDirectoryDaclOutcome], report: &Self) {
        self.uncertain.retain(|o| !resolved.contains(o));
        self.merge(report);
    }
    pub fn has_rights_effects(&self) -> bool {
        !self.completed.is_empty() || !self.uncertain.is_empty()
    }
    pub fn observe_created(&mut self, observation: CreatedDirectoryObservation) {
        if let Some(old) = self
            .created
            .iter_mut()
            .find(|old| old.effect_key == observation.effect_key)
        {
            old.ordinary_metadata_applied |= observation.ordinary_metadata_applied;
        } else {
            self.created.push(observation);
        }
    }
    pub fn merge(&mut self, other: &Self) {
        for observation in &other.created {
            self.observe_created(observation.clone());
        }
        for o in &other.completed {
            if !self.completed.contains(o) {
                self.completed.push(o.clone());
            }
        }
        for o in &other.uncertain {
            if !self.uncertain.contains(o) {
                self.uncertain.push(o.clone());
            }
        }
        self.uncertain.retain(|o| !self.completed.contains(o));
    }
}
/// Ausschließlich aus validierten Store-Records abgeleitete RAM-Herkunft.
/// Prepared hat noch keine Rechtewirkung und darf deshalb keinen Outcome erfinden.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BeforeChildrenRecovery {
    #[default]
    Absent,
    Historical,
    Pending,
}
pub trait CreatedDirectoryDaclReceipt: Send {
    fn effect_key(&self) -> Option<CreatedDirectoryEffectKey>;
    fn abandon(&mut self) -> io::Result<()>;
    fn omission(&self) -> crate::windows_dacl::WindowsDaclTargetIssue;
    /// Führt gewöhnliche mtime genau einmal aus, gegebenenfalls erst nach Originalfreigabe.
    /// false bedeutet endgültig aufgegebenes Receipt, niemals einen erneuten Zeit-SET.
    /// Eine bekannte normale Zeitauslassung kann nach Freigabe als typisierter Fehler folgen;
    /// der Aufrufer bilanziert sie, ohne die normale Mutation nochmals auszuführen.
    fn set_mtime(&mut self, mtime: Option<i64>) -> io::Result<bool>;
    fn begin(
        self: Box<Self>,
        selection: &CreatedDirectoryDaclSelection,
    ) -> io::Result<Box<dyn CreatedDirectoryDaclTransaction>>;
    fn begin_before_children(
        mut self: Box<Self>,
        _selection: &CreatedDirectoryBeforeChildrenSelection,
    ) -> io::Result<Box<dyn CreatedDirectoryDaclTransaction>> {
        self.abandon()?;
        Err(crate::windows_dacl::write_omission(
            crate::windows_dacl::WindowsDaclTargetIssue::UnsupportedBackend,
        ))
    }
}
pub trait CreatedDirectoryDaclTransaction: Send {
    fn abandon_prepared(self: Box<Self>) -> io::Result<()>;
    fn apply(&mut self) -> io::Result<CreatedDirectoryDaclReport>;
    fn report(&self) -> CreatedDirectoryDaclReport;
    fn recovery_paths(&self) -> Vec<String>;
    fn finish(self: Box<Self>) -> io::Result<CreatedDirectoryDaclReport>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedDirectoryDaclRecoveryObservation {
    pub file_id: u64,
    pub object_namespace: [u8; 32],
    pub context: WindowsDaclWriteContext,
    pub dacl: WindowsDacl,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryDaclPreviewEffect {
    pub directory: String,
    pub selection: CreatedDirectoryDaclSelection,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedDirectoryBeforeChildrenPreviewEffect {
    pub directory: String,
    pub selection: CreatedDirectoryBeforeChildrenSelection,
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    fn key(origin: CreatedDirectoryOrigin, file_id: u64) -> CreatedDirectoryEffectKey {
        CreatedDirectoryEffectKey {
            origin,
            directory: "/same/Exact<&>🌍".into(),
            file_id,
            object_namespace: [1; 32],
        }
    }
    fn outcome(key: &CreatedDirectoryEffectKey) -> CreatedDirectoryDaclOutcome {
        CreatedDirectoryDaclOutcome {
            origin: key.origin,
            directory: key.directory.clone(),
            file_id: key.file_id,
            object_namespace: key.object_namespace,
            before_digest: [2; 32],
            after_digest: [3; 32],
        }
    }
    #[test]
    fn v7_key_outcome_report_wire_bytes_remain_golden() {
        let key = key(CreatedDirectoryOrigin::ExclusiveMinimalCreate, u64::MAX);
        let outcome = outcome(&key);
        let report = CreatedDirectoryDaclReport {
            created: vec![CreatedDirectoryObservation {
                effect_key: key.clone(),
                ordinary_metadata_applied: false,
            }],
            completed: vec![outcome.clone()],
            uncertain: vec![],
        };
        for (name, bytes) in [
            ("key", serde_json::to_vec(&key).unwrap()),
            ("outcome", serde_json::to_vec(&outcome).unwrap()),
            ("report", serde_json::to_vec(&report).unwrap()),
        ] {
            let expected = match name {
                "key" => "07cd5d1be13c41ee5971e1f104d592a716363d9d10e636ea8888ba617c6e6195",
                "outcome" => "b5586b6a3297b43e78db6751f2a71378995de7fcbd7840bac26ad3c7db4874e5",
                "report" => "d9c14d9708e7a0ddee24aeac29d2576deee50d0a9494c1fe97740415324bffb7",
                _ => unreachable!(),
            };
            assert_eq!(blake3::hash(&bytes).to_hex().as_str(), expected, "{name}");
        }
        assert_eq!(
            serde_json::from_slice::<CreatedDirectoryEffectKey>(&serde_json::to_vec(&key).unwrap())
                .unwrap(),
            key
        );
        assert_eq!(
            serde_json::from_slice::<CreatedDirectoryDaclOutcome>(
                &serde_json::to_vec(&outcome).unwrap()
            )
            .unwrap(),
            outcome
        );
    }
    #[test]
    fn v8_wire_ids_are_exact_canonical_strings_and_roundtrip_full_u64() {
        for id in [
            1,
            42,
            9_007_199_254_740_992,
            9_007_199_254_740_993,
            u64::MAX,
        ] {
            let key = key(
                CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren,
                id,
            );
            let outcome = outcome(&key);
            let k = serde_json::to_value(&key).unwrap();
            let o = serde_json::to_value(&outcome).unwrap();
            assert_eq!(k["file_id"].as_str(), Some(id.to_string().as_str()));
            assert_eq!(o["file_id"].as_str(), Some(id.to_string().as_str()));
            assert_eq!(
                serde_json::from_value::<CreatedDirectoryEffectKey>(k).unwrap(),
                key
            );
            assert_eq!(
                serde_json::from_value::<CreatedDirectoryDaclOutcome>(o).unwrap(),
                outcome
            );
        }
        let mut report = CreatedDirectoryDaclReport::default();
        for (id, ordinary) in [
            (9_007_199_254_740_992, false),
            (9_007_199_254_740_993, true),
        ] {
            report.observe_created(CreatedDirectoryObservation {
                effect_key: key(
                    CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren,
                    id,
                ),
                ordinary_metadata_applied: ordinary,
            });
        }
        assert_eq!(report.created.len(), 2);
        println!(
            "V8_EXACT_ID_REPORT_JSON={}",
            serde_json::to_string(&report).unwrap()
        );
    }
    #[test]
    fn v8_wire_ids_reject_numeric_noncanonical_overflow_and_foreign_origins() {
        let modern_key = key(
            CreatedDirectoryOrigin::ExclusiveMinimalCreateBeforeChildren,
            42,
        );
        let outcome = outcome(&modern_key);
        for id in [
            serde_json::json!(42),
            serde_json::json!(9_007_199_254_740_993u64),
            serde_json::json!(u64::MAX),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(""),
            serde_json::json!("0"),
            serde_json::json!("00"),
            serde_json::json!("01"),
            serde_json::json!("+1"),
            serde_json::json!("-1"),
            serde_json::json!(" 1"),
            serde_json::json!("1 "),
            serde_json::json!("1.0"),
            serde_json::json!("1e3"),
            serde_json::json!("18446744073709551616"),
        ] {
            let mut k = serde_json::to_value(&modern_key).unwrap();
            let mut o = serde_json::to_value(&outcome).unwrap();
            k["file_id"] = id.clone();
            o["file_id"] = id.clone();
            assert!(
                serde_json::from_value::<CreatedDirectoryEffectKey>(k).is_err(),
                "{id}"
            );
            assert!(
                serde_json::from_value::<CreatedDirectoryDaclOutcome>(o).is_err(),
                "{id}"
            );
        }
        for origin in ["Unknown", "ExclusiveMinimalCreateAfterChildren"] {
            let mut k = serde_json::to_value(&modern_key).unwrap();
            k["origin"] = origin.into();
            let mut o = serde_json::to_value(&outcome).unwrap();
            o["origin"] = origin.into();
            assert!(serde_json::from_value::<CreatedDirectoryEffectKey>(k).is_err());
            assert!(serde_json::from_value::<CreatedDirectoryDaclOutcome>(o).is_err());
        }
        for field in ["origin", "directory", "file_id", "object_namespace"] {
            let mut k = serde_json::to_value(&modern_key).unwrap();
            k.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<CreatedDirectoryEffectKey>(k).is_err());
        }
        let mut extra = serde_json::to_value(&modern_key).unwrap();
        extra["foreign_origin"] = true.into();
        assert!(serde_json::from_value::<CreatedDirectoryEffectKey>(extra).is_err());
        let mut legacy =
            serde_json::to_value(key(CreatedDirectoryOrigin::ExclusiveMinimalCreate, 42)).unwrap();
        legacy["file_id"] = "42".into();
        assert!(serde_json::from_value::<CreatedDirectoryEffectKey>(legacy).is_err());
    }
}
