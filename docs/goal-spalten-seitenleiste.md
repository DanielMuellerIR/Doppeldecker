# Goal: Panes-Spalten und Seitenleiste auf dem Niveau ausgereifter Dateimanager

Stand: 2026-08-26, beschlossen nach einer Testrunde mit 0.1.121 (Referenz:
Verhaltensstudien ausgereifter Dual-Pane- und Systemdateimanager —
Darstellungsoptionen-Popover, Spaltenverhalten, Seitenleiste mit
Geräten/Freigaben). Dieses Goal ergänzt die Sync-Phasen 3–5 aus `backlog.md`; die
Reihenfolge zwischen beiden Strängen ist in `backlog.md` festgelegt.

Die Anforderungen beschreiben beobachtbare Bedienqualität, nicht die Nachbildung
eines fremden Produkts. Layoutsystem, Interaktion, Texte, Assets und Implementierung
bleiben eigenständig.

## Verbindliche Gestaltungsregeln

- Beim Verschmälern von Spalten wird ausschließlich der Name ellipsiert; der
  Abstand zwischen Spalteninhalten bleibt konstant.
- Sortierung bleibt immer wertbasiert (mtime numerisch, Bytes numerisch),
  unabhängig vom Anzeigeformat.
- Remote-Provider zeigen nur Felder, die das Protokoll wirklich liefert; fehlende
  Werte bleiben sichtbar leer beziehungsweise als „—“ gekennzeichnet und werden nie erfunden.
  SFTP-Dateigrößen ohne Protokollwert kommen als JSON `null` an GUI und CLI;
  echte 0 Byte bleiben davon unterscheidbar. Informationen summieren nur bekannte
  Dateigrößen und zählen fehlende Größen gesondert. Eine Auswahl ausschließlich
  unbekannter Dateien oder Links behauptet keine leere Gesamtgröße.
- Alle neuen Listen-/Spaltenmechaniken erhalten die bestehende Virtualisierung
  und `listbox`-A11y.

## G1 — Spaltenlayout-Renovierung (rein Frontend)

Umgesetzt mit 0.1.122 (headless verifiziert; GUI-Abnahme in `backlog.md`).
Umsetzung: `frontend/column-layout.js` (Trenner, gemerkte Breiten, Stufenwahl),
`fmtEntrySize`/`fmtEntryDate` in `frontend/ui-utils.js`.

Ergänzt mit 0.1.123 (2026-07-18): Die Basis der Größenanzeige ist keine
feste Entscheidung, sondern eine gemerkte Einstellung im Menü „Darstellung"
(`frontend/byte-units.js`) — dezimal als Vorgabe unter macOS, binär unter Linux,
binär mit KiB/MiB/GiB beschriftet. Die Wahl ist inzwischen zusätzlich im Darstellungsoptionen-Popover und im
[globalen Einstellungsdialog](settings-session-diff.md) erreichbar.

