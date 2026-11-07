# Gewöhnliche SFTP-Dateiaktionen

Stand: 2026-10-24

## Ausführungsklasse

Gewöhnliche SFTP-Dateiaktionen verwenden eine eigene interaktive Best-Effort-Klasse:
Ordner anlegen, Dateien und Ordnerbäume kopieren oder hochladen,
umbenennen, verschieben, ausdrücklich bestätigte reguläre Dateien ersetzen und
ausgewählte vollständige Bäume dauerhaft löschen. Sie benötigen ausschließlich
das normale SFTP-Subsystem; SSH-Exec und zusätzliche Serverprogramme sind keine
Voraussetzung. Es laufen keine künstlichen Schreib- oder Löschproben.

Diese Klasse ist eine bewusste Ausnahme vom bisher für sämtliche SFTP-Mutationen
verlangten atomaren Rootvertrag. SFTP v3 bindet seine Pfadbefehle nicht atomar an
den Pane-Root, einen Elternhandle oder eine konkrete Eintragsidentität. Frische
LSTAT-/REALPATH-Prüfungen können einen zwischenzeitlichen Austausch nicht
vollständig verhindern: Eine Wirkung kann dann einen anderen gleichnamigen
Eintrag und bei einem Elternpfadtausch auch einen Bereich außerhalb des gewählten
Pane-Roots im selben Serverkonto treffen. GUI und CLI zeigen dieses verbleibende
Risiko vor der gewählten Wirkung. Eine beobachtete Abweichung beendet den Vorgang;
sie ist kein Grund, nach dem Schreiben einen ungebundenen Rückweg zu versuchen.

Die öffentliche `SftpVfs` bleibt für strikte Listings, Rootbelege, Schreibzugriffe,
Sync, Mirror und Quarantäne passiv gesperrt. Die private interaktive Sicht meldet
weder atomare No-Replace-/Replace-Fähigkeiten noch `remove_if_matches`. Ihre
Rootbeschreibung ist ein beobachteter Namespacebeleg für gewöhnliche Dateiaktionen
und Löschvorschauen, keine Behauptung einer descriptor-relativen Bindung. Die
strikte Sync-Materialisierung verwendet weiterhin die öffentliche Sicht.

## Prüfung und Wirkung

Vor Netzwerkzugriff werden die normalisierten absoluten Pfade, direkte Auswahl,
Namen, überlappende Roots und lokale Sandboxgrenzen geprüft. Die private Sicht hält
Hostkey, Server-/Profilroot, Sitzungsgeneration, Abbruch und gegebenenfalls die
Profilrevision des Fensterauftrags. Jede pfadbasierte Mutation prüft unmittelbar
vorher die kanonische Root-/Elternkette ohne bekannte Symlinks. Geöffnete
Dateihandles bleiben bis zum explizit geprüften Abschluss gehalten. READ-/WRITE-
Pakete prüfen nur Abbruch und die gehaltene Sitzung im RAM; Profilrevision und
Elternpfade werden vor OPEN, nach Handleabschluss und vor dem Commit erneut
geprüft. Unterschiedliche Profile behalten ihre eigenen Konfigurationen, während
die gemeinsamen Revisionsbindungen des Queueauftrags vollständig geprüft werden.

- **Ordner:** vollständiges Elternlisting, sichtbare Kollision, genau ein MKDIR,
  erneute Elternprüfung und Typreadback. Ein unklarer Ausgang hinterlässt einen
  Prüfpfad, keinen automatischen Cleanup.
- **Kopie und Upload:** exklusives CREATE/EXCL in einem zufälligen Tempnamen,
  vollständiger Transfer am gehaltenen Handle, explizites CLOSE, optional die
  angekündigte `fsync@openssh.com`-Erweiterung und vollständiger BLAKE3-Readback.
  Erst danach erfolgen frischer Zielcheck, genau ein Standard-Rename und finaler
  Inhaltsreadback. Fehlendes fsync wird als schwächere Dauerhaftigkeit ausgewiesen.
