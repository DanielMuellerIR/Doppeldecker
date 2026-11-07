> Historischer Stand bis 2026-10-31. Keine aktuelle Aufgabenliste oder Releasefreigabe.
> Aktuelle Planung: [Roadmap](roadmap.md) und [Backlog](../backlog.md).

# Roadmap bis zur Produktreife

Stand: 2026-10-28; ältere Stufenabsätze sind Implementierungschronik.

Diese Datei ist die zentrale, verbindliche Übersicht für noch offene Produktarbeit.
Detailentwürfe erklären die Umsetzung; `AGENTS.md` enthält dauerhafte Regeln,
`backlog.md` den belegten Reststand. Historische Stufenbelege unten ersetzen keine
aktuelle Providerfreigabe; maßgeblich ist die [Fähigkeitsmatrix](function-audit.md).

## Beauftragter Ausbau der Alltagstauglichkeit

Ziel ist ein komfortabler, umfassender, performanter und effizienter
Two-Pane-Dateimanager auf macOS und Linux. Normale Copy-/Move-/mkdir-/Rename-Aktionen
sollen direkt aus den Panes ausführbar sein, mit passenden Konflikt- und
Löschentscheidungen. Eine allgemeine Vorschaupflicht oder pauschale
Sicherheitsassistenten gehören nicht zum Ziel. Vorschau unterstützt optional oder
ist konkret für Sync und komplexe Stapelaktionen vorgesehen.

**Nächster verbindlicher Ausbau: gewöhnlicher Inhalts-Sync (2026-10-28).**
Überall, wo gewöhnliche Dateiaktionen schreiben können, soll auch das
Synchronisieren von Dateien möglich sein. Das umfasst SFTP als Quelle und Ziel
sowie FTP/FTPS, WebDAV und SMB als Ziele für Ergänzen, Aktualisieren und Spiegeln.
Die stärkeren bisherigen Sync-/Quarantäneverträge sind kein allgemeines
Eintrittskriterium. Fehlende atomare Bindung, Flushzusagen oder Quarantäne verlangen
eine passende Best-Effort-Ausführung mit konkreter Wirkungsbestätigung,
Driftprüfung, Readback und ehrlicher Teilbilanz. Unvollständige Listings und
ausgelassene Problemobjekte sperren weiterhin die davon abhängigen Entfernungen.
Darstellbare Metadaten bleiben Teil der Übertragung; ACLs und ähnliche Klassen
dürfen nur mit einer konkreten technischen Ursache ausgelassen werden.
Dieser Ausbau ist beauftragt, aber noch nicht als verfügbar abgenommen.

**SFTP-Schreibfunktionen implementiert (2026-10-20).** Gewöhnliche
SFTP-Dateiaktionen umfassen Upload/Kopie auf den Server, Ordner anlegen,
Umbenennen/Verschieben, bestätigtes Ersetzen und dauerhaftes Löschen.
Als Funktions- und Bedienreferenz dient ForkLift. Diese Grundfunktionen
hatten Vorrang vor weiteren Komfort- und WebDAV-Erweiterungen.

Die Umsetzung darf in getrennten, bedienbaren Schritten erfolgen. Sie braucht
eigene Verträge für interaktive Operationen: die stärkste verfügbare Absicherung,
sichtbare Best-Effort-Wege bei begrenzbaren Restrisiken und klare Konflikt-,
Abbruch- und Fehlerzustände. Fehlende Garantien für strikten Sync dürfen normale
SFTP-Dateioperationen nicht pauschal verhindern. Nicht verlässlich begrenzbare
Schreibwirkungen und mutierende Blind-Retries bleiben gesperrt. Für normale
SFTP-Mutationen ist das verbleibende Rennen mit gleichzeitiger serverseitiger
Elternpfadumleitung ausdrücklich als Best Effort akzeptiert: Pfade lexikalisch
beschränken, beobachtete Symlinks ablehnen, Ergebnis prüfen und Restrisiko ehrlich
anzeigen; keine atomare Rootgarantie behaupten.
[Schreibvertrag und aktueller Prüfstand](sftp-writes.md). Abnahmekriterium: echte
GUI-Schreibläufe auf macOS und Linux gegen frische künstliche SFTP-Roots,
einschließlich Kollisionen, Verbindungsabbruch und unklarem Serverausgang.

Die Empfehlungen des [14-Punkte-Audits](usability-audit-2026-10-20.md)
sind im aktuellen Implementierungsstand umgesetzt: gewöhnliches Remote-Löschen
und WebDAV-/SMB-Ersetzen, ein Remote-Lesedurchlauf für Standardsnapshots,
Streaming statt vollständigem Vorhashen, ressourcenbezogene statt fester
Transfergrenzen, ein nachladender Konfliktdialog, Wirkungsbestätigung statt
Routinephrasen, Mengenwarnungen statt pauschaler Sync-Sperren und explizites
Zurückspeichern bearbeiteter Remote-Dateien. Die [Fähigkeitsmatrix](function-audit.md)
trennt diese normalen Aktionen von strengeren Sync-Zielverträgen.
Performanceprüfungen betrachten auch Netzwerkbytes und Anfragezahl.

Beim nächsten GitHub-Release wird die Projektwebsite mit dem tatsächlich
veröffentlichten Funktionsumfang abgeglichen. Das allgemeine Vorschauversprechen
wurde am 2026-10-20 bereits entfernt.

Der [Review vom 2026-10-16](code-review-2026-10-16.md) beschreibt die weiteren
Produktziele; die oben festgelegte SFTP-Priorität geht deren Reihenfolge vor.
Die Core-/Desktop-/Frontend-Schichtung bleibt erhalten. Jeder Schritt
liefert einen bedienbaren Ablauf mit passenden Plattform- und Fehlerfallprüfungen:

1. **Umgesetzt am 2026-10-16:** Hintergrundarbeit im selben Fenster: Navigation, Tabs, Auswahl, Informationen
   und weitere Aufträge bleiben während einer Übertragung nutzbar. Dazu eine echte
   lokale Datei-Zwischenablage mit Mehrfachauswahl, Menüs und üblichen Kürzeln.
   Vertrag und Plattformgrenzen: [Hintergrundarbeit](background-file-work.md).
2. **Umgesetzt am 2026-10-16:** Vergleichbare Konfliktdaten, zulässige Erstbaseline-
   Entscheidungen und beide Versionen behalten, einschließlich gewöhnlichem
   Copy/Move. Vorschau, Simulation und echte Ausführung sind klar beschriftet.
   Plattform- und native Abnahme: [Konfliktvertrag](conflict-decisions.md).
3. **Abgesicherter Start umgesetzt und nativ abgenommen:** Normaler lokaler AddOnly-/Update-Sync
   mit wirkungsbezogener Zustimmung und eigenem Simulationsstart. Reine Additionen
   brauchen keine Quarantäneprobe. Der begrenzte Best-Effort-Weg für schwächere
   lokale Zielgarantien ist ebenfalls umgesetzt und nativ belegt.
   [Vertrag und Prüfstand](local-sync.md).
   GUI-Schreibtests bleiben im markierten Testbereich.
4. **SFTP-Inhalte umgesetzt:** Öffnen über geprüfte Momentaufnahmen und
   lokale Downloads laufen als eigene Best-Effort-Leseklasse ohne feste Datei-/
   Auswahl-/Baumgrößen- oder Tiefengrenzen. Originalformat und expliziter Rückupload
   ergänzen die passive Ansicht.
   Rekursive additive Auswahl bindet den begrenzten Baumumfang und seine
   Auslassungen. mkdir, Rename, Upload, Move, Ersetzen und Dauerlöschen sind
   inzwischen als eigene [Schreibklasse](sftp-writes.md) implementiert.
   [Vertrag und Grenzen der Leseklasse](sftp-contents.md).
   Gewöhnliches WebDAV-mkdir und -Rename sind als bedingte Einzelwirkungen von
   strikten rekursiven Löschfähigkeiten und künstlichen Löschproben entkoppelt.
   Additive Datei- und begrenzte Baumuploads aus lokalen Panes verwenden ebenfalls
   einen eigenen bedingten Weg ohne Collection-/Löschprobe. Gewöhnliches Ersetzen
   verwendet ebenfalls diese interaktive Klasse; Verträge: [Ordneranlage](webdav-folder.md), [Umbenennung](webdav-rename.md)
   und [Inhaltsuploads](webdav-upload.md).