1. Größenspalte mit fixer Breite. Format: Einheitswechsel, sobald vier Stellen
   vor dem Komma entstünden (maximal „999,99 MB"); zwei Nachkommastellen mit
   Komma; Bytes ganzzahlig als „999 Byte"; Tabellenziffern. Spaltenbreite exakt
   auf die breiteste Variante ausgelegt.
2. Datumsspalte mit dynamischem Format nach verfügbarer Breite, vier Stufen:
   `17.07.26` → `17.07.26, 12:23` → `17.07.2026, 12:23` → `17. Juli 2026 um
   12:23`. Umschaltung live beim Spaltenziehen/Fensterresize.
3. Spalten-Splitter im Listenkopf: per Maus ziehbar, Breite je Spalte und Pane
   persistiert, Tastatur-A11y analog zum Seitenleisten-Splitter. Doppelklick auf
   einen Trenner passt die Spalte an den Inhalt an („an Namen anpassen" als
   Aktion statt Dauerschalter).
4. Kompaktere Grundabstände (Gap/Padding), damit kein Leerraum wie im heutigen
   Grid verschwendet wird.

## G2 — Spaltenkonfiguration + Darstellungsoptionen

Teil 1 ist seit 2026-08-05 umgesetzt: `Entry` liefert Art und sichere letzte
Dateiendung sowie Erstellungsdatum, Eigentümer, Gruppe und Berechtigungen. Lokale
Listings gewinnen die Werte descriptor-relativ; FTP/FTPS, SFTP, WebDAV und SMB
reichen nur tatsächlich vorhandene Protokollfelder durch. Fehlende Werte bleiben
`None`. Die vollständigen lokalen Gates und der Core-Gegenlauf auf Linux sind
Pflichtbeleg dieses Teilstands.

Teil 2 ist seit 2026-08-05 umgesetzt: Jede Pane öffnet ihr eigenes
Darstellungsoptionen-Popover. Spalten, Ordnerreihenfolge, Zeilenbild,
Datumsdarstellung, versteckte Dateien, Dichte und Textgröße gelten für den
aktuellen Provider-/Profil-/Ordnerpfad und werden lokal gespeichert; der Nutzer
kann denselben geprüften Satz als globalen Standard setzen. Die Größeneinheit ist
ebenfalls im Popover erreichbar.

Teil 3 ist seit 2026-08-05 umgesetzt: Jede Pane hält ihren eigenen
Sortierzustand. Alle sichtbaren Spalten lassen sich per Klick oder Tastatur nach
ihren Rohwerten sortieren; Größe, Änderungs- und Erstellungszeit sowie Rechte
bleiben numerisch, unbekannte Providerwerte in beiden Richtungen am Ende. Wird
die aktive Spalte ausgeblendet, fällt nur diese Pane sichtbar auf Name
aufsteigend zurück. Damit ist G2 vollständig umgesetzt und headless verifiziert.

1. Core: Listing um Art (Datei/Ordner/Endung), Erstellungsdatum, Eigentümer,
   Gruppe und Berechtigungen erweitern (lokal vollständig; Provider nach
   Capability, fehlend = leer). Kern-Änderung => volle Gates inkl. Linux-Host.
2. Darstellungsoptionen-Popover je Pane mit Scope „Dieser Ordner" und „Als
   Standard verwenden" (per-Ordner-Einstellungen lokal persistiert, Standard
   global). Inhalt:
   - Spaltenanzeige: Größe, Art, Änderungsdatum, Erstellungsdatum, Eigentümer,
     Gruppe, Berechtigungen.
   - Ordner oben anzeigen (innerhalb dieses Blocks optional immer alphabetisch),
     fettgedruckte Ordner, abwechselnde Zeilenfarben, relatives Datum, versteckte
     Dateien.
   - Dichte (Kompakt/Normal) und Textgröße (S/M/L) statt freier Schrift-/
     Icon-/Punktgrößenwahl.
3. Sortieren nach jeder sichtbaren Spalte.

## G3 — Ordnergrößen („Alle Größen zeigen")

SFTP verwendet eine eigene begrenzte Metadatenbeobachtung mit sichtbarem Best Effort,
„≈“ und dem Status `observed`; Auslassungen bleiben eine Teilbilanz. Inhalte werden
nicht geöffnet, strikte rekursive Listings bleiben gesperrt. Grenzen und Prüfung:
[SFTP-Ordnergrößen](sftp-directory-sizes.md). FTPS, WebDAV und SMB verwenden weiterhin
den folgenden lesenden Größenvertrag.

Teil 1 ist seit 2026-08-05 umgesetzt: `core/src/directory_size.rs` misst
angeforderte Ordner rein lesend über das gemeinsame VFS. Ein begrenzter
Worker-Pool verteilt auch die Unterordner eines einzelnen Baums parallel und
meldet für jeden angeforderten Ordner genau ein terminales Ergebnis. Abbruch,
unvollständige Listings, unsichere Ordnernamen und Fehler einzelner Teilbäume
bleiben maschinenlesbar sichtbar; nur vollständige Ergebnisse gelangen in den
kurzlebigen, endpunktgebundenen Sitzungscache. Eine bewusste Neuberechnung liest
neu — es gibt ausdrücklich keine vorgetäuschte mtime-Invalidierung. Vollständige
Core-Suite und Core-Clippy sind auf macOS und Linux grün.

Teil 2 ist seit 2026-08-05 umgesetzt: Eine fenster- und panegebundene
Tauri-Laufzeit hält getrennte Core-Sessions und Abbruchsignale, ohne die
Mutations-Queue des Fensters zu belegen. Lokale Panes können die Berechnung im
Darstellungs-Popover global einschalten; Remote-Panes zeigen stattdessen pro
Ordner einen ausdrücklichen Klickweg und starten nie automatisch Netz-Listings.
Zwischenstand, vollständige beziehungsweise unvollständige Ergebnisse und der
manuelle Neulauf stehen direkt in der Größenspalte. Navigation entwertet alte
Frontend-Token und bricht den Backend-Lauf ab; Ereignisse gehen nur an das
auslösende Fenster.

