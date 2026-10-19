# Gewöhnliche WebDAV-Umbenennung

Stand: 2026-10-18. F2 und CLI `rename` verwenden für WebDAV eine eigene bedingte
Best-Effort-Wirkung. Sie benötigen weder eine künstliche Collection noch eine
Schreib-/Löschprobe. Strikter Ziel-Sync und allgemeine VFS-Mutationen bleiben
ohne gemeinsame Root-/Elternbindung passiv gesperrt;
[Mutationsvertrag](webdav-mutation-boundary.md).

## Anzeige und Vorbereitung

Der Core liefert vor der Namenseingabe `confirmed`, die Klasse `best_effort` und
konkrete Einschränkungen. Die GUI zeigt diese im Namensdialog und reicht die
angezeigte Klasse zurück. Abweichende oder fehlende Klassen scheitern mit
`plan_drift` vor Profilauflösung und Netzwerk. Das CLI zeigt dieselben Hinweise
vor der Wirkung; `rename_result` enthält Klasse, Einschränkungen und neuen Pfad.
`capabilities.interactive_rename` beschreibt diesen gesonderten Interaktionsvertrag;
die bisherigen Felder berichten weiterhin die strengeren verhaltensgeprüften
Fähigkeiten. Der Interaktionsvertrag allein beweist keine konkrete Serverfähigkeit.

Pfad und Name werden vor Queue-, Profil- und Providerzugriff geprüft. Die Quelle
muss unter dem gewählten Root liegen; der neue Name bleibt im selben Elternordner.
Namespacepfade sind auf 4096 UTF-8-Bytes und 64 Komponenten begrenzt. Lesephasen
haben 60 Sekunden zuzüglich eines bereits begonnenen Requests mit höchstens
30 Sekunden I/O-Frist.

## Einzelwirkung und Readback

Eine private VFS-Sicht verwendet den gemeinsamen Rename-Koordinator. Sie bindet
Sitzung, Generation, den starken Quelltoken und alle Collection-ETags vom
kanonischen Profilroot bis zum gewählten Elternordner. Ein vollständiges Listing
prüft auch mögliche Groß-/Kleinschreibungs- und Unicode-Kollisionen. Nach frischen
Quell- und Elternprüfungen folgt genau ein `MOVE` mit `If-Match` für die Quelle,
genau einer getaggten `If`-Bedingung für den gemeinsamen unmittelbaren Elternordner und `Overwrite: F`.

HTTP 201 oder 204 genügt nicht allein. Erfolg verlangt einen frischen vollständigen
Elternbestand: alter Name fehlt, neuer Name ist exakt vorhanden, Typ, Größe und
starker Versionstoken stimmen mit dem Quellbeleg überein. Sitzung und gesamte
Elternkette müssen unverändert sein. Die Wirkung liest keine Datei-Inhalte und
behauptet keinen eigenen Contenthash. Nichtleere Ordner werden als Ganzes
umbenannt, ohne Zusammenführen oder rekursives Löschen.

## Grenzen und unklarer Ausgang

Die vollständige Elternkette wird vor und nach MOVE geprüft. Genau eine getaggte
Liste bindet den unmittelbaren gemeinsamen Elternordner; zusätzliche Listen könnten
diese Bedingung durch ODER abschwächen. Ein später Profilroot-/Vorfahrenwechsel
kann trotzdem bereits umbenennen, bevor der Readback `outcome_unclear` meldet.
[Mutationsgrenze und Gegenbelege](webdav-mutation-boundary.md).

Die Serverbedingungen werden ohne künstliche Gegenprobe verwendet. Missachtet
ein Server `Overwrite: F`, könnte ein spät entstandenes Ziel unwiederbringlich
ersetzt werden. Diese Grenze und die fehlende Durability sind vor der Wirkung
sichtbar. ETags sind Serverbelege; sie ersetzen keine lokale Deskriptorbindung.

Server ohne starke Quell-/Eltern-ETags bleiben konkret gesperrt. Ändert der Server
ETags allein durch die Umbenennung, kann der Readback keinen Erfolg belegen und
meldet einen unklaren Ausgang. HTTP 405, 409 und 412 werden ausschließlich lesend
in Kollision, Drift oder Ablehnung eingeordnet. Antwortverlust, unerwartete Antwort
und fehlender oder widersprüchlicher Readback nach möglicher Wirkung liefern
`outcome_unclear` mit altem Namen, neuem Namen und Elternpfad. Es gibt keinen
automatischen zweiten MOVE, Reconnect oder Cleanup-DELETE.

Andere Provider behalten ihre bestehenden Rename-Verträge. SFTP v3 bleibt wegen
fehlender atomarer Root-/Elternbindung konkret gesperrt; die GUI zeigt dies vor
der Namenseingabe. Fensterreservierung und Profilrevision gelten unverändert.

## Standard und Prüfung

[RFC 4918 §9.9](https://datatracker.ietf.org/doc/html/rfc4918#section-9.9) beschreibt
MOVE und [§10.6](https://datatracker.ietf.org/doc/html/rfc4918#section-10.6) den
Schutz vorhandener Ziele durch `Overwrite: F`.
[RFC 9110 §13.1.1](https://www.rfc-editor.org/rfc/rfc9110.html#section-13.1.1)
verlangt starken ETag-Vergleich für `If-Match`.

Die HTTPS-Gegenläufe in `core/tests/webdav_rename.rs` prüfen Dateien, nichtleere
Ordner, Unicode-/Prozentnamen, Kollisionen, verlorene Antworten, falschen Readback,
Profilroot-/Elternwechsel und Quelländerungen gleicher Größe. Wire-Zähler prüfen
eine Mutation sowie fehlende MKCOL-/PUT-/DELETE-Proben. Browserfälle prüfen
Vertragsanzeige, Klassenweitergabe und veraltete Navigation; Deserialisierung und
CLI prüfen die Eingabegrenze vor Providerzugriff.

Die native macOS-Abnahme vom 2026-10-18 prüfte Datei und nichtleeren Ordner,
Unicode-/Prozentnamen, vorhandene Ziele, Antwortverlust, falschen Readback,
Quelländerung gleicher Größe, Elternwechsel und frischen App-Neustart. Acht MOVE-
Anfragen bewirkten sechs Umbenennungen; die beiden übrigen wurden bedingt abgelehnt.
Eine zusätzliche gewünschte Ordneranlage prüfte den gemeinsamen Namespacehelfer.
Erfolgs- und vollständige Fehlerzustände wurden in ausdrücklich gesetztem dunklem
und hellem Theme visuell geprüft. Es gab keine PUT-/DELETE-Probe, keinen Blind-Retry
und anschließend keine eigenen Prozesse, Dialoge oder künstlichen Zugangsdaten.
