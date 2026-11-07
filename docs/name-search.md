# Rekursive Namenssuche

Stand: 2026-10-23. Die Suche startet über die Lupe neben dem Schnellfilter einer
Pane oder mit `⌘/Ctrl+Shift+F` aus der aktiven Pane. Sie durchsucht den aktuellen
Ordner und dessen Unterordner nach einem wörtlichen Namensteil. Groß-/Kleinschreibung
und versteckte Namen/Ordner sind wählbar. Dateiinhalte werden nicht geöffnet.
Kanonisch gleichwertige Unicode-Schreibweisen finden dieselben Namen. Nur der
Vergleich wird normalisiert; Trefferpfade und Dateiaktionen behalten die Originalnamen.

Treffer zeigen den relativen Pfad und öffnen ihren übergeordneten Ordner mit dem
Namen als Auswahl. Dabei wird der Schnellfilter geleert; versteckte Treffer werden
sichtbar. Ein inzwischen entfernter Treffer ist kein Ausführungsbeleg: Die normale
Pane-Navigation liest den Ordner frisch. Ein Wechsel der Ausgangspane oder ihrer
Providerbindung entwertet die alte Navigation.

## Lesender Vertrag

`core::name_search` enthält einen iterativen Walker und typisierte Ergebnisdeltas.
Lokale und generische Remote-Leser verwenden den bestehenden `Vfs::list_complete`-
Vertrag. SFTP verwendet ausdrücklich dessen typisiertes interaktives Best-Effort-
Listing statt das weiterhin gesperrte strikte SFTP-VFS. Es prüft beobachtete
Elternsymlinks, besitzt aber keine atomare Rootbindung oder Momentaufnahme.
Sitzungsgeneration, Profilrevision und gehaltene Provider-Arc bleiben über die
bestehende gemeinsame Lese-Endpunktbindung geprüft. Ein Wechsel beendet die Suche.
Die Suche führt keine Capability-Proben oder andere Mutationen aus.

Symlinks können selbst einen Namenstreffer liefern, werden jedoch nie rekursiv
betreten. Nicht eindeutig dekodierte oder als einzelner Name nicht adressierbare
Einträge werden mit dem sicheren Elternpfad bilanziert. Ein Listingfehler oder ein
als unvollständig markiertes Listing bleibt eine sichtbare Lücke; unabhängige
Unterordner können weiterhin Treffer liefern. Ergebnisse sind beobachtete Bestände,
keine atomare Aussage über einen während der Suche veränderlichen Gesamtbaum.

## Aufwand, Abbruch und Fenster

Der erste Schnitt begrenzt einen Lauf auf 250.000 geprüfte Einträge, 25.000
Verzeichnisanfragen und 5.000 Treffer. Der iterative Stack enthält höchstens 25.000
Verzeichnisse, die Tiefe beträgt höchstens 512 Ebenen. Gesammelte Trefferpfade und
wartende Verzeichnispfade erhalten jeweils 16 MiB Pfadbudget. Erreichte Grenzen
beenden den Lauf ausdrücklich als unvollständige Teilbilanz. Alle Fehler werden
gezählt; höchstens 100 pfadgenaue Details werden gehalten und angezeigt. Die
Darstellung zeigt alle gespeicherten Treffer auf Seiten mit je 100 Zeilen.

Ein Provider liefert jeweils weiterhin ein vollständiges einzelnes Listing im
Speicher; dessen Parser-/Antwortgrenzen gelten zusätzlich. Es wird kein gesamter
Verzeichnisbaum gehalten. Die Suche startet nur eine Listinganfrage gleichzeitig.
Fortschrittsereignisse werden auf etwa zehn pro Sekunde gesammelt; 100 neue Treffer
oder ein Terminalzustand werden sofort ausgegeben. Dabei gehen keine Treffer oder
Fehlerdetails verloren.

Jedes Fenster hat einen eigenen Suchworker und ein eigenes Abbruchsignal. Die
Dateioperations-Queue, deren Single-Flight-Schranke und andere Fenster bleiben
unabhängig. Erst nach Ende des vorigen Workers kann dasselbe Fenster erneut suchen;
ein Schließen des Suchdialogs fordert dessen Abbruch an. Abbruch wird zwischen
Einträgen und vor/nach Providerlistings geprüft. Ein laufender blockierender
Provideraufruf wird nicht gewaltsam beendet; SFTP hat die bestehende Lesefrist von
60 Sekunden pro Listing. Das Terminalevent gibt den eigenen Suchslot vor dem
Versand frei. Clienttoken und Lauf-ID verhindern, dass verspätete Antworten einen
späteren Dialog oder Folgelauf überschreiben.

## Prüfstand

Gezielte Coretests prüfen Rekursion, Symlinks einschließlich Elternpfadtausch,
unsichere Namen, Listinglücken, Abbruch und technische Grenzen. Tauri-Tests prüfen
Fenster-/Laufbindung, verschachtelte Requestfelder und verlustfreies Drosseln samt
unmittelbarer Terminalbilanz. Die dedizierte Headless-Browsersuite prüft die echte
Oberfläche mit kontrollierten Antworten für lokale, SFTP-, FTP/FTPS-, WebDAV- und
SMB-Panes, Seitenwechsel, Teilstatus, Navigation, Abbruch, verspätete Startantworten
und Fokus. Sie ist in `pnpm test:ui` eingebunden.

Native macOS-/Linux-Läufe prüfen lokale und echte SFTP-Suche samt Navigation und
anschließender Vorschau des ausgewählten Treffers. Die anderen Provider verwenden
in dieser Oberfläche kontrollierte Browserantworten; dieser Schnitt behauptet
keine neue native Ganzabnahme aller Provider. [Gemeinsamer Prüfstand](roadmap-2026-10-23.md).
