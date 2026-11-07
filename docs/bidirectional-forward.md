# Bidirektionaler Best-Effort-Forwardlauf

Stand: 2026-10-29. Dieser gesonderte Core-Einstieg ergänzt den bestehenden
[rollbackfähigen Vertrag](bidirectional-sync-design.md). Die eigene Fensterqueue
ist Teil dieses Vertrags. Die GUI verwendet ihn bei Paaren mit mindestens einem
Remote-Endpunkt. Die CLI bietet bidirektionale Läufe weiterhin nur lokal an;
gewöhnlicher Einwegsync ist dort für Remote-Endpunkte verfügbar.

`bidirectional::forward::prepare_forward_bidirectional` verwendet dieselbe
Baseline, Drei-Wege-Planung, Konfliktentscheidung, Tombstones und schreibgeschützte
Projektion wie der vorhandene bidirektionale Sync. Transfers, Inhaltsprüfung,
Ersetzung und Löschung laufen durch die gemeinsame ContentSync-Engine.
Die Forwardklasse ist ausdrücklich `best_effort_forward`; sie verspricht keinen
universellen Rollback. Lokale Ersetzungen und Entfernungen behalten die verfügbare
Quarantäne. Fehlende entfernte Wiederherstellung und irreversible Wirkungen stehen
in den einzelnen Wirkungsbilanzen. Dateiinhalte werden immer gehasht.

Neue Dateien und Ordner, reguläre Dateiersetzungen und Löschpropagierung verwenden
die stärkste verfügbare ContentSync-Wirkung. Datei-/Ordner-Typwechsel werden
pfadgenau übersprungen; die Baseline des Unterbaums bleibt unverändert. Andere
Konflikte müssen frisch entschieden werden. Ein `KeepBoth` verwendet weiterhin die
vom bestehenden Konfliktplaner gebundenen Namen und Projektionen.

## Journal und Bestätigung

Die vorhandenen Strictjournale bleiben byte- und versionsverträglich und behalten
ihren Resume-/Rollbackvertrag. Ein offenes Strictjournal sperrt den Forwardstart.
Die Forwardklasse schreibt daneben ein eigenes `<baseline>.forward.json` unter
derselben Prozesssperre der Baseline. Ein aktiver Forwardauftrag sperrt seinerseits
alle alten Starts. Jede Änderung dieses Journals erhält einen kanonischen Digest,
einen atomaren Rename und Datei-/Eltern-fsync. Der Verlauf früherer Aufträge bleibt
im selben begrenzten Journal erhalten; die Grenze bleibt 256 MiB.

Vor den ersten lokalen Quarantäne-/Restoreproben eines neuen mutierenden Plans wird
ein vorbereiteter Forwardintent gespeichert. `discard` darf ausschließlich diesen
eigenen noch nicht ausgeführten Auftrag verwerfen. Für eine bestehende Recovery
verändert ein Abbruch der Vorschau den alten Auftrag nicht.

Die Bestätigung bindet beide vollständigen, frischen Bäume, die echte dauerhafte
Endpunktidentität, B0, den Plan und die konkreten Wirkungsbilanzen. Das Einmalrecht
verfällt nach zehn Minuten. Große dauerhafte Entfernungen behalten die von
ContentSync verlangte exakte Phrase. Vor jeder Projektion werden beide Bäume und
Endpunktbindungen gegen die bestätigten Vorwirkungen geprüft. Eine schwächere
Wiederherstellbarkeit oder veränderte Transferwirkung verlangt neue Bestätigung.

Die Oberfläche bilanziert sämtliche Teilwirkungen vor der Zustimmung. Dauerhafte
Ersetzungen und Entfernungen stehen getrennt von lokaler Quarantäne und gesperrten
Entfernungen. Auch große Konfliktpläne behalten alle Teilwirkungen und konkreten
Pfade in aufklappbaren, per Tastatur erreichbaren Listen. Konfliktkopien zeigen
beide gebundenen Namen und Seiten. Bekannte Metadatenauslassungen nennen je
Zielwirkung Pfad, Merkmal und technischen Grund.

## Teilfehler und Recovery

Ein Teilfehler oder Prozessabbruch lässt B0 unverändert und den aktiven Auftrag
bestehen. Eine laufende Mehrdatei-Projektion darf bereits einzelne Dateien verändert
haben. Die Klasse behauptet deshalb weder ein vollständiges Projektionspräfix noch
einen automatisch wiederholbaren Schritt.

