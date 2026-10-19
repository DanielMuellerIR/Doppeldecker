# Gewöhnliche WebDAV-Ordneranlage

Stand: 2026-10-18. GUI „Neuer Ordner“ und CLI `mkdir` verwenden eine eigene
additive Ausführungsklasse. Eine rekursive Löschfähigkeit oder künstliche
Schreib-/Löschprobe ist dafür nicht erforderlich. Strikter Ziel-Sync und allgemeine VFS-Mutationen bleiben
ohne gemeinsame Root-/Elternbindung passiv gesperrt;
[Mutationsvertrag](webdav-mutation-boundary.md).

## Bestätigung und Wirkung

Der Core liefert vor der Namenseingabe die Klasse `best_effort` und die konkreten
Einschränkungen. Das Frontend zeigt sie im Namensdialog und übergibt die angezeigte
Klasse an das Backend; eine abweichende Klasse scheitert vor Providerzugriff mit
`plan_drift`. Das CLI zeigt dieselben Hinweise vor der Anlage und liefert danach
`execution_class`, `path` und `limitations` im JSON-Ereignis `mkdir`.

Syntaxprüfung erfolgt vor Profilauflösung, Verbindung und Wirkung. Der Elternpfad
muss absolut und normalisiert sein. Name und Gesamtpfad sind auf 4096 UTF-8-Bytes,
der neue Pfad auf 64 Komponenten begrenzt. Für die Vorbereitung gelten 60 Sekunden
zuzüglich eines bereits begonnenen Requests mit höchstens 30 Sekunden I/O-Frist.

Die Ausführung bindet Sitzung, Generation und die vollständige Elternkette vom
kanonischen Profilroot bis zum gewählten Pane-/CLI-Ordner an starke Collection-ETags.
Ein vollständiges frisches Elternlisting schließt auch möglicherweise gleiche
Groß-/Kleinschreibungs- und Unicode-Namen aus. Nach erneutem Kettencheck sendet der
Core genau ein `MKCOL` mit genau einer getaggten `If`-Bedingung für den unmittelbaren Elternordner und
`If-None-Match: *`. Er sendet kein probeweises `PUT`, `MOVE` oder `DELETE`.

Erfolg verlangt HTTP 201 sowie einen unabhängigen Readback: exakter Name, echte
leere Collection, eindeutiger Elternlisting-Eintrag, übereinstimmende Tokens und
weiterhin dieselbe Elternkette und Sitzung. Eine ETag in der Anlegeantwort ist
optional; wenn vorhanden, muss sie stark sein und mit dem Readback übereinstimmen.
Ohne diese ETag benennt das Ergebnis ausdrücklich den fehlenden direkten
Identitätsvergleich zwischen Antwort und Readback.

## Grenzen und Fehler

Die vollständige Elternkette wird vor und nach MKCOL geprüft. Atomar wird genau
der unmittelbare Elternbeleg gebunden. Weitere getaggte Listen würden eine falsche
Elternversion durch ODER überdecken und werden deshalb nicht gesendet. Ein später
Vorfahrenwechsel kann trotzdem vor der Drifterkennung wirken.
[Mutationsgrenze und Gegenbelege](webdav-mutation-boundary.md).

Best Effort vertraut den serverseitigen Bedingungen ohne künstliche Gegenprobe.
Es beweist weder dauerhafte Speicherung noch unter allen Umständen, dass Antwort
und späterer Readback denselben neu angelegten Ordner meinen. Server ohne starke
Eltern-ETags bleiben konkret gesperrt. Ändert eine Collection ihre ETag durch die
Anlage, kann die Nachprüfung den Lauf als unklar einstufen, obwohl der neue Ordner
vorhanden ist. Dies ist keine allgemeine WebDAV-Schreibfreigabe.

Vorhandene Namen werden erhalten. HTTP 405, 409 oder 412 werden ausschließlich
lesend eingeordnet: beobachtete Kollision, veränderte Elternkette oder konkrete
Serverablehnung. Verbindungsverlust, unerwartete Antwort und fehlender oder
widersprüchlicher Readback nach möglicher Wirkung liefern `outcome_unclear` mit
dem Prüfpfad. Der mögliche neue Ordner bleibt stehen; es gibt weder automatischen
Wiederholversuch noch aufräumendes Löschen. Die GUI zeigt einen kurzen Fehler und
aufklappbare vollständige Details samt Prüfpfaden.

SFTP-mkdir bleibt wegen fehlendem an den Elternordner gebundenem MKDIR in v3
gesperrt: Ein Elternwechsel könnte außerhalb des gewählten Bereichs schreiben.
Diese Grenze wird durch den WebDAV-Weg nicht gelockert.

## Standardbezug und Prüfung

[RFC 4918 §9.3](https://www.rfc-editor.org/rfc/rfc4918.html#section-9.3) beschreibt
die einzelne Collection-Anlage mit `MKCOL`.
[§9.6.1](https://www.rfc-editor.org/rfc/rfc4918.html#section-9.6.1) verlangt für
Collection-DELETE `Depth: infinity`; eine nicht standardkonforme `Depth: 0`-Löschprobe
darf deshalb keine Eintrittsschranke für die gewöhnliche Ordneranlage sein.

`core/tests/webdav_folder.rs` prüft einen isolierten HTTPS-Server mit standardgemäß
abgelehntem Collection-DELETE, Unicode-/Prozentnamen, fehlender Antwort-ETag,
Namenskollisionen, falschem oder fehlendem Readback, Antwortverlust, Eltern- und
Profilrootwechsel sowie fehlenden Elternbedingungen. Wire-Zähler belegen genau
eine angeforderte Mutation und keine künstliche Probe oder automatische Bereinigung.
Browserfälle prüfen Vertrag vor Namenseingabe, Klassenweitergabe, veraltete
Navigation und zugängliche vollständige Fehlerdetails.

Die native macOS-Abnahme vom 2026-10-18 belegt Unicode-Anlage, vorhandene Namen,
verlorene Antwort, falschen Readback, ungültige Namen, bedingte Elternablehnung
und einen frischen App-Neustart gegen einen künstlichen HTTPS-Root. Fünf MKCOL-
Anfragen erzeugten vier neue Ordner; es gab keine PUT-/MOVE-/DELETE-Probe und
keinen automatischen Wiederholversuch. Die sichtbaren Erfolgs-, Kollisions- und
aufgeklappten Fehlerzustände wurden am echten App-Bundle geprüft. Eigene Prozesse,
Dialoge und künstlicher Schlüsselbundeintrag waren anschließend entfernt.