5. Problematische Namen und Metadaten pfadgenau auslassen und bilanzieren.
   Unabhängige Dateien bleiben nutzbar; Namensexaktheit und Readback auf
   case-sensitiven sowie normalisierenden Servern konsistent behandeln.
   Ein weiterer Teil ist am 2026-10-23 umgesetzt: Lokale WebDAV-Baumkopien lassen
   vollständige mehrdeutige Case-/Unicode-Namensgruppen aus, während unabhängige
   Kinder übertragen werden. Der beobachtete Bestand bleibt driftgebunden.
   Additive SFTP-Baumdownloads verwenden dieselbe Gruppenauslassung; vollständige
   READDIR-Belege binden weiterhin auch die ausgelassenen Namen und Attribute.
   Die Größenbeobachtung zählt alle Servernamen. Direkte Mehrfachauswahlen
   verwenden jetzt ebenfalls ganze Gruppenauslassungen; unabhängige Inhalte bleiben
   übertragbar und ausgelassene Mitglieder werden vor weiteren Wirkungen frisch
   geprüft. Additive lokale FTP-/FTPS-Kopien übernehmen dieselbe Gruppenauslassung
   und erkennen auch neu entstandene Gruppen vor einem Queue-Wiederanlauf.
   Additive lokale SMB-Kopien verwenden denselben Quellenvertrag für direkte und
   rekursive Gruppen. Additive [FTP-/FTPS-Downloads](ftp-contents.md) ergänzen
   dieselbe Gruppenauslassung mit vollständigen MLSD-Fakten und restartstabilem
   Baumbeleg. Additive [WebDAV-Downloads](webdav-contents.md) ergänzen ganze
   Namensgruppen, vorbereitete starke ETags und bedingtes GET mit
   Abschlussprüfung. Additive [SMB-Downloads](smb-contents.md) binden dieselben
   Gruppen sowie Dateifakten am gehaltenen Handle bis zum tatsächlichen EOF.
   Weitere Provider-/Metadatenklassen bleiben eigene Ausbauarbeit;
   [WebDAV-Umfang](webdav-upload.md),
   [FTP-/FTPS-Umfang](ftps-stou-transport.md#additive-lokale-kopien-und-namensgruppen),
   [SMB-Umfang](remote-providers-design.md) und [SFTP-Umfang](sftp-contents.md).
6. **Schnellfilter umgesetzt am 2026-10-22:** Jede Pane filtert ihren geladenen
   Bestand nach Name/Endung und Dateityp, mit Trefferzahl und Escape zum Leeren.
   Sichtbare Auswahl bleibt objektgebunden; ausgeblendete Zeilen sind keine
   Dateiaktionsziele. Beim Ordner- oder Tabwechsel wird der Filter geleert,
   Aktualisieren desselben Ordners erhält ihn. Browserprüfungen decken Aktionen,
   Auswahl, Sortierung, leere Treffer und Virtualisierung ab. Die abbrechbare
   rekursive Namenssuche mit sichtbaren Teilbilanzen ist als eigener Pane-Dialog
   umgesetzt; Grenzen und Abbruchvertrag stehen im [Suchvertrag](name-search.md).
   Native lokale und SFTP-Abläufe sind auf macOS und Linux geprüft;
   [gemeinsamer Prüfstand](roadmap-2026-10-23.md).
7. **Grundvorschau implementiert am 2026-10-23:** F3 zeigt lokale/Remote-Dateien
   als UTF-8-Text oder statisches PNG/JPEG. Größen- und Pixelgrenzen betreffen
   ausschließlich die Vorschau; die System-App bleibt als Alternative erreichbar.
   [Darstellungs-, Lese- und Abbruchvertrag](file-preview.md).
   Remote-Bearbeitung mit geprüftem Rückupload und Versionskonflikten ist am
   2026-10-20 umgesetzt; die Arbeitskopie öffnet sich in der externen System-App.
8. **Lokales Duplizieren und Stapelumbenennen implementiert am 2026-10-23:**
   Beide Abläufe zeigen konkrete backendgebundene Namen und behandeln Kollisionen.
   Duplikate laufen über die Fensterqueue; Stapelumbenennungen zeigen eine
   zeilenweise Teilbilanz. Beide Abläufe sind nativ auf macOS und Linux geprüft.
   Verträge und Grenzen: [Duplizieren](duplicate.md) und
   [Stapelumbenennung](batch-rename.md). Lokale ZIP-Erzeugung ist als eigener
   Vorschau-/Fensterqueue-Ablauf mit Versionsbindung, Streaming und Recovery
   umgesetzt und auf macOS und Linux nativ geprüft ([Vertrag](archive-pack.md)).
   Die [Archivansicht](archive-read.md) bietet virtuelle Ordner, genaue
   Duplikat-/Problemzeilen, Seiten und passive Text-/Bildvorschau über den
   budgetierten Lesekern. Entfernte ZIP-Dateien werden zunächst vollständig als
   geprüfte Momentaufnahme geladen; die Ansicht nennt diese Herkunft. Eine
   gewählte reguläre Datei, das ganze Archiv oder ein geöffneter virtueller
   Unterbaum daraus lässt sich mit persistierter Snapshotbindung in die lokale
   Gegenpane entpacken, ohne beim Claim erneut vom Server zu laden.
   Das [lokale Entpacken](archive-extract.md) ergänzt ganze
   Archive, einzelne gewählte Dateien oder geöffnete virtuelle Unterordner zum
   Ordner der anderen Pane mit gebundener Konfliktentscheidung und persistenter
   Teilbilanz. SFTP-, FTP-/FTPS-, WebDAV- und SMB-Stapel ergänzen die Namensplanung mit
   gebundenen Einzelwirkungen und providergebundenen Stopgrenzen bei unklarem
   Ausgang oder Bindungsverlust.
   SFTP, FTP/FTPS und WebDAV verwenden Best Effort, SMB serverseitigen No-Replace-Zielschutz.
   Optionale Nummerierung ergänzt Start, Schritt, Mindeststellen und die Position
   vor oder nach dem Namen in festgehaltener sichtbarer Reihenfolge. Gewöhnliche
   äußere Leerzeichen lassen sich vor den Namensregeln ausdrücklich entfernen.
   Lokale, SFTP-, FTP-/FTPS-, WebDAV- und SMB-Ketten geben belegte Ziele in passender
   azyklischer Reihenfolge frei; Vorschau und Nummerierung behalten ihre
   ursprüngliche Reihenfolge. Fehlgeschlagene oder unklare Vorgänger sperren
   ihre Abhängigen. Lokale exakte Zweierzyklen ergänzen einen atomaren Namenstausch
   ohne Ausweichnamen. Lokale reine Case-/Unicode-Schreibweisenänderungen verwenden
   einen atomaren No-Replace-Versuch und einen exakten gemeinsamen Abschlussbeleg;
   Dateisystemverweigerungen bleiben sichtbar. Lokale geschlossene Zyklen ab drei
   Namen verwenden mehrere atomare Austausche und eine eigene Teilbilanz mit
   bestätigtem Namen nach Abbruch oder Fehler. Reine SMB-Schreibweisenänderungen
   ergänzen einen einzelnen No-Replace-Versuch mit exaktem Rohname-/FileID-Beleg.
   Reine SFTP-Schreibweisenänderungen nutzen den gewöhnlichen Best-Effort-Rename
   mit exaktem Rohname-/Attributabschluss; aufgelöste eigene Aliase und
   Serververweigerungen bleiben sichtbar. FTP/FTPS ergänzt exakte MLSD-Namen und
   beobachtbare namensfreie Quellattribute. WebDAV nutzt auch für reine
   Schreibweisen seinen bedingten MOVE mit vollständigem Namespace-/ETag-Beleg.
   Entfernte Namenstausche, [entferntes Packen](remote-pack.md) und entfernte
   Entpackziele bleiben offen. Für lokale Auswahl → ZIP → Remoteziel sind
   Ergebnisbindung, Lebensdauer und gemeinsame Phasenbilanz vor der Umsetzung
   zu schließen; der vorhandene Uploadkern bleibt dafür wiederverwendbar.
9. Die Vollreviewbereiche und der Querschnitt sind
   [abgeschlossen](code-review-2026-10-19-abschluss.md). Neue Funktionsänderungen
   benötigen ihre gezielte Deltaprüfung. Anschließend aktuelle
   Linux-AppImage-/DEB- und macOS-/Updater-Kandidaten vollständig abnehmen,
   Funktionsmatrix und Release Notes aktualisieren und ausgehenden Inhalt samt
   vorgesehener Historie vor einer konkreten Releasefreigabe prüfen. Vor dem nächsten
   GitHub-Release zusätzlich die Produktwebsite mit dem tatsächlich
   veröffentlichten Funktionsumfang und der Bedienung abgleichen. Die Website darf
   keine allgemeine Vorschaupflicht für normale Dateiaktionen versprechen.

Fehlendes fsync, No-Replace-Rename oder Quarantäne führt gemäß
[Produktprinzipien](product-principles.md) zuerst zu einer begrenzten, sichtbar
erklärten Best-Effort-Ausführung. Ein Verbindungsabbruch führt weiterhin zu keinem
mutierenden Blind-Retry. Komfortparität ist kein pauschales Preview-Releasegate.

## Was „fertig“ bedeutet

Doppeldecker gilt für Version 1.0 als fertig, wenn:

1. gewöhnliche lokale und unterstützte entfernte Dateiaktionen auf macOS und Linux
   komfortabel, direkt bedienbar und passend geprüft sind; normale Abläufe bleiben
   auch bei großen Beständen reaktionsfähig und vermeiden unnötige Ressourcenlast,
2. der native Metadatenvertrag einschließlich Hardlinks vollständig ist,
3. mehrere Operationen zuverlässig über eine persistente Queue laufen,
4. SFTP, FTP/FTPS, WebDAV und SMB die erwartbaren gewöhnlichen Dateiaktionen mit
   den tatsächlich verfügbaren Garantien anbieten; Sync, Abbruch und Recovery folgen
   jeweils passenden Verträgen, ohne lokale Atomarität für jedes Protokoll zu
   verlangen,
5. bidirektionaler Sync Konflikte und Löschungen anhand eines persistenten Baselines
   nachvollziehbar behandelt,
6. Builds, Installer, Upgrades und Plattformtests reproduzierbar sind und
7. keine Produktwarnung Fähigkeiten verspricht, die der gewählte Modus nicht erfüllt.

Das vollständige Providerziel ist wegen Remote-Sync- und Metadatengrenzen sowie
noch offener Komfortparität nicht erfüllt; diese
Roadmap erteilt deshalb keine 1.0-Freigabe.

Nicht Bestandteil von 1.0 sind Dateisystem-Snapshots, APFS-Klonerhalt, automatische
Übersetzung inkompatibler ACL-Modelle oder unbemerkte Konfliktauflösung.

## Aktueller Stand

Fertig sind das Dual-Pane-Grundgerüst, lokale Dateioperationen, Favoriten,
konfigurierbare Pane-Spalten, lokale und manuell gestartete Remote-Ordnergrößen sowie
ein Informationsdialog für Ein- und Mehrfachauswahl. Dry-Run und Test-Sandbox,
Quarantäne/Papierkorb, ContentHash-Planbindung sowie der native Transfer
von Owner, ACLs, xattrs, Ressourcen-Forks, Sonderbits, Sparse-Extents und Hardlinks.
Gewöhnlicher NativeBestEffort-Sync erhält Sparse-Bereiche lokaler Quellen nun auch
auf geeigneten SMB-Zielen. Der [begrenzte Tempvertrag](metadata-stage2-design.md#sparse-temps-auf-smb-zielen)
hält Probe, Objektabschluss und bekannte Teilwirkungen. Mit vorbereitetem
DACL-Zielkontext können auch die bereits unterstützten Windows-Datei-DACLs und
benannte Datenströme gemeinsam mit Sparse-Bereichen übertragen werden. Konkrete
abgeschlossene DACL-Zielabsagen lassen unabhängige Sparse-/ADS-Kopien mit sichtbarer
ACL-Auslassung zu; ein unvorbereiteter oder unklarer Kontext genügt dafür nicht.
Der Datei-Tempvertrag erhält außerdem konkrete EA-/Ausführungsverbote ohne
unnötige entsprechende Abschlussrechte. WRITE_DAC-Denys behalten den vor SET
gewährten Änderungszugriff und benötigen danach nur den Leseabschluss; dies gilt
auch für die getrennte bestehende Datei-DACL-Wirkung. Weitere Modelle und Servergranularitäten
bleiben offen.
`Portable`, `NativeBestEffort` und `NativeStrict` sind in der GUI wählbar. Native
Profile erzwingen `ContentHash`; Vorschau und Abschluss bilanzieren alle Klassen und
Best-Effort-Auslassungen pfadgenau. `NativeStrict` entsteht nur nach vollständigem
Scan ohne Auslassung. Normaler lokaler AddOnly-/Update-Sync verwendet eine
wirkungsbezogene Vorbereitung und Bestätigung; reine Additionen benötigen weder
Zufallsphrase noch Quarantäneprobe. Mirror behält seinen getrennten Realmodus mit
Mount-/Listing-Proofs, Restore-Probe, Platzbilanz, Wirkungsbestätigung und einem einmaligen
Zehn-Minuten-Permit. Details: [Lokaler Sync](local-sync.md).
Die Vollbackup-Warnung bleibt im zugehörigen Sync-Ablauf bestehen. Reguläre Dateien
lassen sich über
Tastatur, Doppelklick, Kontext- und Datei-Menü mit ihrer System-App öffnen; finale
Symlinks werden dabei nicht verfolgt. Der erste Meilenstein ist abgeschlossen: Der
macOS-Livetest hat das Öffnen mit der System-App, den Abbruch einer künstlichen
4-GB-Kopie ohne Ziel- oder Temp-Rest und den wiederherstellbaren Papierkorbtransfer
einer künstlichen Datei ausschließlich in der markierten Sandbox bestätigt.
Das Headless-CLI stellt Dry-Run, denselben lokalen Realmodus und die zielgebundene
Quarantäne-Verwaltung bereit. Vorbereitung, Wirkungsbestätigung und Ausführung bleiben
in einem Prozess; JSONL-Ausgabe und SIGINT-Abbruch sind automatisiert getestet.
Die lokale Operations-Queue persistiert Copy, Move und Sandbox-/Dry-Run-Sync atomar,
hält pro App-Datenordner eine exklusive Prozesssperre und recovered aktive Einträge als
`Interrupted`. Pause, Umordnung, Abbruch, Fehlerstrategie und laufbezogene Bestätigung
von Plan-/Konfliktdrift sind über den Queue-Zähler im Fensterkopf bedienbar.
SFTP bietet gepinnte Hostkeys, sitzungsgebundene Identität, Reconnect und native
Schlüsselbund-/SSH-Agent-Referenzen. Interaktives Auflisten ist sichtbar Best Effort.
Reguläre Einzeldateien lassen sich über eine eigene begrenzte Best-Effort-Leseklasse
öffnen und zusammen mit begrenzten Bäumen additiv lokal herunterladen.
Strikter Sync bleibt gesperrt; gewöhnliche Remote-Mutationen verwenden die
getrennte Best-Effort-Schreibklasse. [Vertrag und Prüfstand](sftp-writes.md).
Frühere Transferbelege unten sind
historisch.
[Der aktuelle Inhaltsvertrag](sftp-contents.md) erklärt die fehlende atomare Root-/
Elternbindung und die konkreten Grenzen.
FTP/FTPS ist ebenfalls angebunden: Klartext-FTP sowie FTPS mit öffentlicher
Zertifikatskette, Gültigkeits- und Hostnamenprüfung ohne dauerhaften Profilpin als
Standard. Gültige öffentliche Zertifikatserneuerungen blockieren neue Verbindungen
nicht. Die bisherigen öffentlichen, extern geprüften privaten oder bewusst
ungeprüften Pin-Modi bleiben optional erhalten. Dazu kommen `PROT P` im TLS-Modus,
striktes vollständiges MLSD, Reconnect, Profile/Remote-Panes, Realmodus und Queue. Die
Transportstufe ist sichtbar, aber kein pauschales Funktionsgate. FTP/FTPS→lokal
unterstützt alle drei Modi.
Lokal→FTPS wird nur nach erfolgreicher STOU-/Readback-/No-Replace-Probe als AddOnly
freigegeben; Update/Mirror bleiben ohne standardisierten Dauerhaftigkeits-, Quota- und
Remote-Quarantänevertrag fail-closed.
WebDAV ist ebenfalls integriert: kanonische HTTPS-Origin und Collection, Zertifikat-
und Kontoidentität, striktes vollständiges Depth-1-PROPFIND, starke ETags,
ContentHash für jede WebDAV-Quelle, Reconnect, Profile, Remote-Panes, Realmodus und
Queue. WebDAV→lokal unterstützt alle drei Modi. Strikter Ziel-Sync auf WebDAV bleibt seit AP13R ohne gemeinsame Root-/Elternbindung
passiv gesperrt. Gewöhnliche additive lokale Datei-/Baumuploads, mkdir und Rename
verwenden eigene Best-Effort-Verträge; [Grenze](webdav-mutation-boundary.md).
SMB ist als vierter Provider integriert: Server-GUID, Share, Konto, Root, SMB-3.1.1-
Dialekt, Signing und Verschlüsselung bilden die Identität. Vollständiges
`QUERY_DIRECTORY`-Paging mit File-IDs, Reconnect, Profile, Remote-Panes, Realmodus,
Queue und das lesende Anzeigen alter Quarantäneläufe sind umgesetzt. SMB→lokal
unterstützt alle drei Modi. Lokal→SMB unterstützt AddOnly nach Create-/FLUSH-/Rename-
Proben. Update/Mirror sowie Quarantäne-Restore/Purge bleiben ohne den
vollständigen identitätsgebundenen Restore-Proofvertrag gesperrt. Gewöhnliches
Ersetzen und Dauerlöschen verwenden getrennte sichtbare Best-Effort-Wege. Ein echter
verschlüsselter Samba-E2E verwendet ausschließlich einen kurzlebigen `/tmp`-Share.
Tabs je Pane, Sitzungswiederherstellung nach echtem macOS-Neustart, native
Finder→FTPS-Kopie und der private Papierkorb-v2-Schreibweg auf macOS/Linux sind
inzwischen ebenfalls belegt; Verträge und Grenzen stehen im [Index](INDEX.md).

## Historische Umsetzungsschritte

Die folgenden Stufenabsätze bewahren ihre damaligen Belege. Insbesondere frühere
SFTP-Transfer- und SMB-Quarantänefreigaben sind durch die aktuelle Matrix überholt.

Der bidirektionale Sicherheitskern wurde in Stufe 1 umgesetzt: vollständige
providerneutrale Snapshots binden Root-Identität, rohe Pfade, doppelt gelesene
BLAKE3-Inhalte, portable Rechte, mtime und Provider-Token. Eine atomar mit `0600` und
Prozesssperre persistierte Baseline sowie der reine Drei-Wege-Planer unterscheiden
einseitige Änderungen, gemeinsame Änderungen, Löschungen, Typkonflikte und
Tombstone-Wiederbelebungen. Es gibt in dieser Stufe bewusst noch keine Mutation.
Die anschließende Provider-Matrix ist ebenfalls umgesetzt: ein gemeinsamer, nicht
serialisierbarer Endpunkttyp ersetzt die acht internen Local↔Remote-Sonderwege und
erlaubt Provider↔Provider. Ziel-Capabilities sind sichtbar, permitgebunden und werden
nach Reconnect erneut geprüft. Der frühere WebDAV→WebDAV-Zielstream ist seit AP13R ohne gemeinsame
Root-/Elternbindung gesperrt; die providerweisen bidirektionalen Freigaben
folgen in den unten beschriebenen Stufen 6b bis 6h.
Die lokale Stufe 3 führt konfliktfreie Additionen und Ersetzungen nun über vier
geordnet vorbereitete, schreibgeschützte Projektionen aus: additiv links/rechts vor
Ersetzungen links/rechts. Die bestehende Einweg-Engine übernimmt Temp-Datei,
ContentHash, Quarantäne, Abbruch und Permit. Erst zwei stabile finale Vollscans mit
gleichen semantischen Bäumen erlauben den atomaren Baseline-Commit. Vorschau-,
Quell-, Ziel- und Baseline-Drift stoppen ohne Fortschreibung; Konflikte und jede
Löschwirkung blieben bis Stufe 4 gesperrt. Der erste Stufe-4-Schnitt ist nun fertig:
einseitige, baseline-belegte Löschungen werden nach vollständigem Mirror-Zweitscan
und den sichtbaren Mengenwarnungen in die jeweilige Quarantäne verschoben.
Erst der stabile Abschluss schreibt einen richtungsgebundenen Tombstone; beidseitiges
Fehlen benötigt keine Mutation. Reguläre Dateikonflikte besitzen nun explizite
KeepLeft-/KeepRight-/KeepBoth-/Skip-Entscheidungen. Der konfliktfreie Aliasname bindet
Seitenrolle, UTC-Zeit und Zufall an den Plan; beide AddOnly-Konfliktkopien laufen vor
der Originalersetzung. Skip lässt Nutzerdaten und den alten Baseline-Pfad unverändert,
während andere Pfade desselben Laufs fortgeschrieben werden. Typ-/Verzeichnisfälle,
Recovery-Journal und UI waren in diesem Zwischenschnitt noch offen und wurden in den
folgenden Stufen geschlossen.
Der erste Recovery-Schnitt persistiert vor der ersten Probe ein versions- und
digestgebundenes `0600`-Laufjournal. Es bindet Paar, Baseline, beide Snapshots,
Endpunktidentitäten und die geordnete Projektionsliste. `Prepared`, `Running` und
`Committed` werden jeweils vor beziehungsweise nach der Mutation atomar geschrieben;
ein offenes Journal blockiert neue Normalpläne. Ohne Teilcommit darf es nur nach dem
erneuten vollständigen Beweis beider ursprünglicher Snapshots verworfen werden.
Resume ist inzwischen ebenfalls umgesetzt: Journal v2 hält den vollständigen
geheimnisfreien Plan und beide Ausgangssnapshots. Ein frischer Doppel-Scan muss exakt
dem Zustand nach null bis einschließlich der als `Running` markierten Projektion
entsprechen. Dann werden nur offene Projektionen mit neuer Phrase und neuen Permits
vorbereitet; Mischzustände oder fremde Änderungen stoppen. Der rückwärts laufende,
quarantänisierende Rollback ist für reguläre Dateiwirkungen ebenfalls umgesetzt:
Additionen und neue Ersetzungsversionen werden in eine eigene manifestierte Quarantäne
verschoben; alte Ersetzungs- und Löschversionen kehren ausschließlich per No-Replace
aus der Vorwärtsquarantäne zurück. Ein persistenter Zweiphasenschritt recoverd auch
den Crash zwischen beiden Renames. Verzeichnis-Unterbäume waren in diesem
Zwischenschnitt noch offen; die folgenden Stufen schließen sie und sind auf macOS
und Linux grün.
Typgleiche Verzeichnisentscheidungen bilanzieren nun den ganzen Unterbaum und absorbieren
Kindaktionen. Bei `KeepBoth` bindet die Aliasliste jeden Quell-/Zielpfad und materialisiert
den vollständigen Verliererbaum auf beiden Seiten. Löschprojektionen werden absichtlich
erst nach allen Copy-/Update-Projektionen frisch vorbereitet, damit planmäßige frühere
Mutationen nicht als Drift erscheinen. Typwechsel und Unterbaum-Rollback waren hier
noch offen und werden in den folgenden Absätzen geschlossen.
Lokale Typentscheidungen materialisieren den vollständigen Gewinner-Unterbaum zuerst
unter einem gebundenen Stagingnamen, tauschen Datei und nichtleeres Verzeichnis dann
descriptor-relativ atomar aus und verschieben den Verlierer in die manifestierte
Quarantäne. „Beide behalten“ kopiert den Verliererbaum vorher auf beide Seiten. Ein
Crash direkt nach dem Kernel-Swap wird anhand des Journal-Zweiphasenschritts sicher
zurückgetauscht und anschließend mit frischem Permit fortgesetzt. Normale
Mehrpfad-/Verzeichnisprojektionen rollen inzwischen tiefenstabil zurück: additive und
gelöschte Unterbäume bleiben als ganze manifestierte Einheiten erhalten, Kindpfade
laufen vor Elternmetadaten und Journal v7 bindet den Einheitenindex. Zusätzlich prüft
jede erst später vorbereitete Projektion den exakten erwarteten Projektionspräfix,
statt fremde Drift in einen neuen Mirror-Plan zu übernehmen. Der spezielle
Typwechsel-Rollback ist ebenfalls geschlossen: Die Altversion kehrt per No-Replace
zuerst an den Stagingnamen zurück, ein Kernel-Swap stellt den Originaltyp atomar wieder
her und die frühere Stagingprojektion quarantänisiert danach die Neuversion. Crashs
nach beiden Phasen sind anhand der vollständigen Baumform fortsetzbar. Damit ist der
lokale Lauf-/Resume-/Rollback-Vertrag aus Stufe 5 vollständig. Journalversion 6 der
Operations-Queue persistiert nur Paar, Roots, Digests und Konfliktentscheidungen;
Phrase, Handles und Permit bleiben im RAM. Ein Laufabbruch blockiert die Queue bis
zum backend-seitig gebundenen Resume oder Rollback. Toolbar, Queue-Recovery und das
Headless-CLI sind angebunden und im Browser-Mock ohne Fehler geprüft. Offen bleibt
in diesem Stufenstand die providerweise Remote-Freigabe; die folgenden Stufen
beschreiben deren aktuellen Umfang. Der echte macOS-Sandbox-GUI-Abschluss ist nach
den Frontend-Umbauten am 2026-07-13 erneut bestanden.
Stufe 6a bindet inzwischen bei jedem bereits verbundenen Provider dauerhaft Konto,
öffentliche Serveridentität und virtuellen Root, ignoriert dagegen bewusst
Sitzungsgeneration und kurzlebige Handles. Daraus entstehen vollständige
providerneutrale Doppel-Snapshots und ein reiner Drei-Wege-Plan. Der sichtbare
Ausführungsblocker blieb für jede konkrete Zielrichtung bestehen, bis sie Quarantäne
und Rollback auch über Prozessabbruch hinweg bewies.
Die erste dieser Freigaben ist als Stufe 6b umgesetzt: Local↔SFTP darf additive Pfade
in beide Richtungen übertragen. SFTP-Ziele müssen dafür bereits den stärkeren
Quarantäne-/Restore-Vertrag erfüllen. Laufjournal, frischer Resume und rückwärts
quarantänisierender Rollback sind providerneutral wiederverwendet. Stufe 6c erlaubt
zusätzlich reguläre Dateiersetzungen in beide Richtungen: Ein Mirror-Plan ohne
Entfernung darf den bewiesenen Update-Vertrag nutzen, echte Mirror-Wirkungen bleiben
ohne starke Verzeichnis-Dauerhaftigkeit gesperrt. Stufe 6d erlaubt zusätzlich
Löschungen von SFTP in ein lokales Ziel. Queue und UI folgen in Stufe 6f,
Verzeichnis-/Typwechsel in ein starkes lokales Ziel in Stufe 6g. Remote-Zielwechsel
und Löschungen in Richtung SFTP bleiben wegen fehlendem portablen Verzeichnis-`fsync`
absichtlich fail-closed.
Stufe 6e bindet FTPS und WebDAV als reine Quellen eines lokalen Ziels ein. SMB folgt
pro Zielrichtung seiner dynamisch geprüften Update-/Mirror-Capability. Vorbereitung,
Resume und Rollback verwenden dafür dieselben providerneutralen APIs. Der erweiterte
echte SMB-Wiederholungslauf ist auf dem Linux-Testhost grün; FTPS-/WebDAV-E2E und synthetische
SMB-Matrix sind ebenfalls auf macOS und Linux grün.
Stufe 6f integriert dieselben Endpunkte in Direktlauf, Queue, Resume und Rollback.
Queue-Journal v8 persistiert ausschließlich öffentliche Providerdaten und Secret-
Referenzen. Die Mock-UI belegt lokale und SFTP-Paare ohne Browserfehler. Der echte
macOS-Sandbox-GUI-Abschluss belegt Pane-IPC, Dialog-Fokus, Vorschau, Sync-Fortschritt
und Zielaktualisierung unter der strikten CSP. Der Linux-App-/Paketbau ist mit
0.1.115 bestanden. Version 0.1.123 bestand am 2026-08-06 zusätzlich die interaktive
Fenster-, Dateioperations-, Mehrfenster- und Installationsabnahme; für den späteren
Releasekandidaten ist der Wiederholungslauf nach den seitherigen Core-, Provider-,
Queue- und Recoveryänderungen am 2026-10-19 ebenfalls bestanden. Paketbau,
Installation, echter Versionswechsel und Deinstallation sind technisch geprüft;
Darstellung, F2/F5/F6, zweites Fenster und korrigiertes Dockicon sind menschlich
abgenommen. [Kandidat und Grenzen](linux-kandidat-2026-10-19.md).
Stufe 6g erlaubt bestätigte Datei↔Verzeichnis-Wechsel aus SFTP-, FTPS- und WebDAV-
Quellen in ein lokales Ziel. Staging, atomarer Tausch, Quarantäne und Recovery bleiben
im gemeinsamen Kern; Remote-Ziele sind weiterhin gesperrt. Echte E2E aller drei
Provider belegen den Ablauf. Nichtportable Verzeichnisgrößen werden nicht semantisch
verglichen, der rekursive Unterbaum bleibt vollständig plan- und digestgebunden.
Stufe 6h schließt den Resume-/Rollback-Beleg unmittelbar nach diesem atomaren Commit
mit frisch verbundenen Endpunkten. SFTP-Verzeichnismetadaten sind zusätzlich in die
starke lokale Zielrichtung freigegeben und im echten SFTP-E2E belegt; Remote-
Zielrichtungen bleiben ohne gleichwertigen Namespacevertrag gesperrt.
Der echte opt-in SFTP-E2E und die vollständigen Kern-Gegenläufe auf macOS und Linux
sind am 2026-07-13 bestanden.

Der Qualitäts-Schnitt „Property-Tests“ ist ebenfalls abgeschlossen: 64 generierte
Konvergenz-/Idempotenzfälle und 64 Mirror-Roundtrips laufen auf macOS und Linux nur in
temporären Bäumen.

Auch die Parser-Fuzz-Targets sind abgeschlossen: Baseline, Laufjournal und
Metadatenmanifest besitzen getrennte, größenbegrenzte LibFuzzer-Einstiege mit gültigen
versionierten Seeds. Nightly-Sanitizer-/Coverage-Läufe auf macOS sowie Build und
Seedvalidierung auf Linux sind grün.

Die Tauri-Zustandsmutexes sind ebenfalls konsolidiert und poison-tolerant. Nach einem
Worker-Panic gewinnt der zentrale Helfer den vorhandenen Zustand ohne Folge-Panic
zurück; App-Tests/Clippy auf macOS und der isolierte Helfertest auf Linux sind grün.

Die kleine App-Konsistenzrunde ist ebenfalls abgeschlossen: Vorschau und Einreihung
verwenden dieselbe Sandbox-Kindprüfung und denselben descriptor-gebundenen lokalen VFS.
Die Pane-Zeilenhöhe besitzt mit `--row-height` nur noch eine Layoutquelle; PageUp liest
den berechneten CSS-Wert. App-Gegentests, Clippy und Mock-Fenster sind auf macOS grün,
der damals paketblockierte Linux-App-Gegenlauf ist mit 0.1.115 bestanden.

Der erste CLI↔Tauri-Dedup-Schnitt ist abgeschlossen: `core::expand_home_path` ersetzt
beide lokalen Varianten, expandiert nur `~`/`~/…`, lässt `~konto` unverändert und
bewahrt nicht als Unicode darstellbare Pfade. Vollständige Kern-, CLI- und App-Tests
sowie Clippy sind auf macOS grün; Kern, CLI, Provider-E2E und Clippy auch auf Linux.

Auch der Quarantäne-Scope ist jetzt gemeinsam: Der Kern kapselt descriptor-gehaltenen
VFS, Basis, validierte Sandbox und exakte Manifest-Zielbindung hinter einer kleinen
API für Liste, Restore und Leeren. Zwei neue Gegentests belegen Markerpflicht und
Zielfilterung; vollständige Kern-/CLI-/App-Läufe sind auf macOS und Linux grün.

Der CLI↔Tauri-Schnitt ist damit abgeschlossen. Die konsumierende Kernmethode
`confirm_and_execute_with_audit` ordnet Bestätigung, finalen Zweitscan, Einmal-Permit,
Start-Audit und Mutation für beide Frontends identisch. Nur Interaktion, App-Zustand,
Abbruchquelle und Ausgabedrosselung bleiben absichtlich oberflächenspezifisch.

Die erste Kern-Duplikatkonsolidierung ist abgeschlossen: Sync und Quarantäne verwenden
denselben rekursiven Unterbaumbilanzierer. Er zählt Symlinks ohne sie zu betreten und
bricht bei einem nach dem Listing verschwundenen Ordner fail-closed ab. Zwei neue
Gegentests und die vollständigen Plattformläufe sind grün.

Auch relative interne Pfade besitzen jetzt einen gemeinsamen Vertrag. Sync-Pläne,
Quarantänemanifeste und bidirektionale Projektionen akzeptieren nur normale relative
Komponenten; Manifeststrings werden nie verlustbehaftet erzeugt. Vier Pfad-Gegentests
und sämtliche Plattformläufe sind grün.

Die kleinen persistenten Zustandsdateien von Papierkorb, Quarantäne und Queue verwenden
nun denselben atomaren Write/Flush/Sync/Rename/Eltern-Sync-Helfer. Create und Replace
bleiben getrennte Modi, Fehler bereinigen nur den eigenen Zufallstempnamen, lokale
Journale enden stets mit Modus `0600`. Vollständige Plattformläufe sind grün.

Auch reguläre Dateiübertragungen teilen jetzt einen GUI-freien atomaren Kernpfad:
Sync und interaktive Dateioperationen verwenden dieselbe exklusive Nachbartemp-Anlage,
abbruchfähige Stromkopie, BLAKE3-Bindung, Ziel-Readback und dauerhaft synchronisierte
Rename-Primitive. Sparse-Dateien und Hardlinks bleiben spezialisierte VFS-Pfade, nutzen
aber denselben Tempnamens- und Commit-Vertrag. Die vollständigen macOS-/Linux-Läufe
sind grün.

Die internen Sync-Hotspots sind außerdem gegen Parametervertauschungen gehärtet:
Sieben bisherige Argumentlisten für Planung, Vergleich, Datei-/Metadatenstrom,
Ordner-Metadaten und Hardlinks sind benannte Request-Typen. Die öffentliche
`execute`-Grenze bleibt bis zur geplanten Modulaufteilung kompatibel. Vollständige
macOS-/Linux-Gegenläufe und Clippy sind grün.

Im Dateioperationskern verwenden auch die rekursive Baumkopie und der atomare
Einzeldateitransfer benannte Requests. Sandbox, Endpunkte, Ersetzungsentscheidung und
portable Metadaten sind dadurch nicht mehr positionsabhängig verdrahtet; die stabilen
öffentlichen Copy-/Move-Einstiege bleiben vorerst kompatibel. Alle Plattformläufe sind grün.

Der lokale und der Remote-Realmodus teilen nun außerdem benannte Auditlauf- und
Permit-Requests. Identitäten, Listing-Proofs, Capability-Digest, drei Wurzeln und beide
Zeitgrenzen werden ausschließlich per Feldname gebunden; auch lokal existiert nur noch
der gemeinsame zufallsbasierte Permit-Konstruktor. Die Realmodus-Gegentests und die
vollständigen Plattformläufe sind grün.

Auch die bidirektionale Recovery verwendet für Typwechsel, Resume und Rollback ein
gemeinsam benanntes Paar aus linker/rechter VFS-Wurzel. Fünf pauschale Clippy-Ausnahmen
und ein wirkungsloser Parameter sind entfallen. 70 gezielte Crash-/Recovery-Fälle sowie
die vollständigen Plattformläufe sind grün.

Lokale Datei- und Sync-Queue-Einträge bündeln ihre persistierte Wirkung, Operation-ID,
Konflikt-/Planbindung und Queue-Zugriff nun ebenfalls in benannten Requests. Zusammen
mit einer überflüssigen Metadaten-Ausnahme bleiben nur die drei stabilen öffentlichen
Sync-/Copy-/Move-Signaturen bis zu ihrer Modulaufteilung ausgenommen. Queue-Gegentests,
vollständige Plattformläufe und Clippy sind grün.

Die Tauri-App wird schrittweise in Command-Module zerlegt. Favoriten und lokale
Quarantäne sind als erster Schnitt aus `main.rs` herausgelöst; die Quarantäne ruft die
zentrale Single-Flight- und Queue-Prozessschranke weiterhin zwingend auf. App-Tests und
Clippy auf macOS sowie die vollständigen Kern-Gegenläufe auf macOS/Linux sind grün.

Als zweiter Tauri-Schnitt liegen lokale Konfliktprüfung, Copy/Move-Enqueue,
Papierkorb, Ordneranlage und Umbenennen in `file_commands.rs`. Die Sicherheitsreihenfolge
aus Recovery, Sandbox, Konfliktbindung und zentraler Mutationsschranke bleibt erhalten;
App- und Kern-Gegenläufe sind grün.

Der dritte Tauri-Schnitt verschiebt Abbruch sowie Pause, Reihenfolge, Entfernen,
Wiederholen und Freigabe der Queue in `queue_commands.rs`. Worker, Prozesssperre und
Eventemission bleiben als gemeinsame Laufzeitgrenze zentral. App- und Kern-Gegenläufe
sind grün.

Pane-Start, lokales Listing und das shell-freie Öffnen mit der System-App bilden den
vierten Tauri-Schnitt in `browser_commands.rs`. Finaler Symlink und Nichtdateien bleiben
gesperrt und direkt getestet; App- und Kern-Gegenläufe sind grün.

Der fünfte Tauri-Schnitt bündelt lokale Vorschau, plan-gebundenes Queue-Enqueue,
Safety-Info und den backend-gehaltenen Realmodus-Assistenten in
`local_sync_commands.rs`. Der Startcommand akzeptiert weiterhin keine Pfade, Optionen
oder Permitdaten aus dem Frontend; App- und Kern-Gegenläufe sind grün.

Der sechste Tauri-Schnitt kapselt App-Zustand, Single-Flight-Schranke, persistenten
Queue-Worker, vorgeschaltete Papierkorb-Recovery und Fortschrittsereignisse gemeinsam
in `queue_runtime.rs`. Alle lokalen und Remote-Commands teilen weiterhin genau diese
Prozess- und Mutationsgrenze; App- und Kern-Gegenläufe sind grün.

Der siebente Tauri-Schnitt schließt die Produktionsaufteilung von `main.rs` ab:
strukturierte Frontendfehler und die fail-closed Sandbox-Auflösung liegen in
`app_support.rs`, die native Menüleiste in `menu.rs`. Im Einstieg verbleiben nur
Modulverdrahtung, der Tauri-Handler und lokale Gegentests; App-Tests und Clippy sind
grün.

Der erste Core-Schnitt aus dem Sync-Monolithen bündelt geschlossene
Ausführungsfreigaben, Sandbox-Marker, Root-Identitätsbindung, Mirror-Gates und die vor
jeder Mutation geprüfte Pfadschranke in `sync/safety.rs`. Öffentliche API, Plan-Digests
und Fehlercodes bleiben unverändert; die vollständigen Plattformläufe sind grün.

Der zweite Core-Schnitt verschiebt den rein lesenden Planner nach `sync/plan.rs`:
vollständige Listings, Diff, Inhalts-/Baumdigests, Hardlink-Gruppen,
Metadatenmanifeste und sichtbare Planbilanz bleiben dort gemeinsam. Öffentliche API
und serialisierte Aktionen bleiben unverändert; die vollständigen Plattformläufe sind
grün.

Der dritte Core-Schnitt verschiebt den Aktionsorchestrator nach `sync/execute.rs`.
Safety-Validierung, Directory-/Hardlink-Preflight, Fortschritt, Abbruch,
Quarantäne-Abschluss und post-order-Verzeichnismetadaten bleiben in einer festen
Reihenfolge; atomare Datei- und Metadatenhelfer bleiben zunächst gemeinsam. Die
vollständigen Plattformläufe sind grün.

Der vierte Core-Schnitt bündelt Manifest-Planbindung, Ressourcen-Fork-/Owner-/ACL-/
xattr-Transfer und den selektiven Abschluss-Readback in `sync/metadata.rs`. Der
atomare Copy-Pfad ruft alle Phasen weiterhin vor dem Rename-Commit auf; die
vollständigen Plattformläufe sind grün.

Der fünfte Core-Schnitt schließt die Aufteilung ab: Temp-Anlage,
Inhalts-/Sparse-Transfer, Quell-/Zieldrift, Hardlink-Erzeugung, fsync, Quarantäne und
Rename-Commit liegen gemeinsam in `sync/atomic.rs`. `sync.rs` enthält nur noch die
öffentlichen DTOs/API und kleine Berichtstests; die vollständigen Plattformläufe sind
grün.

Der erste Frontend-Modulschnitt extrahiert die lazy Tauri-Brücke sowie zustandsfreie
DOM-, Format- und Fehlerhelfer. `main.js` startet unter der strikten CSP als natives
ES-Modul; der Mock-Server injiziert seine Bridge davor. Pane-Start und Vorschau laufen
im echten Browser ohne Mock- oder Konsolenfehler.

Die Pane-Listen besitzen jetzt echte Multi-Select-`listbox`-/`option`-Semantik samt
stabiler aktiver ID. Ab 2001 Einträgen rendert jede Pane nur Viewport plus Overscan;
CSP-konforme SVG-Spacer erhalten den vollständigen Scrollraum. Ein parametrierter
5000-Einträge-Mock belegt 39 DOM-Zeilen und korrekte Tastatur-Endnavigation ohne
Browserfehler.

Der universelle Dialog ist als modale, beschriftete Dialogrolle ausgezeichnet. Eine
gemeinsame Fokusfalle hält Tab und Shift-Tab im Dialog, Escape löst stets dessen
fachliche Abbruchaktion aus, und nach dem Schließen kehrt der Fokus zum Auslöser
zurück. Dateikontextmenüs lassen sich mit Shift-F10 beziehungsweise der
Kontextmenü-Taste öffnen und über Pfeile, Home, End und Escape vollständig bedienen.
Der große Browser-Mock belegt beide Fokuspfade ohne Konsolenfehler.

Eine eigenständige Playwright-Suite startet den Mock-Server auf einem freien lokalen
Port und prüft sechs Browserabläufe: Tastatur/Type-ahead und Kontextmenü,
Virtualisierung mit 5000 Einträgen, persistente Darstellung, Realmodus-Phrasengate,
Queue-Driftbestätigung sowie die Sequenzsperre bei verspäteten Scans einschließlich
Fehler- und Konfliktpfad. Der Mock begrenzt alle injizierbaren Größen und Latenzen;
die Suite schlägt bei Mock-, Seiten- oder Konsolenfehlern fehl und mutiert keine Daten.

Der zweite Frontend-Modulschnitt trennt die fachunabhängige Dialogsteuerung und die
Darstellungswahl aus `main.js`: `modal.js` besitzt die einzige Fokusfalle und
Fokus-Rückgabe sowie Choice-/Confirm-/Prompt-Dialoge; `theme.js` besitzt Persistenz,
Systemdarstellungswechsel und den WebKitGTK-Fallback. Die öffentliche Bedienung bleibt
unverändert und alle sechs Playwright-Abläufe sind nach dem Schnitt grün.

Der dritte Schnitt isoliert das semantische Maus-/Tastatur-Kontextmenü hinter
injizierten Fachaktionen und verschiebt die reine Vorschau-/Aktionsdarstellung nach
`plan-view.js`. Der Renderer erhält nur bereits gebundene Plan-DTOs und kann weder
planen noch mutieren. `main.js` liegt damit unter 3000 Zeilen; die vollständige
Playwright-Suite bleibt grün.

Die letzten öffentlichen Kernfunktionen mit langen Positionsargumentlisten sind auf
explizite Kontextstrukturen umgestellt. `FileOperationRequest` hält die vollständige
Dateioperationsbindung, `ExecuteRequest` die vollständige Sync-Ausführungsbindung;
Abbruch und Fortschrittscallback bleiben getrennte kurzlebige Laufzeitwerte. Dadurch
können Roots, VFS-Instanzen, Plan und Safety nicht mehr unbemerkt vertauscht werden.
Die vollständigen Kern-, Dateioperations-, Sync-, Property- und Provider-Gegenläufe
sowie Clippy sind auf macOS und Linux grün; die App-Gegenläufe auf macOS ebenfalls.

Der vierte Frontend-Schnitt verschiebt Konfliktprüfung, Copy/Move-Einreihung,
Papierkorb, Umbenennen und Ordneranlage nach `file-actions.js`. Der Controller besitzt
keinen zweiten Pane-Zustand, sondern erhält aktuelle Panes, Ladefunktion und
Statuszugriffe injiziert. Konfliktdialog und Queue-Einreihung bleiben durch die
Playwright-Suite belegt; `main.js` sank auf 2854 Zeilen.

Der fünfte Frontend-Schnitt kapselt Pane-Zustand, Listing-Race-Schutz, Auswahl,
Navigation, Sortierung, Type-ahead, Virtualisierung und die zugehörige DOM-Verdrahtung
in `pane-controller.js`. `main.js` sank auf 2364 Zeilen. Die sechs Playwright-Fälle
sowie ein echter CSP-WebView-Lauf mit Navigation und byteidentischem Sandbox-Sync sind
grün.

Der sechste Frontend-Schnitt bündelt globale Tastaturkürzel und native Menüereignisse
in `input-controller.js`. Das Modul kennt keine eigene Fachlogik, sondern ruft nur die
injizierten Pane-, Datei-, Theme- und Quarantäneaktionen auf; `main.js` sank auf 2230
Zeilen. Alle sechs Playwright-Fälle sowie ein echtes natives Menüereignis unter der
strikten WebView-CSP sind grün.

Der siebente Frontend-Schnitt kapselt Queue-Snapshot, Bedienaktionen, Driftphrase und
Freigabeauswahl in `queue-ui.js`; Ausführung und Journal bleiben im Backend. `main.js`
sank auf 2037 Zeilen. Ein siebenter Playwright-Fall belegt zusätzlich, dass ein
laufender SMB-Sync die Sync- statt der früher irreführenden Move-Fortschrittsanzeige
verwendet. Die Queue-Ansicht öffnet außerdem im echten Tauri-Fenster unter der
strikten CSP ohne Fehler.

Der achte Frontend-Schnitt verschiebt Quarantäne-Scope, Lauf-/Eintragsansicht und die
Bedienung der unveränderten Backend-Commands nach `quarantine-ui.js`; der gemeinsame
fokussichere Spezialdialog bleibt zugleich für die Queue verfügbar. `main.js` sank auf
1872 Zeilen. Ein achter Playwright-Fall belegt die sichtbare Keine-Auto-Löschung-
Grenze und die leere Laufansicht; dieselbe Ansicht öffnet im echten Tauri-Fenster
unter der strikten CSP fehlerfrei.

Der neunte Frontend-Schnitt kapselt lokale Schnellzugriff-Favoriten samt Laden,
Dedup-Anzeige, Navigation und Entfernen in `favorites-ui.js`; Remote-Profile bleiben
bewusst getrennt. `main.js` sank auf 1801 Zeilen. Ein neunter Playwright-Fall prüft
Hinzufügen und Entfernen gegen die zustandsbehaftete Mockbrücke; die vorhandene Liste
lädt außerdem im echten Tauri-Fenster unter der strikten CSP.

Der zehnte Frontend-Schnitt kapselt die ausschließlich geheimnisfreien SFTP-, FTPS-,
WebDAV- und SMB-Profil-DTOs, aktive Markierung, Pane-Navigation und Entfernung in
`connections-ui.js`; Passwortfelder und Schlüsselbund-Commands bleiben in getrennten
nativen Editoren. `main.js` sank auf 1597 Zeilen. Ein zehnter Playwright-Fall prüft
Remote-Navigation, aktive Markierung, sichtbaren Schlüsselbundhinweis und Entfernung;
alle vier Profilbereiche laden zusätzlich im echten Tauri-Fenster unter der strikten
CSP.

Der elfte Frontend-Schnitt verschiebt die nativen SFTP-, FTPS-, WebDAV- und
SMB-Editoren nach `connection-editors.js`. Öffentlicher Hostkey, Zertifikat oder
Server-GUID werden weiterhin vor jeder Anmeldung ermittelt; erst der zweite Schritt
schreibt das Secret direkt in den Schlüsselbund und persistiert nur dessen Referenz.
`main.js` sank auf 1253 Zeilen. Ein elfter Playwright-Fall öffnet alle vier Editoren
ohne Netzwerkzugriff und prüft Passwortfelder, Schlüsselbundhinweis und reine
öffentliche Erstprüfung. Der SFTP-Editor öffnet und schließt außerdem im echten
Tauri-Fenster unter der strikten CSP, ohne die Hostkey-Ermittlung auszulösen.

Der zwölfte Frontend-Schnitt bündelt Dock, Bedienelementsperre sowie gemeinsamen
Datei-, Sync- und Queue-Fortschritt in `operation-ui.js`; der fachliche Laufzustand
bleibt über einen expliziten Setter im Orchestrator. `main.js` sank auf 1205 Zeilen.
Alle elf Playwright-Fälle bleiben grün; eine echte leere Vorschau rendert das Dock
unter der strikten Tauri-CSP fehlerfrei.

Der dreizehnte Frontend-Schnitt bündelt Tauri-Queue-, Sync-, Dateioperations- und
Menüevents in `event-controller.js`. Das Modul plant und bestätigt nichts, sondern
bildet Payloads ausschließlich auf injizierte Fortschritts-, Dock- und Refreshaktionen
ab. `main.js` sank auf 1139 Zeilen; alle elf Playwright-Fälle bleiben grün. Ein echter
Sandbox-Lauf führte Queue-/Fortschritts-/Abschlussereignisse für eine synthetische
1-Byte-Datei bis zum byteidentischen Ziel unter der strikten CSP durch.

Der vierzehnte Frontend-Schnitt kapselt Paar-ID, Konfliktentscheidungen,
Laufbestätigung und Recovery-Bedienung in `bidirectional-ui.js`. Snapshot-, Digest-,
Capability-, Journal- und Mutationslogik bleibt ausschließlich im GUI-freien Kern.
Dabei wurde ein ungebundener `baseName`-Aufruf der Paarbenennung behoben; ein neuer
Browser-Gegenfall klickt den vollständigen Vorbereitungspfad, prüft die dauerhafte
128-Bit-Paar-ID und bricht ohne Mutation ab. Ein weiterer Gegenfall stellt sicher,
dass ein laufender bidirektionaler Queue-Eintrag nicht als Move erscheint. `main.js`
sank auf 922 Zeilen; alle dreizehn Playwright-Fälle sind grün. Derselbe
Vorbereitungsklick zeigte im echten
Sandbox-Tauri-Fenster unter der strikten CSP den korrekt benannten Paardialog und
kehrte nach Abbruch ohne Mutation zur Dateiansicht zurück. Elf App-Tests und
App-Clippy sind auf macOS, 188 Kern-Modultests und Kern-Clippy auf dem Linux-Testhost grün.

Die anschließende Mirror-Gegenprüfung behob denselben ungebundenen Basename-Helfer
auch in der lokalen Zielnamenbestätigung. `ui-utils.js` stellt ihn jetzt beiden
Aufrufern bereit. Ein vierzehnter Browserfall bindet einen künstlichen Löschplan,
bestätigt dessen Wirkung und den sichtbaren Zielnamen `ziel` und prüft die sichere
Queue-Einreihung; Nutzerdaten werden dabei nicht berührt. Elf App-Tests und App-Clippy
sind auf macOS, 188 Kern-Modultests und Kern-Clippy auf dem Linux-Testhost grün.

Der fünfzehnte Frontend-Schnitt kapselt den lokalen und providerbezogenen
Realmodus-Assistenten in `real-mode-ui.js`. Das Modul hält nur den kurzlebigen
Darstellungszustand; Permit, Root-/Serveridentitäten, vollständige Listings,
Capability-Digest, Quarantäneprobe und Einmalverbrauch bleiben im Backend. Ein neuer
Browser-Gegenfall führt einen SFTP-Direktlauf durch Hostkey-/Sitzungsanzeige,
Einmalphrase und Eventabschluss aus, ohne einen Secretwert zu lesen. `main.js` sank
auf 655 Zeilen; alle fünfzehn Playwright-Fälle sind grün. Das neue Modul lädt ebenso
im echten Sandbox-Tauri-Fenster unter strikter CSP; die leere lokale Vorschau endete
sichtbar als „Bereits synchron“, während der getrennte Realmodus deaktiviert blieb.
Elf App-Tests und App-Clippy sind auf macOS, 188 Kern-Modultests und Kern-Clippy auf
dem Linux-Testhost grün.

Der sechzehnte Frontend-Schnitt kapselt Einweg-Vorschau, lokale
Dry-Run-/Sandbox-Planprüfung, sichtbare Mirror-Gates und Queue-Einreihung in
`one-way-sync-ui.js`. Die UI erteilt dadurch weiterhin kein Permit: Der Kern plant
frisch, bindet den erwarteten Digest und prüft dieselben Lösch-, Sandbox- und
Quarantäneschranken unmittelbar vor Mutation. `main.js` sank auf 541 Zeilen; alle
fünfzehn Playwright-Fälle bleiben grün. Ein echter CSP-Sandbox-Lauf übertrug eine
synthetische 1-Byte-Datei durch Planung, Queue und Abschlussereignis bis zum
byteidentischen Ziel. Elf App-Tests und App-Clippy sind auf macOS, 188
Kern-Modultests und Kern-Clippy auf dem Linux-Testhost grün.

Die zuvor nur im Kern freigegebene Provider→Provider-Matrix ist jetzt auch über zwei
Remote-Panes erreichbar. `prepare_provider_sync` hält beide verbundenen Endpunkte und
das Einmal-Permit im Backend-RAM; `ProviderSync` in Queue-Journal v8 persistiert nur
öffentliche Pins, Kontoreferenzen, Roots, Optionen und den erwarteten Plan. Jeder
Queue-Claim verbindet beide Seiten frisch und wiederholt Capability-/Identitäts- und
Planprüfung. Ein Browser-Gegenfall reiht SFTP→WebDAV ohne Secretzugriff ein.
Ein zweiter Gegenfall verbraucht die Provider-Einmalphrase direkt. Abschluss: macOS
188 Modul-/24 Dateiops-/57 Sync-Tests, FTPS-/WebDAV-E2E, 11 App-Tests und
Workspace-Clippy; Linux-Testhost 190/24/60, FTPS-/WebDAV-E2E und Kern-Clippy.
Der echte Linux-Testhost-SMB-E2E ist seit 2026-07-13 grün. Er belegt SMB 3.1.1 mit aktiver
Signatur und Verschlüsselung, 1.500 gepagte Einträge, Reconnect, Upload/Download,
Quarantäne, capability-gesteuerte bidirektionale Richtungen und persistente Queue.
Dabei wurde die fehlende paarweite POSIX-Modus-Normalisierung für SMB gefunden und
mit einem eigenen Regressionstest behoben.

Der siebzehnte Frontend-Schnitt kapselt Richtungs-, Modus-, Vergleichs- und
Metadatenzustand samt Toolbar-Ereignissen in `sync-toolbar.js`. Native Profile stellen
weiterhin sichtbar auf ContentHash um und BestEffort+Mirror auf Update; der Kern
erzwingt beide Invarianten unabhängig. `main.js` enthält mit 488 Zeilen nur noch
Modulkomposition, gemeinsame DTO-Helfer und App-Start. Alle 17 Playwright-Fälle
bleiben grün. Im echten markierten Sandbox-Tauri-Fenster stellte die native
Metadatenwahl unter strikter CSP die sichtbare Backup-Grenze korrekt um; der Test
berührte keine Nutzerdaten. Abschluss: macOS 17 Browser- und 11 App-Tests sowie
App-Clippy; Linux-Testhost 190 Kern-Modultests und Kern-Clippy.

Die Release-Artefaktprüfung fand in 0.1.107 trotz `strip = true` absolute lokale
Cargo-Quellpfade im optimierten Rust-Binary. Der gemeinsame Build-Wrapper für macOS
und Linux neutralisiert Workspace-, Cargo-, Rustup- und Benutzerpräfixe nun über
kodierte Rust-Flags; eine unabhängige Nachprüfung stoppt bei jedem verbleibenden
lokalen Build-Pfad. Der Linux-Paketpfad schreibt zusätzlich dieselben SHA-256-
Prüfsummen wie macOS. Das neu gebaute arm64-App-/DMG-Paar 0.1.108 bestand Binärscan,
Signatur-, DMG- und Prüfsummenprüfung. Die gebündelte App übertrug im markierten
Release-Sandbox-Smoke-Test eine synthetische 60-B-Datei byteidentisch. Abschluss:
macOS 188 Modul-/24 Dateiops-/57 Sync-/7 CLI-/11 App-Tests, FTPS-/WebDAV-E2E,
2 Property- und 17 Browserfälle sowie Workspace-Clippy; Linux-Testhost 190/24/60/7,
FTPS-/WebDAV-E2E, 2 Property-Fälle und Kern-/CLI-Clippy.

Der GUI-Testvertrag isoliert nun nicht nur Dateioperationen, sondern auch sämtliche
App-Persistenz. Bei validierter Sandbox gehen Queue/Lock, Favoriten, Providerprofile,
Baselines, Audit und Papierkorb-Recovery zwingend nach `app-config` unter deren Root;
außerhalb bleibt der native Plattformpfad aktiv. Eine statische Release-Invariante
verbietet direkte Umgehungen. Im echten Sandbox-Fenster lagen Queue v8, Lock und
Favoriten ausschließlich dort; ein synthetischer 9-B-Queue-Sync endete byteidentisch.
Der anschließend neu gebaute pfadneutrale arm64-Kandidat 0.1.109 bestand Signatur,
DMG, Binärscan und Prüfsumme sowie denselben Isolationsvertrag mit einem
byteidentischen 17-B-Release-Sync. Abschluss: macOS 12 App- und 17 Browserfälle sowie
App-Clippy; Linux-Testhost 190 Kern- und 7 CLI-Fälle sowie Kern-/CLI-Clippy.

Der Sandbox-Launcher kann einen vorhandenen markierten Testbereich nun explizit über
`--reuse` für Recovery- und Upgrade-Gegenläufe erneut öffnen. Er verlangt Marker und
getrennte Panes; die vollständige Kernvalidierung bleibt zusätzlich aktiv. Im echten
Release-Wiederanlauf führte ein absichtlich beschädigtes Queue-v8-Journal sichtbar zu
`Queue !` und Startfehler, blieb unverändert und berührte die byteidentischen
synthetischen Quell-/Zieldaten nicht. Der anschließend gebaute pfadneutrale arm64-
Kandidat 0.1.110 bestand Signatur, DMG, Binärscan und Prüfsumme sowie einen isolierten
byteidentischen 5-B-Release-Sync. Abschluss: macOS 12 App-Tests und App-Clippy;
Linux-Testhost 190 Kernfälle und Kern-Clippy.

Die macOS-Artefaktverifikation bindet den Funktionsbeleg nun zusätzlich an das
ausgelieferte Paket: Der gesamte separat erzeugte, im Sandbox-Smoke-Test ausgeführte
App-Baum muss bytegleich mit dem App-Baum im read-only gemounteten DMG sein. Jede
Abweichung in Binary, Ressourcen, Plist oder Signatur stoppt fail-closed. Der
0.1.112-Kandidat bestand diese Prüfung, Binärscan, Signatur, DMG und Prüfsumme sowie
einen isolierten byteidentischen 5-B-Queue-Sync im echten Release-Fenster.

Der öffentliche macOS-Buildpfad 0.1.113 wählt nun fail-closed eine eindeutige
Developer-ID-Application-Identität oder `APPLE_SIGNING_IDENTITY`, akzeptiert nur
vollständige offizielle Tauri-Zugangssätze zur Notarisierung und führt anschließend
die öffentliche Signatur-/Staple-/DMG-/Pfad-/Prüfsummenprüfung aus. Secrets werden
weder als Argumente noch in Projektdateien geschrieben.

Der erste echte Lauf mit gültigen Apple-Zugangsdaten ist am 2026-07-16 bestanden und
hat zugleich eine Lücke geschlossen: Tauri notarisiert und stapelt nur die `.app` und
signiert das DMG danach lediglich, weshalb die eigene Staple-Prüfung des Verifikators
fehlschlug. `build-release.py` reicht das ausgelieferte DMG seit 0.1.117 selbst über
ein notarytool-Schlüsselbundprofil ein und heftet das Ticket an; ein Preflight belegt
das Profil vor dem Build. App und DMG sind von Apple mit Status `Accepted` notariell
bestätigt, Gatekeeper urteilt `Notarized Developer ID`. Für einen öffentlichen
Abschluss fehlt damit nur noch die Verteilentscheidung, keine Zugangsvoraussetzung.

Der Linux-Paketpfad ist auf dem Linux-Testhost gebaut und paketformatgenau geprüft, zuletzt mit
0.1.117. DEB und AppImage werden getrennt extrahiert; Version, Architektur,
Abhängigkeiten, Desktop-Integration, Binary, Bibliotheksauflösung, lokale Build-Pfade
und SHA-256-Prüfsummen sind gebunden. Der Sandbox-Launcher startet exakt das gebaute
AppImage oder ein installiertes DEB-Programm. Interaktive Fensterbedienung sowie
Installations-, Upgrade- und Deinstallationsbeleg bleiben vor der Freigabe offen.

## Historische Meilensteine und Aufwand

Die folgende Tabelle dokumentiert frühere Umsetzungsschritte. „Abgeschlossen“
bezieht sich auf deren damaligen Umfang, nicht auf vollständige Alltagstauglichkeit
oder eine heutige Freigabe aller Provideraktionen. Die Prioritäten und Fertigkriterien
oben sind maßgeblich.

Die Schätzungen sind Netto-Entwicklertage einschließlich automatisierter Tests, aber
ohne Wartezeit auf Zertifikate, fremde Server oder manuelle Freigaben.

| Reihenfolge | Meilenstein | Ergebnis | Aufwand |
|---:|---|---|---:|
| 1 | Lokalen Dateimanager abschließen ✓ | System-App-Öffnen und letzte GUI-Checks bestanden | abgeschlossen |
| 2 | Lokaler Realmodus ✓ | AddOnly, Update und Mirror mit Proofs, Restore-Probe, Audit und Einmal-Permit | abgeschlossen |
| 3 | Headless CLI ✓ | Plan, bestätigte Ausführung, JSONL, SIGINT und Quarantäne ohne Sicherheitsabkürzung | abgeschlossen |
| 4 | Operations-Queue ✓ | Persistente lokale Queue, Abbruch, Recovery, Driftbestätigung und UI | abgeschlossen |
| 5 | SFTP | Identität, Reconnect, Secret-Referenzen, interaktive Ordnerlisten, begrenzte Snapshots/additive Downloads und eigene Best-Effort-Schreibaktionen; strikter Sync/Quarantäne gesperrt | gewöhnliche Dateiaktionen implementiert; Prüfstand im Schreibvertrag |
| 6 | FTP/FTPS ✓ | Sichtbare Transportstufen, optionaler Zertifikatsvertrag, MLSD, Realmodus, Queue und GUI | abgeschlossen |
| 7 | WebDAV ✓ | Vollständiges Listing, ETag/Identität, Temp-/MOVE-Vertrag, Queue und Tests | abgeschlossen |
| 8 | SMB ✓ | Server-/Share-Identität, Paging, Rename/Quarantäne und Plattformintegration | abgeschlossen |
| 9 | Provider-Matrix ✓ | Generische Endpunkte, Remote↔Remote, Capability-/Reconnect-Bindung | abgeschlossen |
| 10 | Bidirektionaler Sync ✓ | Sichere Provider-Richtungsmatrix samt Queue/Tauri/UI, Resume/Rollback, macOS-Sandbox und echtem SMB-Gegenlauf | abgeschlossen |
| 11 | Linux-GUI und Release | macOS-arm64 App/DMG mit bestandener Developer-ID-/Notarisierungs-/Staple-Abnahme; Linux-Kandidat 0.1.123 am 2026-10-19 einschließlich Paketwechsel und menschlicher Fenster-/Dateioperations-/Dockabnahme geprüft. Veröffentlichung bleibt gesondert freizugeben | bestanden |

Gesamtschätzung: etwa 40–75 Netto-Entwicklertage beziehungsweise 8–15 Vollzeitwochen
für einen Entwickler. Größte Unsicherheiten sind heterogene SMB-/WebDAV-Server,
Zertifikatsfälle bei FTPS und Konflikt-/Recovery-Fälle des bidirektionalen Syncs.

## Abhängigkeiten

- Der Metadatenvertrag samt Plattformproben und UI-Bilanz ist abgeschlossen. Die
  Vollbackup-Warnung bleibt trotz lokalem Realmodus bewusst bestehen, weil das externe
  Backup und dessen Restore nicht technisch verifiziert werden.
- Die zwei mutierenden GUI-Abschlusschecks wurden nach separater Löschfreigabe
  ausschließlich in der markierten Test-Sandbox bestanden.
- Der lokale Realmodus ist umgesetzt und Voraussetzung für Remote-Freigaben.
  Test-Sandbox und Realmodus bleiben unterschiedliche Permit-Typen.
- Die lokale Queue ist fertig. Remote-Einträge erweitern denselben Zustandsautomaten;
  Secret- und Reconnect-Abnahme erfolgt jeweils mit dem zugehörigen Provider.
- Bidirektionaler Sync baut seit Stufe 1 auf dem stabilen einseitigen lokalen und
  Remote-Sync auf. Provider-Matrix, projizierte Ausführung und Baseline-Commit sind
  fertig; nicht belegte Remote-Zielrichtungen bleiben capability-gesteuert gesperrt.
- Release-Kandidaten benötigen macOS- und Linux-Gegenläufe sowie dokumentierte
  Upgrade-/Recovery-Tests.

## Verbindliche Detaildokumente

- Metadaten: [`metadata-stage2-design.md`](metadata-stage2-design.md)
- Realmodus: [`real-mode-design.md`](real-mode-design.md)
- Headless-CLI: [`headless-cli.md`](headless-cli.md)
- Operations-Queue: [`operations-queue-design.md`](operations-queue-design.md)
- Remote-Provider: [`remote-providers-design.md`](remote-providers-design.md)
- Bidirektionaler Sync: [`bidirectional-sync-design.md`](bidirectional-sync-design.md)
- Release: [`release-plan.md`](release-plan.md)
