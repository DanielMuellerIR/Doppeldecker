# SFTP-Dateiinhalte und additive Downloads

Stand: 2026-10-19.

Die interaktive Metadatenanzeige erhält fehlende SFTP-Größen als `null` statt
als 0 Byte. Dieser Darstellungswert ist kein Inhalts- oder Plannerbeleg; die
Inhaltsvorbereitung verlangt weiter eine tatsächlich gelieferte Dateigröße.

## Interaktive Leseklasse

SFTP v3 bietet reguläre Inhalte als ausdrücklich ausgewiesenes Best Effort.
Die Klasse ist vom strikten `SftpVfs` getrennt: dessen `stat`, `open_read`,
`list_complete`, Rootidentitäten, Sync- und Mutationsmethoden bleiben gesperrt.
`ProviderEndpoint::interactive_file_read_context` liefert ausschließlich eine
private Lesesicht auf die konkret ausgewählten Dateien und begrenzten Bäume. Sie besitzt keine
Planner-Rootidentität, keine strikte vollständige Verzeichnisfreigabe und keine Mutation.

Jede Auswahl enthält 1 bis 100 direkte reguläre Dateien oder Ordner des gewählten
Pane-Roots. Eine Datei darf höchstens 8 GiB enthalten. Pfade sind auf 4096 Bytes und
64 Komponenten begrenzt. Ungültige, doppelte oder aus dem Pane herausführende
Pfade werden vor dem ersten Providerzugriff abgelehnt. Reguläre Dateien brauchen
Typ, Größe und Änderungszeit; Verzeichnisse brauchen mindestens einen belegten Typ.

Die iterative Baumvorbereitung beobachtet höchstens 1000 Einträge einschließlich
gewählter Wurzeln und ausgelassener Kinder, 16 Unterordnerebenen und bei
Ordnerauswahl insgesamt 16 GiB Dateiinhalt. Sie besitzt eine Frist von 60 Sekunden;
ein bereits laufender Request beziehungsweise dessen Handle-Cleanup kann jeweils
noch bis zu 15 Sekunden benötigen. Sie öffnet Verzeichnisse ausschließlich lesend
und liest READDIR bis zum tatsächlichen EOF. Leere NAME-Pakete, doppelte Namen,
unsichere Namen, mehr als 1000 Kinder, mehr als 1 MiB Namen pro Ordner oder mehr
als 1003 Antwortseiten werden abgelehnt. Möglicherweise lokal gleichwertige
Groß-/Klein- und Unicode-Namen gelten konservativ als Kollision.

Symlinks, Spezialdateien und reguläre Kinder ohne erforderliche Attribute werden
mit Pfad und Grund ausgelassen; Verzeichnislinks werden nicht betreten. Ohne eine
verbleibende unterstützte Auswahl scheitert die Vorbereitung. Unlesbare, ungültige
oder abgebrochene Listings und überschrittene Grenzen stoppen in diesem Schnitt
die gesamte Vorbereitung vor jeder lokalen Wirkung. Es entsteht dabei keine
scheinbar vollständige Teilfreigabe. Unabhängige Wurzeln können gesondert gewählt
werden. Ein einzelner belegter Dateiinhalt braucht weiterhin kein behauptetes
vollständiges Listing seines Elternbestands.

## Belege und verbleibendes Risiko

Die RAM-Sicht hält Endpunkt, gepinnten Hostkey, kanonischen Profilroot,
Sitzungsgeneration, gewählten Pane-Root, Datei und beobachtbare Attribute aller
Elternkomponenten. LSTAT und REALPATH prüfen Root, Eltern und letzten Namen.
OPEN wird ausschließlich mit READ gesendet; FSTAT muss zum Dateibeleg passen.
Die Übertragung liest über den gehaltenen Handle und überschreitet niemals die
gebundene Größe. Ein abgeschnittener oder wachsender Stream ist ein Fehler.

Vor EOF liest ein zweiter Handle zum frisch geprüften Pfad den gesamten Inhalt
nochmals. Größe und BLAKE3 müssen übereinstimmen; beide Handles und die Pfadkette
werden erneut geprüft und geschlossen. Gleich große Inhalte mit unveränderter Zeit
werden dadurch unterschieden. Abbruch und Fehler liefern kein erfolgreiches EOF.
Ein fehlgeschlagener Strom ist nicht wiederverwendbar. Requests haben eine Frist;
Abbruch beendet einen bereits blockierenden Request nicht gewaltsam. Die Eltern-
und Attributprüfung liest den Abbruchtoken zwischen ihren einzelnen Requests,
sodass nach dem laufenden Request keine weitere Pfadkette abgearbeitet wird.

