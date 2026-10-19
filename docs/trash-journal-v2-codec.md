# Papierkorb-Journal v2: strikter Codec

Stand: 2026-10-07. Der produktive Recovery-Einstieg liest v2 und bindet die
belegten privaten Layouts frisch. Ein gültiges Dokument allein autorisiert keine
Mutation; fehlende oder veränderte Bindungen erhalten Journal und Daten zur Prüfung.
Neue macOS- und Linux-/XDG-Papierkorboperationen schreiben v2. Der v1-Schreibweg
existiert ausschließlich für Regressionstests; produktive Altjournal-Recovery
bleibt konservativ gebunden.

## Daten und Grenzen

`core/src/trash/journal_v2.rs` bildet den
[privaten Claim-Vertrag](trash-toctou-design.md#zielvertrag-für-private-claims-journal-v2)
ab. Namen und absolute Pfade sind JSON-Arrays aus unveränderten Bytes. Dadurch
bleiben Nicht-UTF-8-Namen und wörtliche Prozentsequenzen erhalten. Keine
`to_string_lossy()`-Umwandlung liefert eine Wiederherstellungsidentität.
Namen haben höchstens 255 Bytes; Pfade höchstens 4096 Bytes und enthalten keine
NUL-, Punkt- oder Elternkomponenten. Ein Dokument hat höchstens 4 MiB und 10 000
Einträge. Unbekannte Felder, Plattformen und Phasen sind Lesefehler.

Das Dokument enthält Version, 128-Bit-Laufkennung, effektive UID, Journal-Elternstand,
optionale Sandbox-Bindung und privaten Anlageintent, Anker/Bereich, Markeridentität/Nonce/Digest, Trash-/Info-Verzeichnisse und
Einträge. Verzeichnisstände speichern Objektidentität, Mountbeleg, Owner, Modus und
ACL-Abwesenheit beziehungsweise Digest oder ausdrücklich unbekannten ACL-Stand.
Daten- und Info-Verzeichnis sowie Quellen und privater Anker müssen demselben
aufgezeichneten Mount angehören. Journal und Daten dürfen auf unterschiedlichen
Mounts liegen.

Der persistierte Mountbeleg ist von `LocalMountIdentity` getrennt, dessen IDs nur
für vorbereitete Läufe gelten. macOS zeichnet FSID und Volume-ID auf; Linux
Boot-ID, Mount-ID, deren Unique-Status und FSID. Diese Angaben sind Belege für die
spätere erneute Bindung, keine alleinige Berechtigung. Pfadstrings, passende
Inoden oder ein gespeicherter Modus beweisen keinen heute noch privaten Namensraum.
Der [gemeinsame Claim-Kern](trash-private-claim.md) prüft Marker, Owner, Rechte,
ACL und Mount frisch über gehaltene Deskriptoren. Die spätere Recovery muss diese
Bindungen erneut herstellen. Fehlender oder widersprüchlicher Beleg bleibt manuell.

## Phasen und Paarung

Der Stapel kennt `planned`, `area_ready`, `cleanup_intent` und `settled`.
Die einzelnen Einträge kennen Claim-, Info-, Transfer- und Restore-Intents samt
beobachteten Abschlüssen sowie `drifted` und `unclear`. Ein Claim-Abschluss braucht
seine tatsächliche Identität. Eine abweichende Identität ist ausschließlich bei
Drift, unklarem Ausgang oder der Rücksetzung dieses tatsächlichen Claims gültig;
sie darf nicht als passender Transfer gespeichert werden. Cleanup benötigt
`trashed`, `restored` oder `untouched` für sämtliche Datenaktionen. `untouched`
belegt eine nicht begonnene Claim-Wirkung und besitzt weder Claim-Identität noch Info.

XDG-Info ist an privaten und veröffentlichten Namen, reguläre Dateiidentität,
Größe und Digest gebunden. Der veröffentlichte Name muss exakt aus dem Trashnamen
und `.trashinfo` bestehen und ebenfalls innerhalb der Namensgrenze bleiben.
Info-Publish, Info-Abschluss und Daten-Transfer brauchen diesen Datensatz.
macOS besitzt keine XDG-Infophase. Private Claim- und Infonamen sowie der Marker
kollidieren nicht; gleiche Originalnamen in verschiedenen Elternordnern sind
möglich, derselbe Originaleintrag wird aber nicht doppelt aufgenommen.

Der Codec prüft diese logischen Grenzen; er schreibt kein Journal und beweist
keine bereits erfolgte fsync- oder Rename-Wirkung. Der
[gemeinsame Kern](trash-private-claim.md) führt sichere Anlage, Transfer, Rücksetzung,
Recovery und Cleanup über gehaltene Handles aus. Ein Anlageintent liegt bereits
vor mkdir dauerhaft vor. Dokumente ohne die neuen optionalen Felder bleiben lesbar;
fehlende Berechtigungsbelege werden bei der Recovery nicht erfunden. Die konservative
v1-Recovery setzt nur aus einem heute belegten privaten Quellordner zurück.

## Prüfung

Die Codec-Gegenläufe prüfen bytegleichen Roundtrip, maximale Namen, fehlerhafte
Pfade, unbekannte verschachtelte Felder, veränderte Marker/UID/Mountbelege,
doppelte Namen, fehlende Claim-Identität, Drift und XDG-Paarung. Der produktive
Recovery-Einstieg liest ein synthetisches gültiges v2-Journal zweimal; beide Läufe
lassen Journal und Testdaten bytegleich und melden keinen Restore oder Abschluss.
Die Belege im Codec sind synthetische Dokumente. Echte frische Layoutbindungen,
Crashphasen und Dateiwirkungen werden getrennt im gemeinsamen Kern geprüft.
Die native XDG-Adapterprüfung umfasst Btrfs-, tmpfs- und ext4-Mounts; die
macOS-GUI-Abnahme verwendet einen künstlichen Papierkorb. Belege und die weiterhin
separate Linux-Releasekandidatenabnahme stehen im [Claim-Vertrag](trash-private-claim.md).
