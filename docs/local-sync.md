# Normaler lokaler Sync

Stand: 2026-10-18. Der normale lokale Start unterstützt AddOnly und Update über
eine backendgebundene Vorbereitung und die eigene Fensterqueue.

## Bedienung und Wirkung

Die automatisch aktualisierte Vorschau liest ausschließlich. „Testlauf simulieren“
reiht ausdrücklich eine Simulation ohne Schreibwirkung ein, auch innerhalb einer
markierten Testsandbox. „Sync-Ausführung vorbereiten“ prüft den echten lokalen
Auftrag und zeigt Quelle, Ziel, neue Dateien, Ersetzungen, Metadatenänderungen,
Auslassungen und Platzbedarf vor dem Start.
Vorschau und Simulation binden ein unterhalb der Sandbox eingehängtes Zielvolume
gesondert; dessen Fähigkeiten stammen nicht vom äußeren Sandbox-Mount.

AddOnly fügt ausschließlich Fehlendes hinzu. Es benötigt keine Laufphrase, keine
Quarantäne und keine Schreibrechte im Geschwisterordner des Ziels. Der vorhandene
Bestand bleibt erhalten. Update verlangt ContentHash; der schnelle Vergleich ist
in diesem Modus sichtbar deaktiviert. Vor Ersetzungen und Änderungen bestehender
Metadaten wird die konkrete Wirkung bestätigt. Bei abgesicherten Zielen gehen alte
Dateiversionen vor ihrer Ersetzung in die manifestierte Quarantäne. Die begrenzte
Best-Effort-Klasse erklärt stattdessen jede dauerhafte Ersetzung ausdrücklich.
Ein Sync bleibt kein Backup.

Vorbereitung, laufender Status, Ergebnis und Queue unterscheiden abgesicherte
Ausführung, Best-Effort-Ausführung, echten Sandbox-Test und Simulation.
Abbruch bilanziert bereits abgeschlossene Kopien;
die ursprünglichen Endpunkte und der Versuch bleiben auch nach Navigation gebunden.

## Backendbindung und Neustart

Das Frontend erhält nur die darstellbare Vorschau. Es liefert beim Enqueue deren
backendseitigen Digest und die wirkungsbezogene Zustimmung zurück. Quelle, Ziel,
Optionen und ausführbarer Plan stammen aus der gehaltenen Vorbereitung. Sie hält
Root- und vorhandene Eltern-Deskriptoren, Mount-Identitäten und vollständige
Listing-Proofs. Der Digest bindet zusätzlich die konkrete Neunamen-Commitklasse,
Datei- und Verzeichnis-Flushzusagen sowie die Zahl dauerhafter Ersetzungen. Vor
Bestätigung und Wirkung werden Roots, Fähigkeiten, Listings, Plan, Ablaufzeit und
freier Platz erneut geprüft. Ein Zielbaum mit abweichenden eingehängten Untervolumes
wird vor Wirkung abgelehnt; ein Rootpfad allein erteilt dafür keine Freigabe.

Vorbereitung, Verwerfen und Enqueue laufen außerhalb des GUI-Threads. Ein verworfener
Scan kann keinen neueren Slot ersetzen. Die Reihenfolge von Journalcommit und
RAM-Beleg verhindert, dass ein schneller Worker einen gerade bestätigten Auftrag
ohne dessen gehaltene Roots startet. Jede Fensterqueue besitzt ihre eigenen Belege.

Journalversion 23 speichert `LocalSync` als geheimnisfreie Absicht. Ein optionaler
Ausführungshinweis dient ausschließlich der Anzeige. Permits, Handles
und Zustimmung werden nicht persistiert. Fehlt der RAM-Beleg nach einem Neustart
oder einer erneuten Prüfung, bereitet der Worker frisch vor und wartet auf einen
neuen Wirkungsdialog. Auch bei gleichem Plan wird kein dauerhaftes Schreibrecht
aus dem Journal abgeleitet. Alte `Sync`-Einträge ohne Sandbox bleiben Simulationen.

