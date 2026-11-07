# Papierkorb-Drift und Abschluss

Stand: 2026-10-19. Ausgangsstand: `0077e84`.

Der ältere Querschnittskandidat `trash-journal-drifted-abschluss` beschreibt noch
das v1-Verfahren. Dessen Erzeuger steht inzwischen unter `#[cfg(test)]` in
`core/src/trash/legacy.rs`. Die produktive Plattformanbindung
`core/src/trash/native.rs` erzeugt ausschließlich `journal_v2::claim::ClaimRun`.
Die Annahme, heutige Papierkorbläufe sammelten ständig neue terminale
v1-Driftjournale an, gilt daher nicht mehr.

## Heutiger Abschluss

Die v2-Recovery prüft einen `planned`-Eintrag über die gehaltene Quelle und den
privaten Bereich. Nur wenn der private Claim nachweislich fehlt, setzt sie den
Eintrag auf `untouched`. Vor dem Entfernen des eigenen Bereichs und Journals
bleiben Marker-, vollständige Inhalts-, Identitäts- und Durabilityprüfungen
verbindlich. Ein verschwundener oder ersetzter Quellname ist keine Erlaubnis,
eine fremde Datei zu löschen oder in den Papierkorb zu verschieben.

Ein ergänzter Regressionstest durchläuft beide tatsächlichen Dateifälle:
ursprünglicher Name fehlt und ursprünglicher Name enthält eine fremde
Ersatzdatei. Der ursprüngliche Inhalt bleibt unter einem künstlichen Haltenamen
erhalten; Bytes und Inode der Ersatzdatei ändern sich nicht. Bereich und Journal
werden abgeschlossen, Papierkorbdaten bleiben leer. Der erste Recoverylauf zählt
einmal Drift, der zweite meldet weder Drift noch Warnung oder Recoverybedarf.
Der Test benutzt den produktiven v2-Parser, Wiederöffnungs- und Abschlussweg.
Er korrigiert keine Produktlogik und behauptet keinen neuen roten Produktbefund.

## Absichtliche Altaufbewahrung

Ein bestehendes v1-Journal mit `Drifted` bleibt erhalten und liefert bei erneutem
Recovery denselben Hinweis. Der vorhandene Legacy-Dateitest belegt genau diesen
Vertrag; der weitere Gegenfall bindet den Driftcommit trotz ausgetauschtem Pfad
an den gehaltenen Journalordner. Das ist ein fester Altbestand ohne heutigen
v1-Erzeuger. Unbekannte oder ungeklärte Claims bleiben Recoveryblocker.

Eine leere Warnungsliste ist absichtlich keine Voraussetzung für
`RecoveryStats::is_clean`: Terminale Drift ohne Datenclaim ist kein offener
Wiederherstellungsauftrag. Weder automatische Alterslöschung noch eine pauschale
Umdeutung von `is_clean` in Löschautorität wird eingeführt. Ein später ausdrücklich
beauftragtes Quittieren von Altbelegen müsste einen eigenen gebundenen Vertrag
besitzen. Der obsolete Wachstumskandidat wird deshalb begründet verworfen; die
verbleibende Altaufbewahrung wird ausdrücklich benannt.

## Verifikation

Vier gezielte macOS-Fälle liefen nach exakt einem Listentreffer mit `--exact`
grün: neuer v2-Driftabschluss, beibehaltene terminale v1-Drift, gebundener
Legacy-Driftcommit und eine fremde Kollision beim Bereichsabschluss. Danach
bestehen die vollständigen Core-Suiten mit 1143 Tests auf macOS und 1165 auf Linux;
je 22 umgebungsgebundene Fälle bleiben ignoriert. Workspace-Clippy mit allen
Targets und `-D warnings` sowie Formatprüfung sind beidseitig grün. 385
prüfrelevante Dateien blieben während dieser Läufe hashgebunden unverändert.
Die früheren CLI-, App-, Fuzz- und Browserbelege werden nur für ihre bytegleichen
Quellen referenziert und nicht als erneut ausgeführt ausgegeben.

Die einzige ausführbare Änderung ist der zusätzliche Regressionstest in
`core/src/trash/journal_v2/claim/lifecycle_tests.rs`; der produktive Core-, App-,
CLI- und Frontendcode bleibt bytegleich. Keine Dateiformatänderung, neue
Bedienfläche oder Löschfunktion. Keine erneute native GUI-Abnahme beansprucht.
