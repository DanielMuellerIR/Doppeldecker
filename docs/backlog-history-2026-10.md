> Historischer Stand bis 2026-10-31. Keine aktuelle Aufgabenliste oder Releasefreigabe.
> Aktuelle Planung: [Roadmap](roadmap.md) und [Backlog](../backlog.md).

# Aktiver Backlog

Stand: 2026-10-31.

## Abnahme des gewöhnlichen Inhaltssync

Wo ein Provider gewöhnliche Dateien schreiben kann, muss auch Dateisynchronisation
möglich sein. Umgesetzt sind SFTP als Quelle und Ziel sowie FTP/FTPS, WebDAV und
SMB als Ziele für Ergänzen, Aktualisieren und Spiegeln, mit passenden
Best-Effort-Verträgen statt einer pauschalen Quarantänepflicht. Darstellbare
Metadaten bleiben erhalten; Auslassungen brauchen konkrete technische Gründe.
Der verfügbare Stand ist weiterhin in der [Funktionsmatrix](../docs/function-audit.md)
ausgewiesen. Der gemeinsame Registry-Guard ist für ContentSync, beide
bidirektionalen Klassen und den normalen lokalen Sync umgesetzt. Er bindet
bekannte Zwischenstände aller Fenster an Registry, Endpunkte und aktive
Übertragungen, einschließlich lokaler privater Arbeitsverzeichnisse. Namensfilter
und automatisches Löschen ersetzen diese Bindung nicht.

Ein nativer Linux-Gegenlauf bestätigt nach echtem Abbruch während eines Uploads:
Die letzte gemeinsame Baseline bleibt unverändert; der konkrete Rest blockiert
einen neuen Abgleich ohne weitere Remote-Mutation. Nach bewusstem Sichern des
künstlichen Rests außerhalb beider Wurzeln gelingen frisch bestätigter Abgleich
und leerer Folgelauf mit exaktem Bestand und unveränderter Baseline. Die frühere
Abnahme des fehlerhaften Recoverywegs ist damit durch einen neuen Gegenlauf ersetzt.
Native einseitige Linux-Läufe sowie SFTP→lokal auf macOS bestätigen außerdem
Update, Mirror, Quarantäne und Wiederherstellung.

Die strikte bidirektionale Klasse hält den Queuekontext durch Vorbereitung,
Resume, Rollback, aufgeschobene Projektionen und Typwechsel bis zu den
Abschlussscans und dem Baselinecommit. Eigene alte Transferreste bleiben bei
Recovery sichtbar gesperrt; bestehende Journale behalten ihren strikten
Rollbackvertrag. Die folgenden Metadatenmodelle bleiben eigene offene
Produktarbeit.

