# Gewöhnliche WebDAV-Umbenennung

Stand: 2026-10-24. F2 und CLI `rename` verwenden für WebDAV eine eigene bedingte
Best-Effort-Wirkung. Sie benötigen weder eine künstliche Collection noch eine
Schreib-/Löschprobe. Gewöhnliches Ersetzen und dauerhaftes Löschen besitzen
inzwischen eigene Best-Effort-Wege. Strikter Ziel-Sync und allgemeine strikte
VFS-Mutationen bleiben getrennt;
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
prüft auch mögliche Groß-/Kleinschreibungs- und Unicode-Kollisionen. Vor MOVE
müssen alle unmittelbaren Kindbelege einschließlich starker ETags verfügbar sein;
auch eine unabhängige Collection ohne solchen Beleg verhindert diesen Lauf vor
der Wirkung, weil der nachfolgende vollständige Namensvergleich sonst nicht
belegbar wäre. Nach frischen
Quell- und Elternprüfungen folgt genau ein `MOVE` mit `If-Match` für die Quelle,
genau einer getaggten `If`-Bedingung für den gemeinsamen unmittelbaren Elternordner und `Overwrite: F`.

HTTP 201 oder 204 genügt nicht allein. Erfolg verlangt einen frischen vollständigen
Elternbestand: alter Name fehlt, neuer Name ist exakt vorhanden, Typ, Größe und
starker Versionstoken stimmen mit dem Quellbeleg überein. Der vollständige
Kindbestand muss exakt dem Vorbestand mit dieser einen Namensänderung entsprechen.
Sitzung und alle übrigen Vorfahren bleiben unverändert. Nur nach bestätigtem
HTTP 201/204 und diesem exakten Nachbeleg darf die ETag des unmittelbaren
Elternordners auf dessen eigene Änderung folgen. Erneute Elternprüfungen vor und
nach dem Listing begrenzen weitere beobachtete Änderungen. Die Wirkung liest keine Datei-Inhalte und
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

Fehlende oder schwache Quell-, Eltern- oder unmittelbare Kind-ETags verhindern
diese Vorbereitung vor MOVE. Ändert der Server die ETag des umbenannten Objekts
selbst, kann der Readback keinen Erfolg belegen und meldet einen unklaren Ausgang.
Ein hinzugefügter, entfernter oder geänderter Nachbareintrag verhindert ebenso die
Übernahme eines neuen Elternbelegs. Ein gleichzeitiger Pfadtausch, der alle
beobachteten Kindbelege exakt reproduziert, ist dagegen nicht atomar ausschließbar. HTTP 405, 409 und 412 werden ausschließlich lesend
in Kollision, Drift oder Ablehnung eingeordnet. Antwortverlust, unerwartete Antwort
und fehlender oder widersprüchlicher Readback nach möglicher Wirkung liefern
`outcome_unclear` mit altem Namen, neuem Namen und Elternpfad. Es gibt keinen
automatischen zweiten MOVE, Reconnect oder Cleanup-DELETE.

Die [WebDAV-Stapelumbenennung](batch-rename.md) verwendet dieselbe Einzelwirkung
und übernimmt nur deren erfolgreich geprüfte Elternbelege für die nächste Zeile.
Antwortverlust, Ablehnungen oder ein unklarer Nachbeleg erteilen diese Übernahme
nicht; der Stapel stoppt bei Bindungsverlust oder unklarem Ausgang.
Reine Case-/Unicode-Schreibweisen werden bei genau einer exakten alten
Rohgruppe über denselben Weg zugelassen. Starke Quell-ETag, vollständiger
Namespacebeleg und `Overwrite: F` bleiben erhalten; eine Serverablehnung des
eigenen Zielalias wird nicht umgangen. Entfernte Namenstausche bleiben gesperrt.

SFTP und FTP/FTPS besitzen gewöhnliche pfadbasierte Best-Effort-Wege; SMB nutzt
sein serverseitiges No-Replace-Rename ohne künstliche Vorprobe. Diese Freigaben
behaupten keine atomare Elternbindung. Fensterreservierung und Profilrevision
bleiben eigenständig gebunden.

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
