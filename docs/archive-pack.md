# Lokale ZIP-Archive erzeugen

Stand: 2026-11-01. ZIP-Erzeugung ist als eigener Core-/Fensterqueue-Ablauf umgesetzt.
Core-, CLI- und App-Suiten, Clippy und der gesperrte Fuzz-Check bestehen auf
macOS und Linux. Native Bedienläufe auf beiden Plattformen prüfen Abbruch,
Deflate-/Stored-Inhalte, Leerordner, Kollision und Quelländerung;
[Umfang und Prüfgrenzen](roadmap-2026-10-23.md#zip-erstellung-und-lesekern).

Eine entfernte Gegenpane ist als Ziel wählbar; dafür gilt der
[zweiphasige Archivupload](remote-pack.md). Dieser Vertrag beschreibt die lokale Erzeugung.

## Bedienung

Eine lokale Auswahl im aktiven Pane markieren und über **Befehle → ZIP-Archiv
erstellen …** oder das gleichnamige Kontextmenü öffnen. Das Archiv entsteht im
angezeigten Pane-Ordner. Der Dialog bietet einen einzelnen Archivnamen sowie
Deflate (Standard) oder Stored ohne Kompression. **Vorschau erstellen** liest den
Bestand und zeigt den konkreten Zielpfad, Datei-/Ordnerzahl, Inhaltsbytes,
Ausführungsklasse und sämtliche pfadgenauen Auslassungen. Hinweise stehen auf
Seiten mit jeweils höchstens 40 Einträgen und einer vollständigen Gesamtbilanz.

**ZIP erstellen** reiht genau diese backendgebundene Absicht in die Queue des
verursachenden Fensters ein. Eine Änderung an Name oder Kompression verwirft die
Vorschau. Die Vorbereitung ist unabhängig von der Operations-Queue abbrechbar.
Ein laufender Queueeintrag verwendet deren vorhandenen Fortschritt und Abbruch.
Es gibt kein eigenes Tastenkürzel. Entfernte Panes erhalten die konkrete
Backendmeldung, dass lokale ZIP-Erzeugung dort noch nicht verfügbar ist.

## Quellen und Archivinhalt

Der Core bindet direkte Pane-Kinder und beobachtet ausgewählte Bäume iterativ.
Lokale Deskriptoren und Versionsmerkmale schützen gegen Quell-/Elterntausch;
Dateiinhalte werden während des Streams gehasht und am Ende erneut gebunden.
Die Vorbereitung hasht keine vollständigen Dateien vorab. Inhalt und leere
Ordner bleiben erhalten; ZIP64 wird vor dem Stream aktiviert. Es gibt keine
feste Dateigrößen- oder Transfergrenze und keinen zweiten vollständigen Spool.

Symlinks und Spezialdateien werden nicht verfolgt. Nicht bedienbare Namen sowie
mehrdeutige Case-/Unicode-Gruppen werden pfadgenau ausgelassen; unabhängiger
zulässiger Inhalt bleibt packbar. Fehlende oder unvollständige Quellbeobachtung
ist ein Fehler. Nicht darstellbare ZIP-Pfade werden vor dem Schreiben ausgelassen.
Der Inhaltsnamensraum folgt dabei denselben Pfadregeln wie der Archivleser:
Ein Doppelpunkt in einer Komponente lässt die betroffene Datei beziehungsweise
den gesamten betroffenen Teilbaum sichtbar aus. Unabhängige Inhalte bleiben
packbar und können mit Doppeldecker wieder gelesen und entpackt werden.

Das portable Profil speichert einfache Rechte und darstellbare DOS-Änderungszeit
in UTC mit Zweisekundenauflösung (1980–2107; sonst Standardzeit). Erstellungszeit,
Owner, Sonderbits, ACL, xattrs, Resource Forks sowie Sparse-/Hardlinkbeziehungen
werden nicht bewahrt. Diese Grenzen stehen pro betroffenem Pfad in der Vorschau.
Inhaltsauslassungen sind strukturell von Metadatenhinweisen unterscheidbar.

Ein internes Budget von 64 MiB begrenzt den zusätzlich aufgebauten ZIP-Index und
seine Hinweis-/Pfadlisten. Eine Überschreitung bricht vor der ersten Zielwirkung
sichtbar ab. Das Budget ist kein Nutzerschalter und kein Gesamtprozess-Heaplimit;
die vorhandene lokale Quellbindung hält ihren eigenen Metadatenbestand.

## Commit, Queue und Recovery

Der Writer schreibt direkt in genau einen Temp des bestehenden lokalen
Dateioperationsrahmens. Nach explizitem ZIP-Abschluss erfolgen Quellschlussprüfung,
Flush und vollständiger BLAKE3-Readback. Unmittelbar vor dem Commit werden Quellen,
Zielroot, Zielklasse und Zielbestand nochmals geprüft. Bestehende Archive werden
nicht zum Ersetzen angenommen; der Produktpackweg parst kein fremdes ZIP.

Die stärkste tatsächlich verfügbare Zielklasse gilt. Ein abgesicherter exklusiver
Commit verweigert späte Kollisionen. Der vorhandene Best-Effort-Weg prüft den
Zielbestand frisch, versucht genau eine Mutation und liest zurück; eine zwischen
Prüfung und Commit erzeugte Zieldatei kann dabei ersetzt werden. Fehlende
Flushgarantien werden getrennt angezeigt und verwenden ausschließlich den
vorhandenen, für maschinenlesbares Nichtunterstützt zugelassenen Best-Effort-Weg.
Andere I/O-Fehler bleiben Fehler.

Journalversion 28 speichert Absicht, Optionen und bestätigten Bestandsdigest samt
Vorschau, keine Handles oder Permits. Der Digest bindet Root-/Mountidentitäten,
Dateiversionen einschließlich hochauflösender mtime/ctime, vollständige beobachtete
Ordner und Auslassungen. Jeder Claim bereitet frisch vor. Eine Abweichung erzeugt
`ArchivePackChanged`; die Queue zeigt die neue konkrete Wirkung vor erneuter
Bestätigung. Auch nach Neustart entsteht kein vollständiger Vorhash.

Ein beim Prozessende bereits `Running` gesetzter ZIP-Lauf wird als `Interrupted`
mit `outcome_unclear` und Prüfpfaden wiederhergestellt. Es gibt keinen blinden Retry.
Unklare Commit-/Flushausgänge bewahren Ziel, Temp und Recoverybelege; nach manueller
Prüfung kann der Eintrag ohne erneute Mutation geschlossen werden. Ein sauberer
Abbruch entfernt ausschließlich beweisbar eigene Temps. Fehlt dieser Beleg, bleibt
der Temp als ausdrücklich angezeigter Recoverypfad stehen. Ein fehlgeschlagener
terminaler Queuecommit wiederholt ausschließlich den Abschluss, niemals das Packen.

Die [lokale Archivansicht](archive-read.md) bietet bereits Navigation und passive
Inhaltsvorschau. [Entpacken](archive-extract.md) verwendet einen eigenen lokalen
Streaming-/Fensterqueue-Ablauf.