Diese Prüfungen sind **keine atomare Namespacegarantie**. SFTP v3 liefert hier
weder ein rootrelatives No-Follow-OPEN/OPENDIR noch portable unverwechselbare Datei-IDs.
Ein unbemerkter Pfadtausch zwischen den Requests, insbesondere mit identischen
Attributen, bleibt möglich. Auch die beiden Inhaltsdurchläufe sind kein atomarer
Dateisystem-Snapshot. Pane, Vorbereitung, Status, Ergebnis und CLI benennen diese
Best-Effort-Klasse; ein Hostkey-Pin wird nicht als Lösung des Pfadrennens dargestellt.

Nach Antwortverlust scheitert der aktuelle Versuch, auch während der
Metadatenvorbereitung vor dem ersten OPEN. Erst ein neuer interaktiver
Leseversuch darf einmal identitätsgleich reconnecten und frisch binden; alte
Lesesichten verlieren ihre Generation. Eine bereits ausgeführte lokale Mutation
wird dadurch nicht wiederholt. Eine geänderte Serveridentität bleibt gesperrt.

## Öffnen und Dateivergleich

Der externe Dateivergleich verwendet für jede SFTP-Seite ebenfalls
`download_interactive_snapshot`; der strikte Planner-/VFS-Leser bleibt gesperrt.
Best-Effort-Pfadrisiko, Auswahlgrenzen, Hostkey-/Generationsbindung und Inhalts-
Readback bleiben identisch. Die native Vorbereitung ist fenstereigen abbrechbar;
vor der Werkzeugübergabe wird der Abbruch nochmals geprüft. Beide Quelldateien
bleiben unverändert, nur die geprüften lokalen Kopien gehen an das Werkzeug.
Vorbereitung und Ergebnis zeigen die Einschränkungen sichtbar an.
[Vergleichsvertrag](settings-session-diff.md).