`Running` wird vor der ersten Mutation dauerhaft geschrieben. Der terminale
Queuecommit folgt vor dem Abschlussaudit; Fehler erhalten die vorhandene
Recoverybilanz. Privates C2-Staging gehört zur verursachenden Fensterqueue und
Operation. Nach einem Prozessabbruch bleiben unvollständige Zwischenstände als
Recoverypfade sichtbar; das Journal erteilt kein Recht zur blinden Wiederholung.
Ist privates Staging auf dem Ziel nicht verfügbar, registriert der Core vor der
Tempanlage einen privaten Recoveryintent im geschützten Queueverzeichnis. Nach
Neustart zeigt nur die verursachende Queue den Kandidaten, einen möglichen
AppleDouble-Begleiter und das zu prüfende Endziel. Dieser Index erlaubt weder
Wiederholung noch Löschen und wird erst nach belegtem Abschluss und Abwesenheit
der Zwischenstände entfernt. Eine schwache Ausführung ohne geschützten
Recoverybereich wird vor ihrer ersten Schreibwirkung abgelehnt.
Ein später belegter Drift oder eine Kollision führt vor weiterer
Wirkung zu einer konkreten Fehlermeldung und neuer Vorbereitung.

## Begrenzte Best-Effort-Ziele

Die Klasse wird am gehaltenen konkreten Ziel bestimmt. macOS-exFAT besitzt keinen
atomaren No-Replace-Commit; Linux-exFAT mit nativem Treiber besitzt ihn. Fehlende
Datei- und Verzeichnis-Flushzusagen werden davon unabhängig angezeigt. Auch ohne
Zusage versucht der Core beide Flushs. Nur maschinenlesbar nicht unterstützte
Operationen dürfen im bestätigten schwachen Profil weiterlaufen; I/O-, Platz-,
Rechte- und unklare Wirkungsfehler bleiben Fehler. Ein bereits wirksamer Rename mit
anschließendem Syncfehler gilt niemals als mutationsfreie Ablehnung.

Neue Dateien und dauerhafte Ersetzungen verwenden dieselben Commithelfer wie
interaktive Dateioperationen: vollständiger frischer Elternbestand, konservative
Case-/Unicode-Kollisionsprüfung, genau ein Rename und Inhalts-/Namens-/Typ-Readback.
Der Temp darf dabei einen anderen Elternordner besitzen. Nach verlorener Antwort
folgt nur Zustandsklärung, kein mutierender Blind-Retry. Das verbleibende Rennen
zwischen Zielcheck und gewöhnlichem Rename wird vorab erklärt. Eine erkennbare
späte Kollision oder Inhaltsdrift stoppt vor weiterer Wirkung.

Bei einer schwachen Update-Ausführung nennt der Dialog Zahl und Ziele der
dauerhaften Ersetzungen zuerst. Die Zustimmung erklärt ausdrücklich, dass für
diese alten Versionen keine Quarantäne oder Wiederherstellung bereitsteht.
Vorschau und Ergebnis unterscheiden geplante und tatsächlich abgeschlossene
dauerhafte Ersetzungen. ContentHash und die Grenze von 100 Ersetzungen bleiben
verbindlich. Additionen benötigen auch hier keine Zufallsphrase.

`Portable` bilanziert nicht darstellbare POSIX-Rechte pfadgenau. Die Profilzeile
nennt die fehlende Rechteübertragung; Vorschau und Abschluss zeigen auch unter
`Portable` die Auslassungszahl. `NativeBestEffort` überträgt nur die am Ziel
nachgewiesenen Metadatenklassen; eine ACL ohne passenden
Rechtevertrag wird ausgelassen. `NativeStrict` wird niemals still herabgestuft.
Bei schwachen Commits bleiben Case-/Unicode-äquivalente Namen sichtbare Konflikte.
Ein abgesicherter lokaler Commit prüft zusätzlich per Stat, ob die andere
Schreibweise tatsächlich einen vorhandenen Namen trifft: Auf case-sensitivem
ext4 bleiben verschiedene Schreibweisen eigenständige Dateien. Unabhängige
Additionen bleiben ausführbar.

## Grenzen

Mirror verwendet weiterhin den getrennten Realmodus-Assistenten mit Laufphrase,
Backupbestätigung, vollständigem Zweitscan und unveränderten Mengengrenzen.
Dieser normale Einstieg kann keinen Mirror-Permit erzeugen. Eine Umgebungsvariable
allein erteilt auch AddOnly/Update keine Freigabe.

Abgesicherte Ersetzungen behalten den vorhandenen sicheren Geschwisterbereich
für Quarantäne und Restore-Probe. Dateisystemwurzel,
Benutzerordner selbst und Git-Repositoryziele bleiben für diese Ersetzungsklasse
gesperrt. Reine Additionen und ausdrücklich dauerhafte schwache Ersetzungen
benötigen diesen Geschwisterbereich nicht. Die separate strikte lokale Vorbereitung,
Remote-Sync, Mirror und bidirektionale Ausführung übernehmen die neue Freigabe
nicht. Andere Dateisystemtreiber und ein Stromausfall sind durch die realen
Image-Gegenläufe nicht belegt.