Recovery muss ausdrücklich angefordert werden. Sie liest beide Bäume vollständig
neu, zeigt beobachtete Pfadänderungen seit dem alten Auftrag und dessen noch
laufende Projektionen. Diese Beobachtung unterscheidet bekannte aktuelle Inhalte
von einem möglicherweise unklaren ursprünglichen Serverausgang. Es gibt keinen
mutierenden Retry. Ein neuer Drei-Wege-Plan wird gegen die unveränderte letzte
gemeinsame B0 berechnet; alte Konfliktentscheidungen werden nicht übernommen.
Erst die frische Wirkungsbestätigung archiviert den alten aktiven Auftrag und
ersetzt ihn dauerhaft durch den neuen Auftrag.

Nach vollständiger Ausführung folgen zwei stabile Vollscans. Erst gleiche,
nicht übersprungene Inhalte dürfen die echte gemeinsame B1 atomar fortschreiben.
Ein Fehler nach deren Commit wird ausschließlich durch
`finish_forward_bidirectional_completion` abgeschlossen. Dieses liest die exakt
journalisierte Baseline und wiederholt ihre Dauerhaftigkeitsbarrieren; es
wiederholt keine Dateiwirkung. Manuelles Journallöschen und aus einem Teilstand
abgeleitete Zwischenbaselines sind kein Recoveryweg.

## Fensterqueue

Queuejournal 34 besitzt die eigene Variante `BidirectionalForward`; ein Dokument
bis Version 33 darf diese Variante nicht einschleusen. Die vorhandene Variante
`Bidirectional` bleibt vollständig rollbackfähig und unverändert abgegrenzt.

Die Forwardqueue speichert Endpunkte samt Profilreferenzen, B0, Plan-, Konflikt-
und Vorschau-Digest. Der Queueowner wird vor den Projektionen und deren privaten
Remote-Tempintents gebunden. Ein bereits bestätigter RAM-Plan wird an genau den
dazugehörigen geclaimten Eintrag gebunden; dafür ist kein zweiter Dialog nötig.
Spätere Queueclaims ohne diesen RAM-Zustand benötigen stets eine frische Vorschau.
Nach einer neuen Bestätigung darf derselbe Fensterworker den Auftrag ausführen;
`Pending` persistiert kein Permit. Ein Prozessneustart mit offenem Forwardjournal
verwandelt auch einen solchen Pendingstand in einen sichtbaren Recoveryfall.

Veraltete Konfliktentscheidungen werden bei einem abweichenden Konflikt-Digest
verworfen. Neue Entscheidungen müssen den Digest der frisch gehaltenen Snapshots
angeben. Erst die endgültige Wirkungsbestätigung schreibt diese Entscheidungen
und Digests in den Auftrag. Pause und Stop-on-error bleiben Fensterqueuezustände;
ein Teilabbruch ist `Interrupted`, lässt B0 unverändert und wird nicht automatisch
wiederholt. Generischer Retry, Fehlerabschluss und manuelles Anerkennen eines
unklaren Ausgangs können diesen Recoveryvertrag nicht umgehen.

Nach B1 hält die Queue den Coreabschluss bis zum dauerhaften terminalen
Queuecommit fest. Ein Crash zwischen beiden Schritten bietet ausschließlich den
mutationslosen Abschluss an. Der gemeinsame Ergebnis-Typ zeigt
`execution_class: "best_effort"`; die konkrete Forwardklasse bleibt zusätzlich in
Preview, Operation und Journal bezeichnet. Das Fenster-Audit schreibt Start,
Unterbrechung und Abschluss dieser Klasse ohne Secrets.

## Nachweis

Automatisierte macOS-Gegenläufe prüfen einen echten Prozessabbruch nach dem ersten
Dateicommit innerhalb einer Mehrdatei-Projektion. Im lokalen Core und über echte
HTTPS-/WebDAV-Pakete bleiben B0 und das offene Journal bestehen; der frisch
bestätigte Restlauf bildet B1 und der Folgelauf bleibt leer. Der WebDAV-Gegenlauf
läuft zusätzlich durch die echte Queueowner-/Reopen-/Recovery-Pipeline mit Audit.
Ein echter SFTP-Kindprozessgegenlauf bestätigt dieselbe Grenze nach dem ersten
Dateicommit, die unveränderte B0, frische Zustimmung, gemeinsame B1 und leeren
Folgelauf. WebDAV- und Librarygegenläufe bestehen ebenfalls unter Linux.
Lokale Tests prüfen Quarantäne und Tombstones, ausgelassene Typwechsel mit
unveränderter Unterbaumbaseline, RAMverlust nach Pending-Freigabe, neue
Konfliktentscheidungen nach Drift sowie den mutationslosen Queueabschluss nach B1.
Die native GUI und weitere echte bidirektionale Providerkombinationen benötigen
ihre eigene Abnahme; Browserfälle ersetzen diese Prüfung nicht.