- **Ersetzen:** Konfliktvorschau und ausdrückliche Ersetzungsentscheidung binden die
  bisherige reguläre Datei. Nach vollständig geprüftem Tempinhalt und frischem
  Altzielcheck verwendet der Server `posix-rename@openssh.com`, wenn Version 1
  angekündigt ist. Andernfalls ist der bestätigte irreversible Weg REMOVE des
  Altziels, Abwesenheitsreadback und anschließend Standard-Rename. Scheitert der
  Commit danach, kann das Altziel bereits fehlen; Temp und Ziel bleiben sichtbare
  Prüfpfade. Ordner werden ausschließlich zusammengeführt, niemals rekursiv ersetzt.
- **Rename:** frisches vollständiges Elternlisting, Kollisionsprüfung, genau ein
  Standard-Rename, danach Prüfung beider Namen und des Ergebnistyps; reguläre
  Inhalte werden beim Tempcommit zusätzlich zurückgelesen. Gewöhnliches F2
  überträgt keine Dateibytes. Das liefert keine atomare No-Replace-Zusage.
  Reine Case-/Unicode-Schreibweisenänderungen im [Stapel](batch-rename.md)
  verwenden denselben einzelnen Versuch. Ein vollständiger Endscan muss den
  exakten neuen Rohnamen und passende beobachtbare Attribute bestätigen. Eigene
  aufgelöste Aliase können schon vor dem Request blockieren; es gibt keinen
  POSIX-Rename-Ersatzweg.
- **Move:** Zwei Pane-Roots derselben gehaltenen SFTP-Sitzung verwenden bei freien
  Zielnamen genau ein gewöhnliches Rename, ohne Datei-OPEN oder Inhaltskopie.
  Kollisionen erhalten einen neuen Bestätigungsbedarf. Verschiedene Verbindungen
  und Zusammenführungen verwenden den gemeinsamen Kopier-/Move-Kern, vollständigen
  Zielreadback und frischen manifestgebundenen Quellcheck. Quellabbau entfernt Dateien einzeln
  und Ordner ausschließlich frisch leer. Er liefert keine atomare Objektbedingung.
- **Dauerlöschen:** konkrete Auswahl und vollständige Metadatenlistings im
  bestätigten Plan, ohne sämtliche Dateibytes für eine Löschung zu übertragen; unmittelbar vor Ausführung neu planen und vergleichen. Jeder
  Record wird vor seiner Einzelwirkung erneut geprüft. REMOVE beziehungsweise
  RMDIR wird genau einmal gesendet, Abwesenheit danach fehlererhaltend geprüft.
  Unvollständige Listings, Symlinks und Spezialdateien erhalten kein rekursives
  Löschrecht. Es gibt keinen Wiederherstellungsweg; Umfang und Irreversibilität
  werden vorab bestätigt.

Die neue Schreibklasse setzt keine Dateigrößen-, Baumgesamtgrößen-, Auswahl- oder
Tiefengrenzen aus der alten Leseklasse voraus. Dateioffsets verwenden den vollen
`u64`-Protokollbereich, Daten laufen in begrenzten Paketen. Verzeichnislistings
bleiben aus RAM-/Parsergründen auf 100000 Einträge, 16 MiB Namen und eine begrenzte
Seitenzahl je Verzeichnis begrenzt; einzelne Requests besitzen ihre Transportfrist.
Der Löschscan verwendet einen expliziten Stack; Planrecords und ausstehender
Scanbestand bleiben auf den bestehenden 64-MiB-Rahmen des Queuejournals begrenzt,
unabhängig von der Größe der ausgewählten Dateien. Ein solches Ressourcenproblem
ist kein erfundener Vollständigkeitsbeleg. Auch die interaktive
Download-/Snapshotklasse besitzt keine festen Datei-, Auswahl-, Baumgesamtgrößen-
oder Tiefengrenzen mehr; ihr [Lesevertrag](sftp-contents.md) beschreibt die
Streamingprüfung und die verbleibenden Ressourcenbedingungen.
Beim SFTP-Ziel werden Inhalte und Ordnerstruktur übertragen; Dateizeit, Rechte,
Owner, ACL, xattrs, Resource Forks, Sparse- und Hardlinkstruktur bleiben sichtbar
ausgelassen.