`download_interactive_snapshot` benutzt den bestehenden privaten Snapshotcache:
Speicherprüfung, kollisionsfreier Noncename, Inhalts-/Größen- und lokaler
BLAKE3-Readback, Modus 0600, Manifest und sieben Tage Aufbewahrungsziel.
Nur bekannte Dokument-/Medienendungen werden für die System-App erhalten;
unbekannte und ausführbare Endungen sowie HTML, HTM, SVG und XML werden als `.txt`
übergeben. Skripte und externe Stylesheets dieser Webformate werden dadurch nicht
über ihre aktive Dateiendung an einen Browser übergeben. Die System-App für bekannte
Dokument-/Medienformate läuft weiterhin mit ihren eigenen Fähigkeiten; Doppeldecker
behauptet keine Sandbox dieses Betrachters. Der Remote-Name
wird weder lokaler Pfad noch Shellbefehl. Es gibt keinen Rückschreibweg.
Der aktive Formatumfang folgt den Standards für
[SVG-Skripte](https://www.w3.org/TR/SVG2/interact.html#ScriptElement) und
[XML-Stylesheet-Verweise](https://www.w3.org/TR/xml-stylesheet/).
Der Snapshotpfad führt zusätzlich seinen unabhängigen Remote-Hashreadback aus;
mit dem zweipassigen SFTP-Reader sind das vier vollständige Lesedurchläufe.

Abbruch oder Drift entfernt nur den eigenen unvollständigen Download. Cleanup
entfernt ausschließlich alte, manifestierte, unveränderte eigene Snapshots;
fremde Einträge und unmanifestierte Crashreste bleiben erhalten. Die
fenstereigene abbrechbare Hintergrundoperation und die frische Profilprüfung vor
der System-App-Übergabe bleiben bestehen. Andere Fenster und Navigation bleiben
unabhängig.

## Additive lokale Kopie und Recovery

F5 und CLI-`copy` erlauben nur SFTP→lokal. F6, Remote-Ziele und Ersetzen sind für
diese Quellklasse gesperrt. Vorhandene Ziele bleiben stehen. Konflikte erlauben
Überspringen oder, bei abgesichertem lokalen No-Replace, die backendgebundene
Variante „Beide behalten“. Eine späte Kollision erteilt kein Ersetzungsrecht.
Der lokale Commit verwendet dieselben Temp-, Flush-, BLAKE3-, Readback- und
Fehlerverträge wie andere F5-Kopien. Eine schwache lokale Zielklasse wird zusätzlich
und unabhängig vom SFTP-Leserisiko angezeigt und bestätigt.

Konfliktvergleiche binden reguläre Dateien mit vollständigem Hash. Bei Ordnern
binden sie zusätzlich den beobachteten Namensraum und alle enthaltenen Dateibelege. Sie behaupten dafür kein vollständiges SFTP-Elternlisting;
Zielkonflikte und Ersetzungen behalten ihren strikten Listingbeleg. Auch diese
unter Umständen lange Vorbereitung ist über den fenstereigenen Abbruchtoken und
Escape abbrechbar; ihr Abschluss kommt vor dem Start des nächsten Queueworkers.

Die Baumlesesicht liefert ihre Kinder über einen eigenen interaktiven Vertrag.
`list_complete` bleibt gesperrt. Vor und nach einer Baumkopie werden Verzeichnisse,
Elternketten und beobachtete Kinder erneut geprüft; jedes Dateiblatt behält den
vollständigen Inhaltsbeleg. Ein später Fehler rollt bereits fertig hinzugefügte
Dateien nicht zurück. Das Ergebnis bilanziert diese Teilwirkung und nennt den
betroffenen Zielunterbaum zur Prüfung; fremde Ziele bleiben erhalten.

Journalformat 24 persistiert Auswahl, Endpunkte, öffentliche Verbindungsreferenzen
und den gezeigten Baumumfang samt Digest und Auslassungen. Der Digest bindet
Provideridentität, Wurzeln, beobachtete Attribute, Elternketten und Listings,
jedoch keine Sitzungsgeneration oder Handles. Er bleibt bei einer unveränderten
Quelle über Reconnect und Neustart vergleichbar. Eine neue oder geänderte
Baumvorbereitung am Queuekopf hält vor Zielproben und lokalen Mutationen an.
`SourceTreeChanged` zeigt die neue Bilanz und verlangt die exakte Phrase
`BAUM <Laufkürzel> <Eintragszahl>`. Danach bereitet der Worker nochmals frisch vor.
Die GUI bindet auch eine konfliktfreie Baumvorschau an eine fenstereigene opaque
Vorbereitungs-ID; die CLI übernimmt den Umfang aus derselben Core-Vorbereitung. Nach Neustart verbindet und prüft der Worker frisch;
Handles, Lesesichten und Zustimmungen werden nicht serialisiert. Lokales privates
C2-Staging oder ein privater Nachbar-Tempintent wird vor der Tempauswirkung an die
verursachende Queue gebunden. Ein ungeeigneter Recoveryordner stoppt vor Temphanlage.
Nach Prozessabbruch bleiben Prüfpfade sichtbar, ohne automatisches Wiederholen
oder Löschen. Bereits abgeschlossene Dateien und erhaltene fremde Ziele bleiben
bilanziert. Audit und Ergebnis unterscheiden die schwache Inhaltsquelle vom
konkreten lokalen Commit.

Wird nach Anlage eines neuen lokalen Zielordners abgebrochen und fehlt dessen
atomar gebundener Entfernungsbeleg, entfernt der Rückweg diesen Baum nicht über
seinen bloßen Pfad. Er meldet `outcome_unclear` samt Zielbaum, möglichen Temps und
privaten Recoveryintents. Die Queue erhält diese Prüfpfade auch nach Neustart und
wiederholt den fehlgeschlagenen Lauf nicht. Dieser konservative Ausgang ist von
einem mutationsfreien Abbruch sowie dem Abbruch in einem schon vorhandenen
Zielordner zu unterscheiden.

## Prüfung und nächste Grenze

Künstliche SSH-/SFTP-Paketgegenläufe prüfen leere und große Inhalte, Namen,
Typ-/Größengrenzen, Root-/Elterntausch, Inhaltsdrift bei gleicher Größe/Zeit,
abgeschnittene Streams, Readbackfehler, Antwortverlust, Sitzungswechsel, Abbruch,
späte lokale Kollision und Queue-Reopen. Ein deterministischer Metadatenabbruch
stoppt vor dem nächsten Elternrequest und vor jedem OPEN. Ein echter Kindprozess endet nach
Teiltransfer; der Reopen findet seinen privaten Tempintent und startet ihn nicht.
Der isolierte OpenSSH-Gegenlauf prüft zusätzlich den bisherigen strikten
Negativvertrag und eine erfolgreiche Momentaufnahme.

Die rekursive additive Auswahl ist als eigener begrenzter Best-Effort-Schnitt
umgesetzt. SFTP-mkdir, Rename, Upload, Ersetzen, Move und Sync bleiben offen.
Eine atomare Root-/Elternbindung wird weiterhin nicht behauptet. Die
Implementierungs-Deltaprüfung ersetzt keinen Vollreview der übergeordneten
CodeQA-Bereiche und keine interaktive Linux-Kandidatenabnahme.

Der automatisierte Prüfstand vom 2026-10-18 umfasst auf macOS 974 Core-, 66 CLI-
und 202 App-Tests, auf Linux 993 Core-, 66 CLI- und 192 App-Tests. Formatierung,
Workspace-Clippy mit allen Targets und der gesperrte Fuzz-Build sind auf beiden
Plattformen grün; die isolierten Quellen wurden per SHA-256 abgeglichen.
Die Browserprüfung besteht aus 310 Fällen, sechs Pane-Tests und dem CSP-Scan;
135 Tooltests und die gesperrte Frontendinstallation sind ebenfalls grün.

Die Standardsuite lässt je Plattform 21 Core-Opt-ins aus. Die beiden OpenSSH-Fälle
wurden gesondert mit genau einem Filtertreffer ausgeführt; die regulären und
rekursiven SFTP-Crashgegenläufe starten ihren gemeinsamen Helfer jeweils gezielt
mit Exit 91. Ein zusätzlicher Opt-in führt die rekursive Queuekopie auf eigenen
APFS-/ext4-/exFAT-Images aus: tatsächliche lokale Commitklasse, erforderliche
Bestätigung, falsche Phrase, Neustart und Inhalts-Readback sind belegt. Alle eigenen
Images beziehungsweise Loopdevices sind danach freigegeben. Übrige Mount-, aktive
LAN- und Samba-Opt-ins wurden nicht erneut ausgeführt. Zwei Linux-App-Anzeigetests
bleiben ohne interaktive Displayabnahme ignoriert.

Die native AP04-macOS-Prüfung belegt additive Unicode-/Leerdateien, Konfliktkopien ohne
Ersetzungsangebot, gesperrtes Verschieben und nichtreguläre Quellen, die geöffnete
Bildmomentaufnahme in der System-App, Abbruch bei weiter möglicher Navigation,
späte Kollision, Inhaltsdrift und Antwortverlust. Ein ausdrücklicher neuer
Öffnen-Aufruf nach Antwortverlust erzeugte über eine frische Sitzung eine
inhaltsgleiche, nicht ausführbare 16-MiB-Momentaufnahme. Ein pausierter F5-Auftrag
blieb über den App-Neustart erhalten und wurde erst nach Fortsetzen frisch geprüft
und abgeschlossen; Quellhashes blieben unverändert. Eigenes Cleanup ist bestätigt.

Einzelne Hilfsläufe scheiterten an nativer Navigation beziehungsweise an der
Dokumentbereitschaft der externen Text-App. Sie werden nicht als bestandene Blöcke
gezählt: Reconnect ist durch App-Abschluss, Sitzungs-/Wirebeleg und Snapshot-Hash,
der Neustart durch den gesondert bestandenen nativen Block belegt. Die früheren
Zustandsprüfungen und die letzte gezielte Abnahme behalten jeweils ihren eigenen
Buildnachweis; die dazwischen korrigierten Abbruch-/Reconnectgrenzen sind außerdem
in den finalen Paketgegenläufen geprüft.

Die native AP04R-Abnahme ergänzt rekursive Unicode-Dateien und leere Ordner,
sichtbare Link-/FIFO-Auslassungen, Zusammenführen ohne Verlust alter Dateien und
gebundene Kopien des ausgewählten Unterbaums unter neuem Namen. Der geänderte
Baum nach App-Neustart hielt vor jeder Zielwirkung an; die richtige Phrase führte
zu einer frischen erfolgreichen Vorbereitung. Während des Transfers blieb echte
Zielnavigation möglich. Abbruch nach neuer Ordneranlage erhielt den Zielbaum und
seine Recoveryintents sichtbar mit `outcome_unclear`; es entstand keine vollständig
übertragene Zieldatei. Der bestandene Block und sein eigenes Cleanup sind belegt.
Die konkrete Tiefenabsage ohne Zielwirkung ist zusätzlich vor einer später
fehlgeschlagenen Hilfserwartung geprüft. Frühere Hilfsläufe erwarteten falsche
Fokus-, Konflikt- oder Recoveryzustände und zählen nicht als bestandene Blöcke.
Die Quellhashes blieben bis auf die ausdrücklich injizierte neue Datei gleich;
kein SFTP-Mutationsrequest wurde gesendet.