GUI-Schreibtests verwenden weiterhin ausschließlich den markierten Sandbox-Launcher.
Auch die Vorbereitung mit eigener Restore-Probe bleibt dort an denselben gehaltenen
Marker-Root gebunden. Der normale Produktweg wird nicht durch die Testvariable
aktiviert.

## Prüfung und Reviewzuordnung

Gezielte Corefälle prüfen Additionen ohne Phrase und Geschwister-Schreibrecht,
leere und gleiche Bäume, überlappende Roots und den Austausch beider Wurzeln,
Update mit gleicher Größe/Zeit und anderem Inhalt, Zustimmung, Quarantäne,
Drift, späte Kollision, Abbruch vor Wirkung und nach Teilwirkung, Queue-Reopen
und einen echten Kindprozessabbruch
mitten in der Übertragung samt Wiederfinden des privaten Stagings. Appfälle prüfen
Vorbereitungsbesitz, Fenstertrennung und Fehlercodes. Browserfälle prüfen beide
Themes bei kleinem Fenster, Tastatur/Escape, ContentHash, Simulation, den
Reopen-Dialog und verspätete Antworten. Vollständige Core-/App-/CLI-Gates sowie
der isolierte Linux-Gegenlauf ergänzen diese Fälle.

Eigene APFS-/ext4- und exFAT-Images auf beiden Plattformen belegen seit 2026-10-17
Additionen, Update bei gleicher Größe/Zeit und anderem Inhalt, Quarantäne oder
dauerhafte Ersetzung, Metadatenprofile, normalisierte Namenskollisionen und
Elterntausch während der Übertragung. Ein tatsächlich abgebrochener Kindprozess
und Queue-Reopen finden schwache Temp-/AppleDouble-Reste wieder, ohne sie blind zu
wiederholen. Die Mounttests laufen nur nach ausdrücklichem Opt-in mit eigenem
Marker; ihre normalen Gateeinträge bleiben ignoriert. Vollständige Headless-Gates
auf macOS und in einer isolierten Linuxkopie sind ergänzend grün.

Dies ist eine Deltaprüfung des normalen lokalen Starts, kein neuer Vollreview der
übergeordneten CodeQA-Bereiche. Die historischen Bereichshashes werden dadurch
nicht aktualisiert; die offenen Ganzbereiche und der Querschnitt bleiben offen.
Die native macOS-Abnahme im markierten Testbereich belegt beide Themes bei
760 × 480 Pixeln, Escape und Tastaturstart, AddOnly mit Unterbaum, wirkungsfreie
Simulation, Update bei gleicher Größe/Zeit, Quarantäne und Drift, reine
Metadatenänderungen sowie frische Bestätigung nach echtem App-Neustart aus
`Pending` und bereits wartender Zustimmung. Die verlorene lokale Vorbereitung
wird als solche erklärt und erst über „Neu prüfen“ erneut vorbereitet. Der lange
Dialog ist per Tastatur scrollbar. Ein Abbruch nach 25 von 2.000 Dateien erhält
alle Quellen; Navigation und eine parallel abgeschlossene F5-Kopie im zweiten
Fenster bleiben möglich. Die interaktive Linux-GUI-Abnahme ist weiterhin offen;
der isolierte Linux-Gegenlauf prüft den Core und die App headless.

Die native AP03B-Abnahme vom 2026-10-18 belegt zusätzlich den abgesicherten
APFS-Start und exFAT-Best-Effort-Additionen, beide Themes bei 760 × 480 Pixeln,
Escape und Tastaturstart, sichtbare Portable-Auslassungen, dauerhafte Update-
Ersetzung bei gleicher Größe/Zeit und den Driftstopp vor Wirkung. Nach echtem
App-Neustart aus `Pending` sowie erneut aus wartender Zustimmung bleibt die neue
Datei bis zur frischen Bestätigung aus. Die Simulation gegen das eingehängte
exFAT-Ziel bleibt wirkungsfrei. Der abschließende unveränderte Produktbuild bestand
den vollständigen Block; eigenes App-/Image-Cleanup und Ressourcenfreigabe sind
bestätigt.
