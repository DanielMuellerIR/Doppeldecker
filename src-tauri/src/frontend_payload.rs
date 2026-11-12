//! Anzeigeprojektion: alte Journalbytes bleiben erhalten, Objekt-IDs im IPC exakt.
use serde::{Serialize, Serializer};
use serde_json::Value;

#[derive(Clone)]
pub(crate) struct FrontendPayload<T>(pub(crate) T);
pub(crate) type FrontendQueueSnapshot = FrontendPayload<doppeldecker_core::QueueSnapshot>;

impl<T> From<T> for FrontendPayload<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}
impl<T: Serialize> Serialize for FrontendPayload<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.0).map_err(serde::ser::Error::custom)?;
        project_reports(&mut value);
        value.serialize(serializer)
    }
}

fn project_reports(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if let (Some(source), Some(paths)) = (
                fields.get("generated_source"),
                fields.get("temp_recovery_paths").and_then(Value::as_array),
            ) {
                let id = source.get("id").and_then(Value::as_str).unwrap_or("");
                let root = fields
                    .get("operation")
                    .and_then(|op| op.get("source"))
                    .and_then(|source| source.get("root"))
                    .and_then(Value::as_str);
                let available = !id.is_empty()
                    && paths.iter().filter_map(Value::as_str).any(|path| {
                        let path = std::path::Path::new(path);
                        root.is_some_and(|root| {
                            path == std::path::Path::new(root)
                                || Some(path) == std::path::Path::new(root).parent()
                        }) || path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with(".cleanup-") && name.ends_with(&format!("-{id}"))
                            })
                    });
                fields.insert("generated_source_recovery".into(), Value::Bool(available));
            }
            for (name, child) in fields {
                if name == "created_directory_dacl_report" {
                    project_report(child);
                } else {
                    project_reports(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(project_reports),
        _ => {}
    }
}
fn project_report(value: &mut Value) {
    if let Some(created) = value.get_mut("created").and_then(Value::as_array_mut) {
        for item in created {
            if let Some(key) = item.get_mut("effect_key") {
                project_object_id(key);
            }
        }
    }
    for name in ["completed", "uncertain"] {
        if let Some(outcomes) = value.get_mut(name).and_then(Value::as_array_mut) {
            outcomes.iter_mut().for_each(project_object_id);
        }
    }
}
fn project_object_id(value: &mut Value) {
    // serde_json::Number hält das ursprüngliche u64 noch exakt. Erst der
    // JavaScript-Parser würde runden; deshalb vor dieser Grenze umwandeln.
    if let Some(id) = value.get_mut("file_id") {
        if let Some(number) = id.as_u64() {
            *id = Value::String(number.to_string());
        }
    }
}
pub(crate) fn serialize_report<T: Serialize, S: Serializer>(
    report: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut value = serde_json::to_value(report).map_err(serde::ser::Error::custom)?;
    project_report(&mut value);
    value.serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use doppeldecker_core::created_directory_dacl::{
        CreatedDirectoryDaclOutcome, CreatedDirectoryDaclReport, CreatedDirectoryEffectKey,
        CreatedDirectoryObservation, CreatedDirectoryOrigin,
    };

    #[test]
    fn generated_recovery_projection_recognizes_content_workspace_and_claim() {
        for (path, expected) in [
            ("/store/generated-id/content", true),
            ("/store/generated-id", true),
            ("/store/.cleanup-queue-generated-id", true),
            ("/unrelated/temp", false),
        ] {
            let value = serde_json::json!({"entries": [{"generated_source": {"id": "generated-id"},
                "operation": {"source": {"root": "/store/generated-id/content"}},
                "temp_recovery_paths": [path]}]});
            let wire = serde_json::to_value(FrontendPayload(value)).unwrap();
            assert_eq!(wire["entries"][0]["generated_source_recovery"], expected);
        }
    }

    fn report() -> CreatedDirectoryDaclReport {
        let key = CreatedDirectoryEffectKey {
            origin: CreatedDirectoryOrigin::ExclusiveMinimalCreate,
            directory: "/ziel/neu".into(),
            file_id: 9_007_199_254_740_992,
            object_namespace: [1; 32],
        };
        let outcome = CreatedDirectoryDaclOutcome {
            origin: key.origin,
            directory: key.directory.clone(),
            file_id: key.file_id + 1,
            object_namespace: key.object_namespace,
            before_digest: [2; 32],
            after_digest: [3; 32],
        };
        CreatedDirectoryDaclReport {
            created: vec![
                CreatedDirectoryObservation {
                    effect_key: key.clone(),
                    ordinary_metadata_applied: false,
                },
                CreatedDirectoryObservation {
                    effect_key: CreatedDirectoryEffectKey {
                        file_id: key.file_id + 1,
                        ..key
                    },
                    ordinary_metadata_applied: true,
                },
            ],
            completed: vec![outcome.clone()],
            uncertain: vec![CreatedDirectoryDaclOutcome {
                file_id: u64::MAX,
                ..outcome
            }],
        }
    }

    #[test]
    fn ipc_reports_keep_adjacent_large_ids_exact_without_changing_journal_numbers() {
        let report = report();
        let persisted = serde_json::to_value(&report).unwrap();
        assert_eq!(
            persisted["created"][1]["effect_key"]["file_id"].as_u64(),
            Some(9_007_199_254_740_993)
        );
        let payload = serde_json::json!({
            "entries": [{ "created_directory_dacl_report": report,
                "progress": { "created_directory_dacl_report": report },
                "operation": { "unrelated_file_id": 42 } }]
        });
        let wire = serde_json::to_value(FrontendPayload(payload)).unwrap();
        for path in [
            "/entries/0/created_directory_dacl_report",
            "/entries/0/progress/created_directory_dacl_report",
        ] {
            let projected = wire.pointer(path).unwrap();
            assert_eq!(
                projected["created"][0]["effect_key"]["file_id"],
                "9007199254740992"
            );
            assert_eq!(
                projected["created"][1]["effect_key"]["file_id"],
                "9007199254740993"
            );
            assert_eq!(projected["completed"][0]["file_id"], "9007199254740993");
            assert_eq!(projected["uncertain"][0]["file_id"], "18446744073709551615");
        }
        assert_eq!(wire["entries"][0]["operation"]["unrelated_file_id"], 42);
        if let Some(destination) = std::env::var_os("DOPPELDECKER_TEST_FRONTEND_REPORT_FIXTURE") {
            std::fs::write(destination, serde_json::to_vec(&wire).unwrap()).unwrap();
        }
        assert_eq!(serde_json::to_value(report).unwrap(), persisted);
    }

    #[test]
    fn command_errors_and_events_use_the_same_exact_projection() {
        let mut error = crate::AppError::new("plan_drift", "Test");
        error.created_directory_dacl_report = Some(Box::new(report()));
        let direct = serde_json::to_value(&error).unwrap();
        let event =
            serde_json::to_value(FrontendPayload(serde_json::json!({"payload": error}))).unwrap();
        assert_eq!(
            direct["created_directory_dacl_report"]["created"][1]["effect_key"]["file_id"],
            "9007199254740993"
        );
        assert_eq!(event["payload"], direct);
    }
}
