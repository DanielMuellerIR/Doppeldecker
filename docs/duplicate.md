# Lokal duplizieren

Stand: 2026-10-23.

„Duplizieren“ im Kontext-/Befehlsmenü und Cmd/Ctrl+D kopieren die aktuelle lokale
Auswahl in denselben Ordner. Die Vorschau zeigt für jedes Original den konkreten
freien Kopienamen. Erst „Duplizieren“ reiht diese Zuordnung in die Operations-Queue
des verursachenden Fensters ein. Dateien, Verzeichnisbäume und leere Ordner sind
unterstützt. Remote-Duplikate gehören noch nicht zu diesem Schnitt; das Backend
meldet diese Grenze.

## Gebundene Namen und Quellen

Das Backend erzeugt die Namen mit dem vorhandenen Vertrag für „Beide behalten“.
Vorhandene Namen und andere neue Duplikatnamen werden konservativ nach
Groß-/Kleinschreibung und Unicode-Normalisierung berücksichtigt. Das Frontend
erfindet weder Zielnamen noch Pfade: Es sendet die Auswahl zur Vorbereitung und
zur Bestätigung ausschließlich die zufällige `preparationId` zurück.

Die Vorbereitung hält den lokalen Root-Deskriptor sowie die Quellbelege im RAM,
gilt zehn Minuten und gehört nur diesem Fenster. Für Dateien und rekursive Bäume
werden die vorhandenen vollständigen Inhaltsbelege von „Beide behalten“ verwendet.
Das kann bei großen Auswahlen vor der Vorschau merklich dauern. Vor dem Enqueue
und erneut bei der Ausführung werden die Quellen und die angezeigten freien
Zielnamen geprüft. Beobachtete Drift verlangt eine neue Vorbereitung.
Abbruch, eine verspätete Antwort nach Navigation und der Abschluss der Bestätigung
verwerfen die exakte Vorbereitung. Eine alte ID entfernt keinen neueren Plan.
Beim Fensterschließen wird der Cache dauerhaft geschlossen; laufende ältere
Vorbereitungen dürfen danach keine Root-Handles mehr darin veröffentlichen.

## Ausführung und Grenzen

Ein Duplikat ist eine gewöhnliche persistierte Copy-Operation mit vollständigen
gebundenen Aliaszielen. Es verwendet denselben Transfer-, Temp-, Hash-,
No-Replace-, Flush-, Fortschritts- und Fehlervertrag wie „Beide behalten“.
Ein Queue-Neustart verliert weder die tatsächliche Zuordnung noch die Quellbelege.
Späte Kollisionen ersetzen keine fremden Dateien. Sandboxgrenzen gelten auch für
die tatsächlichen Aliasziele.

Die Ausnahme für Kopieren im eigenen Elternordner gilt nur für eine nichtleere
Auswahl direkter Kinder mit vollständig gebundenen neuen Namen und ohne Ersetzen.
Gewöhnliche Selbstkopien, unvollständige Aliaszuordnungen und Move zum Original
bleiben abgelehnt. Der Schnitt verwendet die bestehende atomare lokale
„Beide behalten“-Klasse; ein zusätzlicher Best-Effort-Duplizierweg ist noch offen.
Nicht sicher darstellbare Namen oder nicht vollständig belegbare Quellen werden
sichtbar abgelehnt. Direkt ausgewählte Symlinks und Spezialobjekte werden nicht
dupliziert; innerhalb unterstützter Ordner folgt ihre Auslassung der gewöhnlichen
Copybilanz. Der Schnitt erteilt keine zusätzliche Zusage zum Erhalt nativer
Metadaten und keine atomare Momentaufnahme aller Quellinhalte.

Abbruch der Vorschau erzeugt keinen Queueeintrag. Eingereihte Kopien werden über
die bestehende Queue pausiert, abgebrochen und bilanziert; die Originale bleiben
erhalten. Der Stapel ist keine gemeinsame Transaktion, bereits abgeschlossene
Kopien bleiben bei einem späteren Fehler bestehen.