Teil 3 ist seit 2026-08-05 umgesetzt: Bei aktiver Größensortierung werden
vollständige Ordnerbilanzen anhand ihrer Byte-Rohwerte sofort mit jedem
Streaming-Ergebnis neu eingeordnet. Partielle, fehlgeschlagene, abgebrochene oder
noch laufende Bilanzen gelten nicht als verlässliche Größe und bleiben auf- wie
absteigend am Ende. Innerhalb des aktivierten Ordnerblocks behält die bewusste
Option „Ordner immer alphabetisch“ Vorrang; ohne Ordnerblock gilt für alle Einträge
dieselbe transitive Spaltensortierung. Damit ist G3 vollständig umgesetzt und
headless verifiziert.

1. Core: paralleler, abbrechbarer Größen-Walker (Streaming-Ergebnisse pro
   Ordner, Fehler je Teilbaum sichtbar statt still). Session-Cache; ehrliche
   Grenze: keine vorgetäuschte persistente Invalidierung (Ordner-mtime deckt
   nur direkte Kinder), stattdessen manuelles Neuberechnen.
2. UI: global einschaltbar für lokale Panes (wie Finder „Ordnergröße
   berechnen"), auf Remote-Panes ausschließlich pro Ordner auf Klick — nie
   automatisch übers Netz. Laufende Berechnung erkennbar, Navigation bricht ab.
3. Sortierung nach Größe bezieht berechnete Ordnergrößen ein.

## G4 — Seitenleiste: Geräte, Freigaben, Favoriten

Geräteabfragen übernehmen nur die jüngste Antwort, auch bei Lesefehlern. Nach
bestätigtem Auswerfen liest die Anzeige den tatsächlichen Mountbestand frisch;
ältere Gesamtlisten bleiben wirkungslos. Auch Favoriten werden nach Hinzufügen,
Entfernen, Umsortieren und Finder-Import frisch aus dem gemeinsamen Store gelesen.
Eine alte Startliste oder verspätete Mutationsantwort nimmt neuere Änderungen
nicht zurück. Scheitert nur dieser Readback, nennt die Meldung ausdrücklich die
bereits gespeicherte Änderung und den fehlgeschlagenen Anzeigeabgleich.
Ein erster Lesefehler bleibt ebenfalls sichtbar; eine beschädigte Ablage wird
nicht als kommentarlos leere Favoritenliste dargestellt.

Teil 1 ist seit 2026-08-05 auf macOS und Linux umgesetzt und headless
verifiziert. Der Core erkennt nur sichtbare lokale Volumes und Netzwerk-Mounts,
filtert Pseudo-Dateisysteme und liefert die Speicherbilanz. Seit 2026-10-06 laufen
Linux-Speicherabfragen über `/usr/bin/stat` (GNU Coreutils) in getrennten
Hilfsprozessen: höchstens 250 ms pro Abfrage und zwei Sekunden gemeinsame Frist.
Höchstens vier Prozesse dürfen zugleich laufen oder auf ihren tatsächlichen Exit
warten. Ein im Kernel blockierter Prozess wird nach SIGKILL nicht synchron
abgewartet; der nächste Abruf sammelt beendete Prozesse ohne Warten ein. Fehlendes
`stat`, Zeitlimit und Abfragefehler erhalten den Mount mit unbekannter Größe und
`space_error` samt stabilem Fehlercode. Die Seitenleiste zeigt „Speicher nicht
abrufbar“; der Tooltip enthält den genauen Grund. Gesunde Volumes bleiben verfügbar.
Die Zeitlimitprüfung erzeugt keine echten hängenden Netz-Mounts, sondern künstliche
langsame Hilfsprozesse und belegte Slots. 17 Linux-Mounttests sowie acht
macOS-Mounttests und Core-Clippy auf beiden Plattformen sind grün; der
macOS-Tauri-Gegenlauf besteht 164 Tests. Ein tatsächlicher Browserlauf prüft
Fehlertext, unveränderte gesunde Volumes, erhaltenen Auswurfknopf, escaped
Fehlergrund und die vollständige Sichtbarkeit der kurzen Meldung. Der Screenshot
wurde visuell geprüft; alle 222 Fälle der Browser-Suite sind grün. Linux bindet dabei die Kernel-Mount-ID aus `/proc/self/mountinfo`; ein später neu am
selben Pfad eingehängtes Ziel erhält dadurch ein anderes opaque Token. Die
Tauri-Schicht bindet jede Auswurfstrategie an eine opaque Mount-ID, prüft die
Liste unmittelbar vor dem festen, shellfreien Systembefehl erneut und bestätigt
danach das Verschwinden desselben Eintrags. Nicht belegte oder unklare Fälle
bleiben gesperrt.

Teil 2 ist seit 2026-08-05 umgesetzt und im vollständigen headless UI-Lauf
verifiziert. Lokale Volumes stehen unter „Geräte“, gemountete Netzwerkziele
unter „Freigaben“. „Favoriten“ umfasst die Ordnerliste und als klar beschriftete
Untergruppe die gespeicherten Verbindungen. Queue und Quarantäne folgen als
unveränderte Schnellzugriffe. Jeder der drei Hauptabschnitte besitzt bereits eine
stabile Abschnitts-ID als Grundlage für die Reihenfolge aus Teil 3.

Teil 3 ist seit 2026-08-05 umgesetzt und im vollständigen headless UI-Lauf
verifiziert. Jeder Hauptabschnitt besitzt einen sichtbaren Drag-Griff, der
dieselbe Verschiebung per Pfeiltasten, Pos1 und Ende zugänglich macht. Die App
speichert ausschließlich eine vollständige Permutation der drei bekannten IDs;
fremde, doppelte oder alte Werte fallen vollständig auf
„Geräte → Freigaben → Favoriten“ zurück. G4 ist damit abgeschlossen.

Nachtrag 2026-08-06: Die optionale macOS-Anreicherung durch `diskutil info` läuft
außerhalb des Tauri-/AppKit-Threads und besitzt pro Volume eine harte
Ein-Sekunden-Grenze. Ein real beobachtetes virtuelles Volume hielt den Erststart
zuvor 6,84 Sekunden fest; nach der Grenze bleibt es neutral sichtbar und die Panes
sind währenddessen bereits bedienbar.

Nachtrag 2026-08-07: macOS-Mounts mit `MNT_DONTBROWSE` bleiben außer dem Systemroot
unabhängig von ihrer lokalen oder Netzwerk-Klassifizierung aus der Seitenleiste.
Damit verschwindet insbesondere der nicht nutzbare `auto_home`-/`autofs`-Platzhalter,
ohne sichtbare Netzwerkfreigaben oder normale Volumes unter `/Volumes` zu filtern.

1. Neue Kategorie „Geräte": interne/externe Volumes und gemountete Freigaben
   mit freiem Speicher (macOS `statfs`; Linux `/proc/self/mountinfo` und begrenzte
   GNU-stat-Hilfsprozesse; Pseudo-FS gefiltert), Auswerfen-Knopf für externe/Netz-Volumes (macOS `diskutil`,
   Linux `gio mount -u` am konkret gewählten Mountpfad, fail-closed mit
   Fehlermeldung).
2. Struktur: Geräte → Freigaben → Favoriten (Ordner und gespeicherte
   Verbindungen); Queue über den Fensterkopf, Quarantäne über ihren eigenen
   Bedienweg.
3. Kategorien-Reihenfolge per Drag umsortierbar und lokal persistiert
   (Mechanik analog zum bestehenden Favoriten-Drag).

## G5 — Netzwerk-Entdeckung für „Freigaben"

mDNS/Bonjour-Browsing (`_smb._tcp`, `_sftp-ssh._tcp`, `_webdav._tcp`) für eine
marktübliche Server-Liste. Eigenständige Phase, standardmäßig aktivierbar, aber
abschaltbar (sendet aktiv Anfragen ins LAN); Klick auf einen Fund öffnet den
bestehenden Verbindungs-Editor vorbefüllt. Kein NetBIOS-Legacy-Scan.

Umgesetzt seit 2026-08-05 und auf macOS sowie Linux headless verifiziert. Der
Core sendet ausschließlich die drei festen PTR-Fragen in ein begrenztes
Suchfenster, parst komprimierte DNS-Namen mit harten Größen-/Schleifengrenzen und
verknüpft nur zugehörige SRV-Antworten mit gültigem `.local`-Host. Die Suche ist
standardmäßig eingeschaltet, in „Freigaben“ sichtbar abschaltbar und manuell
erneuerbar; die Wahl wird lokal gespeichert. Ein Fund füllt nur Anzeigename,
Host und Port des vorhandenen Editors vor. Anmeldung, Share/Root und jede
Identitätsprüfung bleiben bewusst leer. Ein realer macOS-Socketgegenlauf, die
Core-Suite auf beiden Plattformen und der vollständige UI-Lauf sind grün. Jeder
Fund ist ein echter Button und öffnet den Editor daher per Klick, Enter oder
Leertaste. G5 ist abgeschlossen.

Nachtrag 2026-08-06: `src-tauri/Info.plist` bindet für das ausgelieferte macOS-Bundle
eine konkrete `NSLocalNetworkUsageDescription` und exakt `_smb._tcp`,
`_sftp-ssh._tcp` sowie `_webdav._tcp` unter `NSBonjourServices`. Quell-Preflight,
App-Test und Artefaktprüfer halten diese Deklaration fest.

## G6 — Informationen zu Dateien und Ordnern

Umgesetzt seit 2026-08-26: „Informationen“ ist für die aktuelle Ein- oder
Mehrfachauswahl über das Datei-Menü, `⌘I` beziehungsweise `Ctrl+I` und das
Kontextmenü erreichbar. Der Dialog zeigt Pfad, Provider/Profil, Art, Größe,
Änderungs- und Erstellungszeit, Eigentümer, Gruppe, POSIX-Berechtigungen und
vorhandene Hardlink-Anzahl. Er übernimmt ausschließlich die Werte des gebundenen
Pane-Listings; ein Providerfeld ohne belastbaren Wert erscheint als „—“.

Ordnergrößen und rekursive Datei-/Unterordnerzahlen entstehen erst nach „Berechnen“.
Der Dialog verwendet dafür denselben fenster-, pane-, Profilrevisions- und
sitzungsgebundenen Größenlauf wie die Größenspalte. Vollständige Resultate fließen
in die Ein- oder Mehrfachsumme ein. Partielle Läufe zeigen ihre Teilbaumfehler,
werden aber nie als exakte Summe ausgegeben. „Neu berechnen“ umgeht den
kurzlebigen Sitzungscache bewusst. Die Darstellungsoptionen besitzen zusätzlich
den gut auffindbaren Menüeintrag `⌘J` beziehungsweise `Ctrl+J`; der bisherige
Drei-Linien-Knopf bleibt als pane-eigener Direkteinstieg erhalten. Damit ist auch
G6 abgeschlossen.

## Bewusst verworfen / verschoben

- SCM-Status: fremder Scope, Performance-Falle, stille Repo-Introspektion.
- „Zuletzt geöffnet" (atime): auf modernen Systemen (`noatime`/`relatime`)
  systematisch falsch — eine plausibel lügende Spalte ist schlechter als keine.
- „Hinzugefügt am", Tags: macOS-only-Metadaten; höchstens später als
  macOS-only-Anzeige.
- Scope „Dieser Ordner und Unterordner": Vererbungssemantik erst, wenn sich
  per-Ordner-Einstellungen bewährt haben.
- Symbolvorschau (Thumbnails) und Gruppieren: wertvoll, aber je eigene große
  Phase (Thumbnail-Pipeline durch den Core; Gruppierung × Virtualisierung).
- Freie Schrift-/Icon-/Textgrößenwahl: ersetzt durch Dichte- und Textstufen.

## Prüfung fehlender SFTP-Dateigrößen

Die native Abnahme unterscheidet unbekannte, leere und bekannte Dateien in
Informationen sowie im CLI-Text-/JSON-Listing. Dunkles und helles Theme sowie ein
frischer App-Neustart sind mit künstlichen Protokollantworten geprüft; der Server
entfernt ausschließlich das optionale Größenfeld eines benannten Eintrags.
Die Anzeige öffnet keinen Dateiinhalt und verändert keine Quelldatei. Browserfälle
belegen zusätzlich Sortierung, gemischte Auswahlen und die gemeinsame Bilanz mit
beobachteten SFTP-Ordnergrößen. Core, Desktop, CLI, Formatierung, Clippy und Fuzz-
Übersetzung sind auf macOS und in einer isolierten Linuxkopie geprüft.