## Abbruch, Antwortverlust und Recovery

Mutationen werden nach Verbindungsverlust nie blind wiederholt. Ein Fehler nach
möglicher Wirkung wird `outcome_unclear` samt betroffenen Pfaden. Bereits
abgeschlossene Teilwirkungen bleiben im Fortschritt erhalten. Fehler nach einer
früheren Entfernung weisen ausdrücklich auf den bereits entfernten Umfang hin.
Ein erneutes Listing ist reine Zustandsklärung und erteilt kein Wiederholungsrecht.

Schreibströme und angelegte Ordner erhalten bei Ausführung über die Fensterqueue
vor der ersten Wirkung einen privaten lokalen Remote-Intent. Er enthält nur
Queueauftrag und mögliche Pfade, keine Handles, Secrets oder Löschfreigaben.
Nach bewiesenem Commit wird er entfernt; nach Fehler oder Prozessende bleibt er
als Prüfhinweis stehen. Tempnamen ohne weiterhin gebundenen Entfernungsbeleg werden
nicht spekulativ gelöscht. ProviderDelete-Fehleraudit und öffentlicher Fehlerpayload
erhalten dieselben Recoverypfade, auch wenn zusätzlich das Queuejournal scheitert.

## Nachweis

Die Tests in `core/tests/sftp_contents.rs` verwenden einen künstlichen
Loopback-SSH-/SFTP-Paketserver mit gepinntem Testhostkey, künstlichen Credentials
und einem ausschließlich im Speicher gehaltenen Dateibaum. Sie prüfen die
gewöhnlichen Wirkungen, Kollisionen, Antwortverlust, genau einmal gesendete
Mutationen, weiterhin gesperrte starke Fähigkeiten und die Queueintegration.
Die bestehenden isolierten OpenSSH-Gegenläufe und die Betriebssystemprüfungen
bleiben zusätzliche Prüfungen; Pakettests behaupten deren Ergebnis nicht.


Prüfstand 2026-10-20: Der integrierte Stand besteht auf macOS die vollständigen
Core-, CLI- und App-Suiten sowie die UI-Suite. Auf Linux bestehen Core-, CLI- und
App-Suiten ebenfalls; der bestehende XDG-Test mit festem Mountroot `/` wurde als
unverändertes Testbinary direkt auf dem Linux-Host geprüft, die übrigen Fälle in
einer isolierten Kopie auf dessen eingebundenem Dateisystem. Das Overlay-Dateisystem
des Containers bietet für die bisherigen lokalen Metadatenprüfungen nicht das
benötigte bekannte Rechtemodell und ist deshalb kein geeigneter Fixturebereich.

Echte native macOS-Abläufe gegen einen frischen künstlichen OpenSSH-Root belegen
mkdir, F5-Datei-/Baumupload einschließlich leerem Ordner, F2-Rename, bestätigtes
Ersetzen, F6 mit erst nach Zielerfolg entfernter Quelle und dauerhaftes Löschen.
Unabhängige Inhalts- und Sentinelprüfungen bestehen. Auf Linux sind mkdir,
Datei-/Baumupload, Rename, Ersetzen und F6 sowie Kollisions- und Abbruchfälle
nativ belegt. Der Linux-Löschdialog wurde erreicht; die automatisierte
Phraseneingabe schloss ihn nicht erfolgreich ab, es wurde keine Entfernung
angefordert. Dieser native Löschabschluss bleibt dort unbestätigt. Die
protokollnahen Linux-Löschtests bestehen davon unabhängig.

Antwortverlust und genau einmal gesendete Mutationen sind durch Pakettests belegt,
nicht durch diese nativen GUI-Läufe. Ein nativer Lauf im hellen Theme und im
schmalen Fenster wurde für diesen Schnitt nicht durchgeführt. Die vorhandenen
Browserfälle decken Darstellung und Konfliktdialoge zusätzlich ab.
