# ZIP-Archive entpacken

Stand: 2026-11-01. Ganze lokale ZIPs und geprüfte entfernte Momentaufnahmen,
eine darin gewählte reguläre Datei oder ein virtueller Unterbaum lassen sich
in die lokale oder entfernte Gegenpane entpacken. Die Archivquelle bleibt erhalten.

## Bedienung

**ZIP entpacken …** im Befehls-/Kontextmenü prüft zunächst Umfang, Ziel und
Auslassungen. **Entpacken** startet die Fensterqueue. Im Archivbrowser gelten:

| Aktion | Zielabbildung |
| --- | --- |
| Ganzes Archiv entpacken | Gesamter bekannter Katalog, unabhängig von der markierten Zeile |
| Datei entpacken | `A/B/Text.txt` wird zu `Text.txt` |
| Geöffneten Ordner entpacken | `A/B` wird zu `B` samt Unterbaum, auch wenn leer |

Datei-/Ordner-IDs stammen aus dem Backendkatalog. Lokale Archivänderungen oder
veränderte Momentaufnahmen verlangen erneutes Öffnen. Entfernte Quellen werden
vollständig als geprüfte [Momentaufnahme](archive-read.md) geladen; bestätigt
werden diese Bytes, ohne späteres Nachladen vom Server.

## Inhalte, Konflikte und Grenzen

Vorhandene Ordner werden zusammengeführt; zusätzliche Zielinhalte bleiben stehen.
Reguläre Dateikonflikte bieten Ersetzen, Überspringen und Abbrechen. Auf Remotezielen
verwendet der anschließende Upload den gewöhnlichen Kopierdialog samt Beide behalten.
Zustimmungen binden die beobachteten Ziele; Symlinks, Spezialziele und
Datei-/Ordnertausch werden nicht still ersetzt.

Unsichere Namen, Links, Spezialtypen, verschlüsselte oder nicht unterstützte Inhalte
werden pfadgenau ausgelassen. Duplikate, Case-/Unicode-Aliase und Datei-/Ordnerkonflikte
werden katalogweit als mehrdeutige Gruppen ausgeschlossen, auch vor einer Einzelwahl.
Unabhängige Inhalte bleiben nutzbar. Eigene Archivquelle und Identitätsaliase bleiben geschützt.
Ein Teilkatalog braucht **Bekannten Teil entpacken**; sein unbekannter Rest gilt nie
als vollständig. Der Ausführungsplan stammt aus dem Core, nicht aus sichtbaren Browserzeilen.

CRC, gebundene Größen und frische Ziel-/Identitätsprüfungen gelten vor dem Commit.
Der zusätzliche Entpackindex ist auf 64 MiB und 50.000 Knoten begrenzt, einschließlich
Elternnamensindizes; das ist kein Gesamtprozesslimit. Kataloggrenzen gelten zusätzlich.
Platzmangel bleibt auch während des Streams ein sichtbarer Fehler.

Dies ist ein portabler Inhaltstransfer. Owner, Rechte/Sonderbits, Zeitstempel,
ACLs, xattrs, Resource Forks, Sparse- und Hardlinkbeziehungen werden nicht aus ZIPs
wiederhergestellt. Weitere Archivformate und freie Mehrfachauswahl bleiben spätere Wünsche.

## Queue und Teilwirkungen

Lokales Entpacken bereitet bei jedem Claim frisch vor; Drift verlangt neue Zustimmung.
Snapshotclaims brauchen den bekannten Store und eine frische Lease. Cachepfade,
Handles, Secrets und Permits sind keine Freigaben im Journal.

Remoteziele verwenden den zweiphasigen [Archivupload](remote-pack.md): zuerst lokal
entpacken und versiegeln, dann dieselben Ergebnisbytes hochladen. Die Queue zeigt
Archivhinweise, Zwischenstand und Upload getrennt. Konfliktpause und Neustart bauen
die Quelle nicht neu auf. Journalversion 40 ergänzt diese Phase; bekannte ältere
Journale werden migriert, ältere Programme lehnen das neue Format ab.

Abbruch stoppt weitere Wirkungen. Fertige Dateien und angelegte Ordner bleiben;
nur nachweislich eigene offene Temps werden bereinigt. Unbelegte Reste, CRC-Fehler
und unklare Commitantworten erhalten Recoverypfade. Teilbilanzen unterscheiden
fertige, übersprungene, fehlgeschlagene und offene Wirkungen. Entpacken ist keine
Gesamttransaktion; laufend abgestürzte Aufträge werden nicht blind wiederholt.
Siehe [Restbereinigung](decision-lokale-restbereinigung.md).

Plattformbelege für die bisherigen lokalen Wege stehen in den Roadmapständen
[2026-10-23](roadmap-2026-10-23.md) und [2026-10-24](roadmap-2026-10-24.md).
Aktuelle Remote- und GUI-Prüflücken: [Releaseplan](release-plan.md).
