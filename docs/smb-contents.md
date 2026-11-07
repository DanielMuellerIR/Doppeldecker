# Additive SMB-Downloads

Stand: 2026-11-01.

Gewöhnliche additive SMB-Kopien in einen lokalen Ordner verwenden eine private
Lesequelle für direkte Mehrfachauswahl, reguläre Dateien und Verzeichnisbäume.
Ganze mehrdeutige Case-/NFC-Namensgruppen werden ausgelassen; unabhängige Inhalte
bleiben übertragbar. Ausgelassene Ordner werden nicht betreten. Beobachtete
Reparse-Punkte werden als Symlinks behandelt. Ersetzen, Move, Remote-Ziele,
Snapshots und strikter Sync behalten ihre bisherigen Quellenverträge.

## Umfang und Inhaltsbeleg

Vollständige Listings binden Namen, Attribute und Provider-Tokens innerhalb der
ausgewählten Bäume. Ausgelassene Mitglieder und ihre beobachteten Vorfahren
bleiben vor weiteren Wirkungen driftgebunden. Außerhalb der Auswahl beeinflussen
unselektierte Geschwister weder Vergleich noch Digest. Benötigte übergeordnete
Ordner werden bei verlässlicher Nichtnull-Dateisystem-ID an Name, Typ,
Reparse-Status und diese ID gebunden. Veränderliche synthetische CreationTime
ist dort kein Identitätswechsel. Fehlt die ID, bleiben die vollständigen
Attribut- und Tokenfakten als schwächerer Beleg erhalten.

Der Inhaltsstrom prüft dieselben vorbereiteten Dateifakten vor dem ersten Byte
und nach tatsächlichem EOF am gehaltenen Handle. Die Dateisystem-ID stammt aus
`FILE_INTERNAL_INFORMATION`; die kurzlebige Handle-ID aus CREATE ist dafür kein
Ersatz. Größe, Typ, Zeitwerte und ChangeTime bleiben ebenfalls gebunden.
Die im Verzeichnislisting fehlende Hardlink-Anzahl wird am ersten Handle ergänzt
und bis EOF geprüft; ein zusätzlicher Handlewert ist allein keine Dateidrift.
Ein zusätzlicher READ jenseits der erwarteten Länge erkennt Wachstum; ein
vorzeitiger EOF erkennt Verkürzung. Positive kurze READ-Antworten bleiben
zulässig und setzen am tatsächlichen Offset fort; sie sind bei `MinimumCount=0`
kein Nachweis einer Verkürzung. [SMB2-READ-Verarbeitung](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/21e8b343-34e9-4fca-8d93-03dd2d3e961e).
Frische benötigte Eltern- und
Auslassungsprüfungen sowie der erfolgreiche Handleabschluss gehen dem lokalen
Temp-Commit voraus. Fehler und Reconnect starten keinen zweiten Inhaltsversuch.

Die Fensterqueue erhält Digest und Auslassungsbilanz. Veränderte Gruppen
verlangen nach Neustart erneut Zustimmung. Eine aktive Vorbereitung bindet die
ursprüngliche Sitzungsgeneration und Serveridentität; die Generation steht nicht
im restartstabilen Digest.

## Temp-Recovery und Grenzen

Queue-Downloads nutzen bevorzugt den privaten lokalen Tempindex. Bereits
gebundene Temps bleiben nach einem unterbrochenen Lauf konkrete Prüfpfade;
sie erteilen kein Lösch- oder Wiederholungsrecht. Fehlt ein exklusiver
Queue-Namespace, bleibt die gewöhnliche Kopie möglich und meldet die fehlende
dauerhafte Temp-Recovery. Die verpflichtende SFTP-Klasse bleibt bestehen.

Der Quellenhinweis benennt Best Effort: Gleichzeitige Pfadänderungen zwischen
Elternprüfungen können unbemerkt bleiben. Bei Dateisystem-ID 0 sind Datei und
Vorfahren nur attributgebunden; ein Austausch mit identischen gemeldeten Fakten
ist nicht erkennbar. Auch eine Nichtnull-ID friert Dateiinhalte nicht ein, wenn
der Server eine Inhaltsänderung in keinem gebundenen Attribut sichtbar macht.
Der Baumbeleg braucht Speicher proportional zum beobachteten Bestand. Es gibt
keine neuen pauschalen Datei-, Auswahl- oder Baumtiefengrenzen.

Der Gegenfall für die zunächst fehlende Linkzahl reproduziert die frühere
Fehlabsage. Nach der Korrektur bestehen der vollständige Samba-Gegenlauf mit
Copy/Move, Reconnect und Queue sowie ein nativer Linux-Tauri-F5-Lauf: bytegleicher
Zielinhalt, unveränderte Quelle und bestätigter Queueabschluss. Geänderte Linkzahl,
Datei-ID oder ChangeTime bleiben weiterhin Drift.