FTP-Rechte und -Quellrechte, WebDAV-Dateizeiten sowie die getrennte
Ownerdomäne sind umgesetzt. Die echte Nextcloud-Abnahme über die Fensterqueue
bestätigt Ergänzen, Aktualisieren und einen leeren Folgelauf einschließlich exakt
zurückgelesener Dateizeiten. Benannte Datenströme regulärer SMB-Dateien sind jetzt
manifestgebunden im nativen Inhaltssync umgesetzt: Ein echter Samba-Queue-Lauf
bestätigt Ergänzen, reine Streamänderungen, vollständigen Basistausch und leere
Folgeläufe. Ein lokales Ziel erhält den Hauptinhalt mit konkreter Streamauslassung,
die auch beim leeren Folgelauf sichtbar bleibt.
SMB-Verzeichnisstreams werden vollständig gelesen, manifestiert und im
gewöhnlichen NativeBestEffort-Sync zwischen geeigneten SMB-Endpunkten übertragen.
Die Vorschau und Queue bilanzieren dauerhafte Streamwirkungen und bekannte
Teilwirkungen gesondert. Nicht darstellbare Ziele bleiben auch im leeren Folgelauf
sichtbar. SMB besitzt jetzt ein lesendes, manifestgebundenes Windows-SID-/DACL-Modell
mit getrennten Beobachtungslücken je Objekt. Der erste DACL-Schreibvertrag überträgt
geeignete geschützte explizite ACLs auf neue reguläre Basisdateien im gewöhnlichen
NativeBestEffort-Sync. Dieser Temp-Vertrag erhält auch explizite Daten-Schreib- und
Anhängeverbote nach vollständiger Übertragung, mit vorher gewährtem FLUSH-Handle
und originalgebundenem Zeit-/Dateiabschluss. Konkrete Verbote für Lesen/Schreiben
erweiterter Attribute und Dateiausführung bleiben nun auch auf eigenen neuen und
ersetzten Datei-Temps exakt erhalten, ohne diese Rechte beim frischen Abschluss
anzufragen. Auch WRITE_DAC-Denys werden auf Datei-Temps und bei der getrennten
bestehenden Datei-DACL-Wirkung erhalten: Änderungszugriff vor SET, danach frischer
Leseabschluss ohne erneute Änderungsanforderung. Der separate EA-Daten- oder Verzeichnisvertrag wird dadurch nicht erweitert. Geeignete geschützte explizite DACLs können außerdem auf
bereits bestehende SMB-Unterordner übertragen werden. Die Vorschau bestätigt die
dauerhafte Wirkung; Queue und Wiederanlauf behalten bekannte Teilwirkungen und
prüfen offene Ausgänge ohne erneutes Schreiben. Der eigene objektweite Vertrag
für reine DACL-Abweichungen bestehender regulärer SMB-Dateien ist umgesetzt und
plattformübergreifend geprüft. Der
[Prüfumfang](../docs/metadata-stage2-design.md#dacls-bestehender-regulärer-smb-dateien)
benennt die Server- und Oberflächengrenzen. Der begrenzte Dateivertrag umfasst
nun auch explizite Verbote zum Schreiben und Anhängen von Dateiinhalten sowie
konkrete Datei-Denys für Löschen, Attributschreiben, EA-Zugriff und Ausführen.
Zusätzliche Löschrechte des Elternordners bleiben davon unabhängig.
Geeignete bereits gleiche Rechte bleiben auch bei vorbereitetem Temp-Writer ohne
falsche ACL-Auslassung. Er ersetzt keine Inhalte und verlangt
die ausdrückliche Bestätigung aller
bestehenden und künftig angelegten Namen, auch außerhalb der ausgewählten Ordner.
Für selbst neu angelegte, vollständig leere SMB-Unterordner mit vollständig
beobachteter leerer Ordnerstrommenge ist der getrennte begrenzte Rechtevertrag
umgesetzt und auf macOS und Linux geprüft. Der echte Queue-Gegenlauf bestätigt
den Ablauf einschließlich Auslassungen, Teilwirkungen und rein lesendem Abschluss.
[Umfang und Grenzen](../docs/metadata-stage2-design.md#rechte-selbst-neu-angelegter-leerer-smb-unterordner).
Der getrennte frühe Rechtevertrag für geeignete neue nichtleere SMB-Unterordner
ist umgesetzt und im begrenzten Vertrag abgenommen. Plattformprüfungen auf macOS
und Linux sowie echte Samba-Queue-Gegenläufe einschließlich Fehlerfällen sind
bestanden. Die native Sicht- und Bedienprüfung der Originaloberfläche verwendete
eine kontrollierte Brücke; eine echte Tauri-IPC-/SMB-GUI-Ausführung ist damit nicht
belegt. Der Vertrag erhält bei späteren Kind- oder Zeitfehlern den eigenen
Rechtebericht und setzt beim lesenden Abschluss keine Kinderarbeit fort.
[Umfang und Grenzen](../docs/metadata-stage2-design.md#rechte-neuer-nichtleerer-smb-unterordner).
Unbekannte Quellbestände oder Strommengen, weitere ACE-/Vererbungsformen, SID-Übertragung und -Domänenzuordnung sowie WebDAV-ACL-Modelle
und weitere native Metadaten bleiben offen; das sind teilweise
Implementierungslücken und keine pauschalen Protokollverbote. Eine Releasefreigabe
folgt daraus nicht.
Reguläre SMB-Dateien binden nun ihre gemeldete Linkzahl an das Manifest. Bekannte
fehlende Gruppenzuordnung mehrerer ausgewählter Dateien bleibt bei Noop und reiner DACL-Wirkung sichtbar;
Linkzahl 1 braucht keine pauschale Auslassung. Die Zahl umfasst auch Namen außerhalb
der Auswahl und ersetzt weder eine Aliasliste noch einen Hardlink-Schreibvertrag.
Bekannte feinere Sparse-Quellgrenzen auf lokalen APFS-/ext4-/Btrfs-Zielen sind nun eine
konkrete NativeBestEffort-Auslassung statt eines Inhaltsabbruchs. Echte leere
Folgeläufe, Hardlink-Erhalt und ungerades EOF sind Teil des begrenzten
[Darstellbarkeitsvertrags](../docs/metadata-stage2-design.md#lokale-allokationsgrenzen).
Andere Dateisystem- und Speicherschichtgrenzen bleiben offen; NativeStrict
wird nicht herabgestuft.
SMB-Quellen können ihre vollständig gemeldeten Sparse-Bereiche im gewöhnlichen
nativen Sync auf starke lokale Ziele übertragen. Serverabsagen, Puffergrenzen
und ungültige Antworten bleiben konkret bilanziert. Der begrenzte SMB-Zielwriter
erhält nun Sparse-Bereiche lokaler Quellen über eigene neue Temps und einen
objektgebundenen Rename-/Metadatenabschluss. Mit vorbereitetem DACL-Zielkontext
übertragen nun auch geeignete SMB-Quellen Sparse-Bereiche, ADS und die bereits
zulässigen Datei-DACLs gemeinsam über einen einzigen gehaltenen Abschlusszugriff.
Bei einer konkret abgeschlossenen DACL-Zielabsage bleiben unabhängige Sparse-/
ADS-Kopien nun ebenfalls möglich, mit gesonderter ACL-Auslassung und ohne
zusätzlichen DACL-Zugriff. Weitere DACL-Modelle, unvorbereitete oder unklare
Zielkontexte, Servergranularitäten und zusätzliche strikte Zusagen bleiben offen.
Eine gemeldete SMB-Allokationseinheit ist dabei keine Sparse-Grenze: Der
[belegte Gegenfall](../docs/metadata-stage2-design.md#sparse-temps-auf-smb-zielen)
erhält exakte 4-KiB-Bereiche trotz einer gemeldeten 64-KiB-Einheit.
Reine Rechteabweichungen bestehender Dateien behalten ihren getrennten Vertrag;
ein normalisierender Server kann dort weiterhin einen unklaren Rechteausgang melden.
[Zielwriter und Grenzen](../docs/metadata-stage2-design.md#sparse-temps-auf-smb-zielen). Der
[lesende Sparse-Vertrag](../docs/metadata-stage2-design.md#lesende-sparse-bereiche-von-smb-quellen)
trennt Serverbeobachtung, Ressourcenabschluss und native Prüfgrenzen.
WebDAV bilanziert nun pro Objekt die beobachteten ACL-/Owner-Properties samt
konkreten Statuslücken. Auch leere Folgeläufe behalten Bericht und Digestbindung;
lesbarer Inhalt bleibt übertragbar. Ein vollständiges DAV-ACL-Modell und dessen
Schreibvertrag bleiben offen.


Die echte Nextcloud-Probe belegt zwei Clientgrenzen: Collections liefern keine
Dateigröße, und SabreDAV wertet ETag-Bedingungen nur für Dateien aus. Der
gewöhnliche Schreibweg muss deshalb wirksame Dateibedingungen behalten und
Collection-Bedingungen passend behandeln; der strikte Vertrag bleibt getrennt.
Eine leere Collection frisch zu beobachten garantiert bei rekursivem WebDAV-DELETE
nicht, dass während des Requests neu entstandene Kinder erhalten bleiben.

Das dauerhafte Finder-Custom-Icon hat an einem echten notarisierten macOS-Bundle
die strikte Signatur ungültig gemacht. Der Schreibweg ist entfernt; die gespeicherte
Iconwahl wird nur auf das laufende Dock-Icon angewendet. Die native Prüfung des
notarisierten Builds 0.1.124 bestätigt Start, Iconwechsel und gespeicherte Auswahl
nach Neustart. Die strikte Signatur bleibt vor und nach diesen Schritten gültig;
die sechs regulären Bundle-Dateien stimmen danach mit dem geprüften Archiv
überein. Eine Releasefreigabe folgt daraus nicht.

## Alltagstauglichkeit vor zusätzlichen Schranken

Der [Produktaudit vom 2026-10-20](../docs/usability-audit-2026-10-20.md) belegt
14 Einschränkungen und ihre konkreten Vereinfachungsmöglichkeiten. Der praktische
Funktionsumfang, kurze Arbeitswege und effiziente Transfers bestimmen den Ausbau.
Eine allgemeine Vorschaupflicht besteht nicht. Die Empfehlungen
sind im aktuellen Stand durch normale Provideraktionen, effiziente Streaming-
prüfungen, ressourcenbezogene Transfergrenzen und einfachere Wirkungsentscheidungen
umgesetzt. Der [Audit-Umsetzungsstand](../docs/usability-audit-2026-10-20.md#umsetzungsstand)
trennt verfügbare Funktionen, Prüfbelege und bleibende Protokollgrenzen.

Beim nächsten GitHub-Release die Produktbeschreibung und den veröffentlichten
Funktionsumfang auf der Projektwebsite abgleichen. Das allgemeine
Vorschauversprechen wurde bereits am 2026-10-20 von der Website entfernt.

Der [Ausbauplan für die Alltagstauglichkeit](../docs/roadmap.md#beauftragter-ausbau-der-alltagstauglichkeit)
ist beauftragt. Hintergrundarbeit, native lokale Datei-Zwischenablage, verständliche
Konfliktentscheidungen, normaler lokaler Sync und begrenzte SFTP-Datei- und Bauminhalte
sind umgesetzt. Weitere Produktziele und die Kandidatenabnahmen stehen dort in ihrer Reihenfolge.
Die vollständige Codeprüfung und ihre anschließenden Querschnittsprüfungen sind
abgeschlossen; [Abschluss und Prüfgrenzen](../docs/code-review-2026-10-19-abschluss.md).

## Stand der abgeschlossenen Korrekturen

Die Korrekturen vom 2026-10-22 und der Schnellfilter sind im
[Prüfbericht](../docs/code-review-2026-10-22.md) festgehalten. Rekursive Namenssuche,
integrierte Text-/PNG-/JPEG-Vorschau, lokales Duplizieren und lokales
Stapelumbenennen sind inzwischen umgesetzt. Native macOS-/Linux-Läufe bestätigen
Suche und Vorschau lokal sowie über SFTP und die lokalen Schreibaktionen.
WebDAV-Uploads, additive lokale FTP-/FTPS-/SMB-Kopien sowie additive
SFTP-/FTP-/FTPS-/WebDAV-/SMB-Downloads lassen mehrdeutige Namensgruppen innerhalb
ausgewählter Bäume und in direkten Mehrfachauswahlen pfadgenau aus.
Der [Downloadprüfstand](../docs/roadmap-2026-10-24.md) beschreibt die jüngsten Schnitte.
Unabhängige Inhalte bleiben übertragbar; auch Änderungen ausgelassener direkter
und rekursiver SFTP-Mitglieder stoppen weitere Wirkungen vor dem nächsten lokalen
Commit. [Umfang und Prüfgrenzen](../docs/sftp-contents.md).
Lokale ZIP-Erstellung ist ebenfalls umgesetzt und nativ auf beiden Plattformen
geprüft. Die ZIP-Ansicht ergänzt virtuelle Navigation und passive
Inhaltsvorschau mit sichtbaren Duplikaten, Problemzeilen und Teilkatalogen.
Entfernte ZIP-Dateien verwenden vollständige geprüfte Momentaufnahmen mit
sichtbarer Herkunft; aktive Reader bleiben vor der Cachebereinigung geschützt. Das
[lokale Entpacken](../docs/archive-extract.md) ergänzt ganze Archive mit konkreten
Konfliktentscheidungen, Streaming und persistenter Teilbilanz; im Archivbrowser
sind auch eine einzelne Datei oder der geöffnete virtuelle Unterordner auswählbar.
Die [Stapelumbenennung](../docs/batch-rename.md) ergänzt SFTP, FTP/FTPS, WebDAV und SMB
mit gebundenen Einzelwirkungen und providergebundenen Stopgrenzen bei unklarem
Ausgang oder Bindungsverlust. Optionale
Nummerierung folgt der festgehaltenen sichtbaren Auswahlreihenfolge und ergänzt
Start, Schritt, Mindeststellen und Position. Eine separate Option entfernt
gewöhnliche äußere Leerzeichen vor den Namensregeln. Azyklische
Umbenennungsketten sind lokal sowie für SFTP, FTP/FTPS, WebDAV und SMB umgesetzt;
abhängige Zeilen starten erst nach bestätigter Freigabe ihrer Zielnamen.
Lokale exakte Zweierzyklen verwenden einen atomaren Namenstausch mit gemeinsamer
Bilanz beider Zeilen. Lokale reine Case-/Unicode-Schreibweisenänderungen erhalten
einen eigenen atomaren No-Replace-Versuch mit exaktem Abschlussbeleg und sichtbarer
Dateisystemverweigerung. Lokale geschlossene Zyklen ab drei Namen ergänzen
mehrere atomare Austausche und eine eigene bekannte Teilbilanz mit bestätigtem
Namen. Ganze entfernte ZIP-Momentaufnahmen, gewählte reguläre Dateien und virtuelle
Unterbäume lassen sich mit persistierter Quellenbindung in lokale Ziele entpacken. Reine
SMB-Schreibweisenänderungen verwenden einen No-Replace-Versuch und exakte
Rohname-/FileID-Belege. Reine SFTP-Schreibweisenänderungen verwenden den
gewöhnlichen Best-Effort-Rename mit exaktem Rohname-/Attributabschluss und
sichtbarer Alias-/Serververweigerung. FTP/FTPS ergänzt exakte MLSD-Namen und
namenunabhängige beobachtbare Attribute; WebDAV verwendet seinen bedingten MOVE
mit vollständigem Namespace-/ETag-Abschluss auch für reine Schreibweisen.
Entfernte Namenstausche, weitere Provider-/Metadatenabweichungen,
entferntes Packen und entfernte Entpackziele
sowie die übrigen 1.0-Verträge bleiben offen.
Eine neue Release- oder 1.0-Freigabe besteht nicht.

Der vollständige Fenster-/Queue-Review korrigiert ungültige gesetzte
Sandboxwerte, automatische Worker-Neustarts hinter einer Schließschranke sowie
natives macOS-Beenden und das Schließen des letzten sauberen Fensters. Rote
Gegenbelege, App-Suiten und statische Gates bestehen auf macOS und Linux; aktive
Exit-Sperren, parallele Fensterqueues, erneute Kopie nach letztem Schließen und
sauberes Beenden sind nativ geprüft. [Befund und Prüfgrenzen](../docs/code-review-2026-10-19.md#fenster-appstate-und-queue).

**Gewöhnliches WebDAV-Rename:** Der eigene Best-Effort-Weg verwendet einen
bedingten MOVE ohne künstliche Collection-/Löschprobe. Seine Grenzen und der
gezielte Prüfvertrag stehen unter [WebDAV-Umbenennung](../docs/webdav-rename.md).
Gewöhnliche additive lokale WebDAV-Dateiuploads verwenden ebenfalls eine private
Klasse ohne Collection-/Löschprobe, einschließlich begrenzter Verzeichnisbäume,
exaktem Zusammenführen und erhaltenen Ordnerintents; [Vertrag](../docs/webdav-upload.md).
Die frühere AP13-Absage an gewöhnliches WebDAV-Ersetzen beruhte auf dem Versuch,
Temp, Altziel und sämtliche Eltern gemeinsam atomar zu binden. Dieser strikte
Vertrag bleibt unbelegt; normale Ersetzungen und Dauerlöschungen nutzen inzwischen
eine eigene sichtbare Best-Effort-Klasse. FTPS und SMB bieten diese Alltagswirkungen
ebenfalls ohne künstliche Fähigkeitsproben an. Getaggte WebDAV-If-Listen werden
weiterhin korrekt mit ODER ausgewertet. Normale lokale Uploadquellen werden im
Transfer gehasht und halten nur das aktuelle Dateihandle.
Gewöhnliche SFTP-Mutationen stehen in ihrer eigenen
[Schreibklasse](../docs/sftp-writes.md).
AP14 ersetzt die Zeitannahmen der beiden SFTP-Browserfälle durch gehaltene
Antworten und beobachtete Anfrage-/Navigationsgrenzen. Negative Gegenläufe
belegen, dass beide Produktschranken weiter geprüft werden. Die FTPS-Drift ist
reproduziert: Unnötige Schreibproben veränderten eine leere Erstbaseline. Core,
Resume und Rollback prüfen solche Fähigkeiten jetzt nur für wirkliche Zielseiten.
AP15 reproduziert den STOU-Nullbytefall durch vorzeitigen Datenkanal-Drop mit
ungelesenen TLS-Sessiontickets und korrigiert den geprüften Transportabschluss.
Cleanup erhält unklaren oder fremden Bestand; Teilprobenfehler, alte Fähigkeiten
und strukturierte Recoverypfade sind abgesichert.
[Befund, Vertrag und Gegenbelege](../docs/ftps-stou-transport.md).
AP16 bindet die FTPS-Proben-/STOU-Intents dauerhaft vor der ersten Wirkung an das
Fensterjournal. Kopierziele und Move-Quellen prüfen nur ihre benötigten Fähigkeiten;
bereits passiv gesperrte bidirektionale Ziele starten keine Dateiprobe. Reste bleiben
nach Prozessabbruch als manuelle Recovery ohne Retry oder automatische Löschung
sichtbar. Das Implementierungsdelta ersetzt keine offenen Ganzbereichsreviews.
AP17 reproduziert und korrigiert die unnötige Verdrängung paralleler
FTPS-Vorbereitungen verschiedener Pane-Roots. Überlappende Roots, Neuproben
derselben Vorbereitung und Reconnect verwerfen ältere Antworten weiterhin;
verspätete unabhängige Ergebnisse überschreiben keine neuere Cachegarantie.
AP18 trennt No-Replace-Belege für Dateien und Ordner am tatsächlichen Move-Ziel.
Ohne passende Garantie bindet derselbe Core-Weg die bestätigte Best-Effort-Kopie
und Quellentfernung in GUI, CLI und Queue. Gleiche Profile überspringen deren
Anzeige nicht. Verschiedene beobachtete kanonische Roots behalten ihre eigene
Zielverbindung; der bestehende vollständige Konten-/Secret-/Profilroot-Vergleich
bleibt zusätzlich erhalten. Die Gegenbelege und Grenzen stehen im
[FTPS-Vertrag](../docs/ftps-stou-transport.md#ap18-eintragstyp-zielroot-und-tatsächliche-move-wirkung).
AP19 bindet den Abbruch der FTP-/FTPS-Dateivorbereitung an das verursachende
Fenster. Der native Gegenlauf reproduziert vorher einen trotz Escape ausgeführten
Move. Nach Abbruch starten keine weiteren Teilproben oder Nutzerwirkungen;
begonnene Proben behalten ihren geprüften Transport-/Cleanupvertrag und unklare
Ausgänge ihre Intents.
AP20 reproduziert unnötige Inhaltsproben bei ausschließlich inhaltsfreien
Ordnerbäumen. Die gemeinsame Core-Vorbereitung bindet dafür ausschließlich
Ordnerwirkungen; vorhandene Zielordner benötigen keine Zielprobe. Spätere Dateien
bleiben gesperrt und RMD-Antwortverluste dauerhaft unklar ohne Wiederholung.
Prüfstand und Grenzen stehen im [FTPS-Vertrag](../docs/ftps-stou-transport.md).
AP21 reproduziert den unnötigen MKD-/RMD-Versuch eines vollständig übersprungenen
Auftrags und den nativen Fehler noch vor dem Konfliktdialog. Die erste
FTP-/FTPS-Konfliktvorschau bleibt jetzt ohne Mutation; die konkrete Entscheidung
wird danach fenster- und snapshotgebunden vorbereitet. Vollständiges Skip bleibt
terminal wirkungsfrei. Konfliktdrift wird vor Proben abgefangen, und ausgeschlossene
Top-Level-Einträge geben weder Inhalts- noch Quellentfernungsproben vor.
AP22 reproduziert und korrigiert beide Folgegrenzen: Rekursive Skip-Wirkungen
zählen ausgelassene Blätter und benötigen ausschließlich die verbleibenden
Ordner-/Inhaltswirkungen. Quellbäume mit ausgelassenen Kindern bleiben erhalten;
unabhängig vollständig kopierte Quellen behalten ihren manifestgebundenen Abbau.
Passive FTPS-Konflikte und Skip verwenden private Metadatenbelege, tatsächliches
Ersetzen und neue Namen wieder vollständige Inhaltsbelege. Eine passive Bindung
erteilt weder Ersetzungs- noch Aliasfreigabe.
Die menschliche interaktive Linux-Kandidatenabnahme ist am 2026-10-19 bestanden;
der Abschluss steht unter Punkt 9.
Die beiden Fastra-Testhelferfristen sind durch die Entkopplung von frisch
erzeugten ausführbaren Skripten korrigiert. Ein unabhängiger Gegenlauf reproduziert
den Stillstand vor der ersten Skriptzeile; die festen Systeminterpreterfälle
bleiben unter derselben Last innerhalb der Frist. AP22s historischer Teilgate-
Beleg bleibt unverändert erhalten. Nach gesonderter Freigabe besteht auch die
vollständige isolierte macOS-App-Suite mit 204 Tests, einschließlich der beiden
Fastra-Fälle sowie Setup und privaten AppKit-Pasteboards. [Befund und Prüfgrenzen](../docs/test-stability-2026-10-06.md#fastra-testhelfer-start-ohne-neue-ausführbare-skripte).

**Deltareview 2026-10-19:** Bidirektionale Konfliktentscheidungen zeigen die
kumulierte Grenze von 100 lokalen Typwechseln bereits vor der Auswahl. Neue
Konfliktkopien bleiben auch untereinander und bei schrittweisen Entscheidungen
gegen Case-/Unicode-Namensalias geschützt. Rote Gegenbelege, beidseitige Gates
und native Theme-/Tastatur-/Escape-Abnahme sind dokumentiert in
[Deltareview: Dateioperationen und Sync](../docs/code-review-2026-10-19.md).

Der anschließende Queue-Deltareview korrigiert außerdem die generische
FTP-/FTPS-Sync-Verbindungsauswahl bei abweichender kanonischer Zielroot-Abbildung.
Sync und Dateioperationen teilen denselben geprüften Binder. Künstlicher
Protokollgegenlauf und die vollständigen Plattform-Gates sind grün; Details
stehen im selben Prüfbericht.

Der Einweg-Sync-Deltareview ergänzt die tatsächliche Abschlussbilanz dauerhafter
Hardlink-Ersetzungen. Ein roter Dateigegenlauf belegt die zuvor fehlende Zahl;
Additionen bleiben korrekt bei null. Vollständige Plattform-Gates und
Hashbindung sind im selben Prüfbericht dokumentiert.

Der Dateioperations-Deltareview ergänzt die rekursive Inhaltsbindung nativer
Quellordner für „Beide behalten“ sowie die handlegebundene Größenbilanz lokaler
WebDAV-Uploadquellen. Zwei rote Dateigegenbelege und die korrigierten Copy-/Move-
und Uploadprüfungen stehen im selben Prüfbericht.

Der vollständige Realmodus-Review korrigiert den Verlust fremder Kollisionsdateien
im lokalen Restore-Proben-Cleanup und begrenzt Snapshot-Downloads samt Readback
auf die angekündigte Größe. Bestätigte und unklare Probephasen bleiben getrennt;
die Gegenläufe und Prüfgrenzen stehen im selben Bericht.

**Review 2026-10-16:** AP-025 ist abgeschlossen. Die neuen Vollbereiche und
Hashbelege stehen in `.codeqa/`: Vor dem Produktausbau waren 18 von 26 registrierten
Bereichen aktuell abgeschlossen. Betroffene Bereiche brauchen jetzt erneut ihre
Deltaprüfung; acht Ganzbereiche und der Querschnitt bleiben offen.
Sync-Bedienzustand, Teilfehlerrefresh, Einstellungen, Provider-GUI-Worker,
CLI-Listing, FTP-/WebDAV-Interoperabilität und DMG-Fehlerpfade wurden korrigiert.
Prüfstand und genaue Grenzen:
[Review und Alltagstauglichkeit](../docs/code-review-2026-10-16.md).

**Hintergrundarbeit und Clipboard:** Navigation, weiteres Enqueue, zwei parallele
Fenster, Finder-Übergabe in beide Richtungen, Move und Queue-Neustart sind nativ auf
macOS belegt. Große Auswahl, verspätete Antworten, Textfelder und unmittelbares
Copy/Paste sind zusätzlich automatisiert geprüft. Linux-GTK-Formate sind mit einem
unabhängigen Gegenprozess geprüft; die Linux-Fenster-/Dateioperationsabnahme
unter Punkt 9 ist bestanden. Vertrag: [Hintergrundarbeit](../docs/background-file-work.md).

**Konflikte und Sync-Entscheidungen:** Beide Endpunkte, zulässige Erstbaseline-
Entscheidungen und kollisionsfreie Konfliktkopien sind umgesetzt. Copy/Move mit
langen Unicode-Namen, beide Themes, Tastatur/Escape, späterer Dialogabbruch und
Pane-Tausch sind nativ im markierten Testbereich geprüft. Eine unmittelbare
fensterübergreifende Übergabe von 10.000 Dateien blieb vollständig; ihr Copylauf
wurde nach drei Dateien regulär abgebrochen. Core-/App-/CLI-/Browser-/Tool-Gates
und der isolierte Linux-Gegenlauf sind grün. Vertrag und Grenzen:
[Konflikte vergleichen und entscheiden](../docs/conflict-decisions.md).

**Normaler lokaler Sync:** Der abgesicherte und begrenzte Best-Effort-Start für
AddOnly/Update ist umgesetzt:
wirkungsbezogene Bestätigung, ContentHash für Update, Fensterqueue und frische
Zustimmung nach Neustart. Reine Additionen benötigen keine Quarantäneprobe oder
Geschwister-Schreibrechte. Native Abnahme belegt echte Ausführung, Simulation,
Drift, Quarantäne, Neustart, Teilabbruch und parallele Fenster. Ein echter
Prozessabbruch mit privatem Staging wird der verursachenden Queue zugeordnet.
Eigene APFS-/ext4-/exFAT-Images auf beiden Plattformen belegen den normalen
Sync-Inhaltslauf, pfadgenaue Metadatenauslassungen und dauerhafte schwache
Ersetzungen. Neunamencommit und Flushzusagen bleiben getrennt gebunden;
schwache Temp-/AppleDouble-Reste werden nach Prozessabbruch über einen privaten
Queueintent gemeldet. Strikte Remote-/Mirror-/bidirektionale Gates bleiben erhalten.
Die native Best-Effort-Abnahme einschließlich Rechtebilanz, dauerhafter Ersetzung,
Simulation und zweier Neustarts ist bestanden; eigenes Cleanup ist bestätigt.
Vertrag und konkrete Zielgrenzen: [Lokaler Sync](../docs/local-sync.md).

**SFTP-Anzeige:** Nicht bedienbare Namen und fehlende Dateitypen blockieren
unabhängige gültige Einträge nicht mehr. Anzahl und Gründe stehen direkt in der
Pane sowie im CLI-Listing. Noch nicht dekodierbare Protokollnamen und unvollständige
Antworten bleiben Fehler; weitere Provider und rekursive Auslassungen bleiben
unter Roadmap-Punkt 5 offen. [Vertrag](../docs/sftp-listing.md).

**SFTP-Inhalte:** Reguläre Dateien lassen sich über geprüfte private
Momentaufnahmen öffnen und mit Verzeichnisbäumen lokal kopieren. Hostkey,
Sitzung, Pfadattribute, READ-Handles und Streaminghash sind gebunden;
Standardsnapshots verwenden einen Remote-Inhaltsdurchlauf, zusätzliche
Vollverifikation ist wählbar. Feste Datei-/Auswahl-/Baumgrößen- und Tiefenlimits
sind entfallen; die fehlende atomare
Root-/Elternbindung bleibt ausdrücklich Best Effort. Vorhandene Ziele bleiben
geschützt; Konfliktkopien nutzen lokale exklusive Neunamen. Vorbereitung und Lauf
sind abbrechbar, Queue-Neustart verbindet frisch und Crashreste haben einen privaten
Tempintent. Grenzen und Prüfstand: [SFTP-Inhalte](../docs/sftp-contents.md).

**WebDAV-Ordneranlage:** Gewöhnliches mkdir verwendet eine eigene Best-Effort-Klasse
ohne künstliche Löschprobe. Elternbedingungen, vollständige Listings und Readback
bleiben erforderlich; unklare Ausgänge erhalten den möglichen neuen Ordner.
[Vertrag und Grenzen](../docs/webdav-folder.md).

**SFTP-Schreibfunktionen (2026-10-20):** Upload/Kopie auf den Server, Ordneranlage,
Umbenennen/Verschieben, bestätigtes Ersetzen und dauerhaftes Löschen sind als
gewöhnliche Best-Effort-Aktionen implementiert. Gleichserver-Moves vermeiden die
Inhaltsübertragung. Fehlendes fsync und eine fehlende atomare Elternbindung sind
keine pauschalen Schranken; das akzeptierte Pfadrisiko bleibt sichtbar. Metadaten
außer Inhalt/Ordnerstruktur, strikter Sync und Quarantäne sind weiterhin Grenzen.
[Schreibvertrag und Prüfstand](../docs/sftp-writes.md).
Priorität, Umfang und Abnahme stehen in der [Roadmap](../docs/roadmap.md#beauftragter-ausbau-der-alltagstauglichkeit).
Die interaktive Linux-Kandidatenabnahme unter Punkt 9 ist bestanden.
Fehlende Komfortparität allein sperrt ein klar
bezeichnetes Preview nicht.

**Recoverybilanz über mehrere Teilaktionen (Review 2026-10-02):** Ein späterer
Sync-Aktionsfehler und der erfolgreiche beziehungsweise abgebrochene Provider-/
SFTP-Queueabschluss erhalten die Pfade seit `fabc53b` (2026-10-03), auch bei einem
sekundären Audit- oder Journalfehler. Mehrdatei-Reproduktion und Journal-Reopen
sind auf macOS und Linux belegt; siehe
[`docs/code-review-2026-10-03.md`](../docs/code-review-2026-10-03.md).
Späte bidirektionale Folgefehler und die Erfolgs-/Fehlerübergabe an die Queue
sind seit 2026-10-06 ebenfalls korrigiert und mit mehreren künstlichen Projektionen
sowie Journal-Reopen geprüft. Der ursprüngliche Befund ist damit abgeschlossen;
Vertrag und Prüfbelege: [Recoverybilanz über mehrere Aktionen](../docs/recovery-aggregation.md).


Die folgende nummerierte Liste erhält die stabilen Bezeichner früherer
Reviewbefunde. Ihr jeweiliger Status und Prüfbeleg sind maßgeblich; erledigte
Punkte sind keine offene Restarbeit. Die CodeQA-Historie bleibt separat erhalten.
Der abgeschlossene Linux-Kandidatengegenlauf steht unter Punkt 9.

**Headless auf Linux belegt:** Der Gegenlauf vom 2026-09-04 ist auf dem Testhost
Linux-Testhost im `rust:1.96`-Container (Linux x86_64, frischer Klon unter `/tmp`)
für Commit `ffe4903` vollständig grün — `fmt`, `cargo test -p doppeldecker-core`,
`cargo test -p doppeldecker-cli`, `clippy -p doppeldecker-core -p doppeldecker-cli
--all-targets -- -D warnings` und `cargo check --manifest-path fuzz/Cargo.toml
--locked --all-targets`. `rustfmt` und `clippy` fehlen im Image und werden per
`rustup component add` nachgeholt; `bash -lc` überschreibt den `PATH` des Images
und verliert Cargo, deshalb `bash -c`.

**Auch die GUI-Schicht baut dort inzwischen headless:** Mit
`apt-get install libwebkit2gtk-4.1-dev libssl-dev libayatana-appindicator3-dev
librsvg2-dev libxdo-dev file` im selben Container läuft `cargo test -p doppeldecker`
und ist mit 135 Tests grün (macOS: 137, der Unterschied sind die plattformeigenen
Tests). Der erste solche Lauf am 2026-09-04 hat zwei Fehler aufgedeckt, die auf
macOS gar nicht auftreten konnten und beide behoben sind: zwei gleichnamige
Testmodule in `browser_commands.rs` (E0428) und die als macOS-only markierte
Auswurf-Frist in `system_mount_commands.rs` (E0425).

1. **`local-parked-backup-outcome`** — Im privaten lokalen C2-Laufkontext ist
   die belegte Übertragung jetzt erfolgreich; eine erhaltene Verzeichnissicherung
   wird getrennt mit ihrem Prüfpfad bilanziert. Reguläre Sicherungen werden erst
   nach Ziel-/Metadatenreadback und Durability entfernt. Unbelegte Identitäten,
   Commit-/Restorephasen und spätere Elterndrift bleiben `outcome_unclear`.
   Ohne nachweisbar privaten Kontext bleibt der bisherige konservative Weg bestehen.
   Grenzen stehen in [`docs/decision-lokale-restbereinigung.md`](../docs/decision-lokale-restbereinigung.md).

2. **`provider-probe-root-readback` — korrigiert am 2026-10-06.** WebDAV, FTPS
   und SMB lesen nach Erfolgs- und Fehlerpfad Auswahl- und Profilroot vollständig
   auf Probenreste zurück. FTPS verwendet getrennte identitätsgeprüfte Lesesitzungen,
   SMB bindet die Listings an die ursprüngliche Generation. Neue Fähigkeiten
   erscheinen erst nach erfolgreicher Restprüfung. macOS/Linux-Schutztests sowie
   lokaler FTPS-/FTP-Gegenlauf und verschlüsselter Samba-E2E sind grün; ein
   verweigertes Profilroot-Listing reproduzierte vorher die fehlende SMB-Prüfung.
   Vertrag und Grenzen: [`docs/remote-providers-design.md`](../docs/remote-providers-design.md).

3. **`local-metadata-capabilities-probe` — korrigiert am 2026-10-06.** Die rein
   lesende Abfrage folgt dem gehaltenen Root und dessen Mount. macOS verwendet
   gültige Volume-Bits, Linux den tatsächlichen Treiber samt ACL-/xattr-Abfragen.
   FAT/exFAT erhalten keine falschen nativen Zusagen; unbekannte Treiber bleiben
   `Unknown`, nur lesbare Mounts `ReadOnly`. Eigene APFS-/ext4-/exFAT-Images belegen
   Vorschau und tatsächlich kopierte unterstützte Metadatenklassen; xattrs bleiben
   auf macOS sichtbar nur lesbar. Vertrag und Grenzen:
   [Lokale Metadatenfähigkeiten](../docs/local-metadata-mounts.md).

4. **`linux-mount-listing-deadline` — korrigiert am 2026-10-06.** Speicherabfragen
   verwenden begrenzte, getrennte GNU-stat-Prozesse mit einer gemeinsamen Frist
   und höchstens vier offenen Kindern. Nach SIGKILL folgt kein blockierendes Wait.
   Fehler verschwinden nicht mehr: Mount, unbekannte Größen und `space_error`
   bleiben erhalten; die Seitenleiste erklärt den Fehler. Plattform- und
   Lifecycle-Vertrag: [Geräte-Seitenleiste](../docs/goal-spalten-seitenleiste.md).

5. **`state-holding-commands-off-ui-thread` — korrigiert am 2026-10-06.**
   Die drei Commands verwenden `AppHandle` und holen State erst im Blocking-Worker.
   Listing-Reconnect und frische Profilbindung bleiben erhalten. Remote-Dateiöffnung
   reserviert vor dem Providerzugriff und nutzt den Abbruchtoken ihres Fensters;
   Terminalevent vor dem nächsten Queueworker verhindert eine verspätete UI-Freigabe.
   App-/Browser-Gegenläufe sind grün; das echte macOS-Tauri-Fenster blieb während
   künstlicher SSH-Wartezeit bedienbar (größte Framelücke 50 ms), Abbruch und
   anschließende UI-Freigabe waren grün. Vertrag und Grenzen:
   [GUI-Hintergrundarbeit](../docs/gui-background-commands.md).

6. **`sftp-mkdir-machinery-is-test-only` — erledigt am 2026-10-06.** Nach der
   bestätigten Entscheidung sind die ungenutzten Proben-/mkdir-Helfer und ihre
   18 abhängigen Tests entfernt. Die 16 Schutz-, Listing-, Identitäts- und
   Fehlerzuordnungstests prüfen produktive Funktionen. Damals blieben sämtliche
   SFTP-Mutationen gesperrt. Seit 2026-10-20 ergänzt eine getrennte gewöhnliche
   Schreibklasse diesen damaligen Stand; striktes Sync-VFS bleibt gesperrt.

7. **Bidirektionale CLI-Tests unter Last — korrigiert am 2026-10-06.** Der
   Resume-Test lieferte im zehnten parallelen Suite-Lauf erneut `EAGAIN` beim
   nachfolgenden Baselineabschluss. Ein deterministischer Fork-Gegenbeleg war
   auf macOS/Linux rot: Ein geerbter FD verlängerte flock nach dem Ende des
   ursprünglichen Besitzers. Der Store gibt nun beim Ende dieses Besitzers
   ausdrücklich frei; eine abweichende PID darf den Elternlock nicht freigeben.
   30 weitere parallele Linux-Suiten unter Last waren grün (1.260 Tests).
   Beleg: [`docs/test-stability-2026-10-06.md`](../docs/test-stability-2026-10-06.md).

8. **`sftp-signal-test-timing` — korrigiert am 2026-10-19.** Ein neuer roter
   Gatefall war vor dem Signalversand bis zum verzögerten Start frischer
   Python-Attrappen eingegrenzt. Die Klasse bereitet unveränderliche Attrappen
   jetzt einmal mit begrenztem Setup vor; jeder Fall behält getrennte Zustände.
   Signalphasen und bestehende Fallfristen bleiben erhalten. Der vollständige
   macOS-Werkzeuglauf besteht mit 146 Tests, die Linux-Signalklasse mit 26 Tests.
   [Gegenbelege und Grenzen](../docs/code-review-2026-10-19-sftp-signale.md).

9. **Linux-Geräteabnahme — bestanden am 2026-10-19.** Der technische
   Gegenlauf vom 2026-10-19 besteht mit neuen signierten AppImage-/DEB-Paketen:
   Installation, erster WebKitGTK-Fensterstart, echter Versionswechsel von einem
   unverändert neu gebauten 0.1.122, Deinstallation mit erhaltener Konfiguration,
   getrennte Fensterqueues und echte F2-/F5-/F6-Läufe. Dabei gefundene Paketprüfer-
   und Linux-Menüfehler sind korrigiert; offene Queue-Aufträge behalten ihre
   nativen Schließ-/Beendenrückfragen. Die Tests liefen in eigenen Anzeigen und
   einem isolierten Container. Die menschliche Abnahme von Darstellung,
   F2/F5/F6 und zweitem Fenster in der normalen Linux-Gerätesitzung ist bestanden.
   Das anschließend gemeldete fehlende Dockicon ist korrigiert: GTK erhält
   256-Pixel-Icons und das Hauptfenster die gespeicherte Variante. AppImage und
   DEB-GUI bestehen den nativen Gegenlauf; auch das sichtbare Dockicon ist
   menschlich bestätigt. Es gibt keine Veröffentlichungsfreigabe.
   [Kandidat und Prüfgrenzen](../docs/linux-kandidat-2026-10-19.md).

10. **Sichere lokale Temp-Restbereinigung:** C2 hält einen privat erzeugten
    Arbeitsbereich und den tatsächlichen Ziel-Elterndeskriptor. Abbruch und Fehler
    entfernen eigene Temps ausschließlich im gehaltenen privaten Namensraum.
    Ohne diesen Nachweis bleiben Temps mit sichtbarem Recoverypfad erhalten;
    unbedingtes Cleanup im gemeinsam beschreibbaren Zielordner entfällt.
    Die Datei-ACL-Vererbung wird ausdrücklich aus dem Ziel-Elternordner nachgebildet
    und an dessen frische Identität und Metadaten gebunden. Queueoperationen
    journalisieren ihre Phasen und erkennen Crashreste ohne automatische Löschung.
    SELinux-aktive und unbekannte Rechte-/Owner-Modelle erhalten keine C2-Garantie.
    Die allgemeine lokale VFS-Fähigkeit für bedingtes Entfernen bleibt unverändert
    deaktiviert. Der ältere CodeQA-Kandidat ist seit 2026-10-16 begründet geschlossen;
    der private Besitzvertrag bleibt maßgeblich.

## Abgeschlossen

- **Dock-Icon nach App-Ende (2026-10-13):** Als bekannte Finder-/Dock-Cache-Eigenheit
  akzeptiert. Einstellung, natives Bundleicon und laufendes Dockicon sind korrekt;
  eine veraltete Dock-Anzeige lässt sich nach Nutzerbeobachtung durch
  „Optionen → Im Finder anzeigen“ aktualisieren. Dokumentiert, keine weitere
  Untersuchung oder Produktänderung vorgesehen.
  [Prüfung und Abhilfe](../docs/settings-session-diff.md#visuelle-dock-abnahme-2026-10-13-abgeschlossen-mit-cache-einschränkung).

- Lokale Dateioperationen auf macOS-exFAT besitzen einen volumenbezogenen,
  an den Zielroot gebundenen Best-Effort-Weg. Tatsächliche APFS-/ext4-/exFAT-
  Gegenläufe prüfen Queue, Roottausch, Kollision, Abbruch, Kopie und Verschieben;
  die native macOS-GUI und CLI bestätigen vor jeder Übertragung. Strikte
  Sync-Commits bleiben exklusiv. [Vertrag und Grenzen](../docs/local-metadata-mounts.md).

- Finder→FTPS ist seit 2026-10-07 als integrierter nativer macOS-GUI-Lauf belegt:
  tatsächliche Finder-Geste, TLS-Testserver, Pane-Ziel trotz Ordnerzeile,
  bytegleiche Kopie, Konfliktvarianten, Fortschritt, Abbruch und Schreibfehler.
  Alle Quellen bleiben erhalten. Konkrete Fehler-/Sperrgründe bleiben neben der
  Drop-Gruppenbilanz sichtbar. [Prüfvertrag und Grenzen](../docs/finder-ftps-gui.md).

- Der private Papierkorb-Kern und beide Plattformadapter sind implementiert und
  auf macOS/Linux headless sowie mit tatsächlichen Mount-Gegenläufen geprüft.
  Die gemeinsame native macOS-GUI-Sandbox-Abnahme vom 2026-10-07 belegt die
  erhaltene Dateiidentität im künstlichen Papierkorb, fehlenden Quellpfad und
  leeres Journal. Vertrag und Grenzen: [privater Papierkorb-Kern](../docs/trash-private-claim.md).
- Mehrere unabhängige Tabs je Pane sind einschließlich Leisten, Tastaturbedienung,
  frischen Listings, Auswahl-/Scrollwiederherstellung und Sitzungsformat 2 verfügbar.
  Modell-, Persistenz- und Browsertests sowie die native macOS-Abnahme mit echtem
  App-Neustart vom 2026-10-07 bestehen. Queue und Permits bleiben je Fenster.
  Vertrag: [Pane-Tabs](../docs/pane-tabs.md).

Die übrigen lokal umsetzbaren Entwicklungsbefunde sind abgearbeitet und durch den
vollständigen Headless-Gate-Lauf belegt:

- Ordnergrößen sind lokal optional automatisch und auf FTP/FTPS, WebDAV sowie
  SMB einzeln per Klick verfügbar. SFTP besitzt eine eigene begrenzte frische
  [Metadatenbeobachtung](../docs/sftp-directory-sizes.md) mit sichtbarem Best Effort;
  strikte rekursive Listings bleiben gesperrt. Der Informationsdialog zeigt Listing-Metadaten
  für Ein- und Mehrfachauswahlen und berechnet rekursive Größen und Zähler erst auf
  ausdrückliche Anforderung; Darstellungsoptionen und Informationen sind zusätzlich
  über native Menüs und Standardkürzel erreichbar.
- Der damalige geprüfte Stand `94cc802` ist am 2026-08-25 auf dem
  Linux-Testhost in einem frischen Klon unter `/tmp` geprüft. In einem
  `rust:1.96`-Container sind die vollständigen Core-Tests (731 bestanden, vier
  ignoriert), die CLI-Tests (56 bestanden) sowie
  `cargo clippy -p doppeldecker-core --all-targets -- -D warnings` grün. Ein dabei
  gefundener Linux-spezifischer Clippy-Fehler in der Geräte-ID des
  Quarantänebelegs ist in diesem Stand portabel korrigiert; der zugehörige
  Purge-Rennbedingungstest lief danach noch einmal gezielt. Der Linux-Zweig in
  `core/src/acl.rs` samt Default-ACL-Test ist Teil dieses Laufs. Nicht gelaufen ist
  dort `cargo test -p doppeldecker`: Die GUI-Schicht braucht
  WebKitGTK-4.1-Dev-Bibliotheken, die im Container fehlen.
- Die Release-Verträge prüfen Verhalten statt Quelltextfragmente: Der Buildtreiber
  läuft mit injizierter Umgebung und gepatchten Schritten, sodass beobachtbar ist,
  dass ohne Signierschlüssel kein Buildschritt anläuft und nur der Updaterlauf
  `createUpdaterArtifacts` setzt. Der Install-Preflight läuft gegen ein kopiertes
  Projektabbild mit je einer gezielten Änderung an der Updater-Konfiguration.
- Profile besitzen pro Protokoll dauerhaft monotone Revisionen. Cache-Handshakes,
  Pane-Listings, Pläne, Konfliktlisten, Permits und persistierte Remote-Queueeinträge
  werden daran gebunden; Save/Remove kann keinen alten Handshake oder Pending-Lauf
  nachträglich veröffentlichen.
- Eine Profiländerung verwirft nur davon abhängige RAM-Vorbereitungen. Wartende
  `RemoteRun`-Queueeinträge wechseln über `discard_remote_preparation` zurück nach
  `Pending`, und nur das zugehörige Fenster erhält den neuen Queue-Snapshot.
- Panes, Favoriten, Dateiaktionen, Sync, Quarantäne und neue Fenster führen die
  sichtbare Profilrevision in jedem Remote-Endpunkt. Profilereignisse entwerten nur
  betroffene alte Panes; ein Remote-Request ohne Revision stoppt als Plan-Drift.
  Speichern und Entfernen nutzen zusätzlich einen Compare-and-Swap unter dem
  Profildatei-Lock, damit ein veralteter Editor keinen neueren Profilstand verändert.
- Temp-Recoverypfade abgebrochener Dateiübertragungen bleiben atomar im terminalen
  Queueeintrag, im Audit und in beiden GUI-Abschlussansichten auffindbar. Dasselbe
  gilt für Fehlerpfade und einen zusätzlichen fehlgeschlagenen Journalcommit;
  fachlicher Ausführungsfehler und Journalfehler bleiben getrennt erhalten. Eine
  bereits vollständig vollzogene Wirkung wiederholt nur noch ihren terminalen
  Abschlusscommit und wird nie als gewöhnlich retryfähiger Fehler gespeichert.
- Profilrevision, Pfad, Dateiname und Sync-Optionen werden vor jedem Providerkontakt
  und jeder mutierenden Capability-Probe geprüft. Profilentfernung invalidiert über
  ein Tombstone-Ereignis sofort alle betroffenen Fenster, Panes und Favoriten.
- Späte Ergebnisse von Rename, Ordneranlage sowie Quarantäne-Restore und -Leeren
  bleiben sichtbar; nur weiterhin passende Pane- und Badge-Sichten werden geladen.
- Migrierte WebDAV-Löschjournale besitzen einen typisierten Abschluss: ungestartete
  Altpläne lassen sich sicher verwerfen, mögliche Teilwirkungen nur nach bestätigter
  manueller Prüfung terminal schließen. Eine Wiederholung bleibt ausgeschlossen.
- Quarantäneläufe besitzen prozessweit gebundene Live-Leases; Restore und Leeren
  können nicht mehr aus einem anderen Fenster in einen aktiven Lauf eingreifen.
- Das macOS-Updater-Archiv wird erst aus der notarisierten und gestapelten App neu
  gepackt und signiert. Alle Release-Signaturen werden gegen genau ihr Artefakt und
  den ausgelieferten öffentlichen Schlüssel kryptografisch geprüft.
- Virtualisierte Pane-Listen behalten die aktive Option über einen semantischen
  Proxy, wenn die Cursorzeile außerhalb des gerenderten Ausschnitts liegt.
- permanentes WebDAV-Löschen nutzt einen frischen starken ETag mit `If-Match`; der
  typ- und größengleiche Austausch ist als echte Server-Rennbedingung getestet,
- das CLI bietet den sicheren lokalen Papierkorbvertrag über `trash --item`,
- frische Bundle-Builds installieren die gesperrten Node-Abhängigkeiten selbst und
  brechen ohne Eingabe-TTY sofort bei fehlendem Notarisierungsprofil ab,
- `.github/workflows/ci.yml` führt `tools/check-gates.py` auf macOS 14 und Ubuntu
  24.04 aus,
- der manuelle, zweistufig bestätigte Updater ist in Backend und Frontend umgesetzt;
  sein echter öffentlicher Schlüssel und der HTTPS-Manifest-Endpunkt sind bereits im
  Bootstrap-Build gebunden, signierte macOS-App-, AppImage- und DEB-Nutzlasten bleiben
  im Release-Manifest getrennt.

## Veröffentlichung

Die erste öffentliche Preview [0.1.124](https://github.com/DanielMuellerIR/Doppeldecker/releases/tag/v0.1.124)
ist seit 2026-10-19 für macOS und Linux veröffentlicht. Der frühere Auftrag zur
Erstveröffentlichung einschließlich Produktlinks, Paketen und Website ist erledigt.
Weitere Releases benötigen weiterhin einen konkreten Veröffentlichungsauftrag.

Vor dem nächsten Release Funktionsmatrix, README beider Sprachen, Release Notes
und Projektwebsite auf denselben tatsächlich ausgelieferten Funktionsstand
bringen. Gewöhnliche Dateiaktionen erhalten keine allgemeine Vorschaupflicht als
Werbeversprechen. Die aktuellen Änderungen sammeln sich bis dahin unter `Unreleased`.

## Bedingter Betriebsbefund

Nur falls ein FTP-/FTPS-Profil tatsächlich mehrere Serverknoten hinter derselben
Adresse verwendet, muss die Capability-Probe je Knoten gebunden werden. Für den
belegten Einzelserverbetrieb ist das keine offene Produktwirkung. Die Grenze und die
beiden zulässigen Lösungen — Probe je Mutationssitzung oder Probe und Mutation auf
derselben Verbindung — stehen in `core/src/ftps.rs` und
[`docs/remote-providers-design.md`](../docs/remote-providers-design.md).

## Exakte Objekt-IDs in Oberflächenberichten

Die Anzeigeprojektion überträgt auch historische V7-Objekt-IDs als exakte
Dezimalzeichenfolgen. Verschiedene Objekte desselben Pfads bleiben oberhalb 2^53
in Fortschritt, Queue und Fehlerberichten unterscheidbar. Die numerische
V7-Persistenz, ihre Digests und die internen u64-Identitätsprüfungen bleiben erhalten.
Der zweite Zeit-Readback neuer leerer SMB-Ordner darf außerdem bekannte
Objektdrift nicht mehr übergehen. [Korrekturen und Prüfgrenzen](../docs/code-review-2026-10-31.md).
