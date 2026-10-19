# Operations-Queue

Stand: 2026-10-18

## Ziel

Mehrere Datei- und Sync-Operationen werden sichtbar, geordnet und recoverbar
abgearbeitet. Jedes App-Fenster besitzt eine eigene Queue und
Single-Flight-Schranke; genau ein Scheduler je Fenster besitzt dessen aktives
Mutations-Permit. Scheduler verschiedener Fenster dürfen parallel laufen.

## Datenmodell

Jeder Eintrag besitzt eine zufällige `operation_id`, Typ, Quell-/Ziel-Provider und
-Roots, unveränderliche Nutzeroptionen, Erstellzeit sowie einen Zustand:

`Pending → Validating → Running → Completed | Failed | Cancelled | Interrupted`.
Plan- oder Konfliktdrift führt vor `Running` nach `AwaitingConfirmation`.

Pläne und Permits werden niemals über einen Neustart hinweg als gültig betrachtet.
Nach Recovery wird ein Eintrag erneut gescannt und benötigt bei jeder materiellen
Planänderung eine neue Bestätigung.

Version 23 ergänzt `LocalSync` für den normalen lokalen AddOnly-/Update-Start.
Seine bestätigte Vorbereitung bleibt ausschließlich im Fenster-RAM. Fehlt sie nach
Neustart oder Retry, wird auch bei gleichem Plan frisch vorbereitet und die Wirkung
erneut im Dialog bestätigt. Alte `Sync`-Einträge ohne Sandbox bleiben dauerhaft
Simulationen. Details: [Normaler lokaler Sync](local-sync.md).

Version 24 bindet bei interaktiven SFTP-Baumdownloads den öffentlich gezeigten
Umfang, einen Digest des beobachteten Namensraums und die Auslassungen. Geänderte
Bäume erzeugen `SourceTreeChanged` vor jeder lokalen Wirkung und benötigen eine
neue exakte Bestätigung. Alte Einträge ohne Baumbeleg bleiben lesbar, können einen
neuen Baum jedoch nicht ungefragt ausführen. Handles und Sitzungen bleiben im RAM.
Siehe [SFTP-Inhalte](sftp-contents.md).

Version 27 ergänzt reine `FtpsRecovery`-Einträge für dauerhaft erhaltene
Vorbereitungsintents, die vor dem Enqueue entstanden. Sie sind nur unterbrochen oder
manuell geschlossen darstellbar und besitzen weder Enqueue- noch Retry-/Executorweg.
Die übrigen FTPS-Intents binden den bereits aktiven Queueeintrag. Ein unbekannter
STOU-Name und bekannte Container-/Wirkungsnamen bleiben strukturierte Prüfhinweise.
Lokale Recovery greift nicht auf den Server zu. Details: [FTPS-Intents](ftps-stou-transport.md#wirkungsbezogene-vorbereitung-und-dauerhafte-intents).

FTP-/FTPS-Dateivorbereitung hält den nicht serialisierbaren Abbruchtoken des
verursachenden Fensters auch im privaten Probenowner. Ein sauberer Abbruch während
`Validating` wird ohne Nutzerwirkung terminal als `Cancelled` gespeichert; dafür
verwendet der bestehende Zustandsvertrag den Übergang über `Running`. Er erzeugt
keine bestätigte Ausführungsklasse. Unklare Probe-/Cleanupausgänge und vorhandene
Recoverypfade dürfen niemals zu diesem sauberen Abbruch abgeschwächt werden.
Die Vorbereitung vor Enqueue erzeugt bei sauberem Abbruch keinen Queueeintrag.
Journalformat27 und seine manuelle Recovery bleiben unverändert.

## Scheduler und Persistenz

- Der gehaltene Konfigurationsordner bindet Prozesslock, Journal, Audit und
  Intentindizes. Ein ausgetauschter Ordner stoppt Writes und Readbacks; ein
  Ersatzordner wird nicht als neue Queue akzeptiert.
- Jede Fensterqueue wird atomar als versionsgebundenes eigenes Journal im
  App-Datenordner gespeichert. `main` behält `operations-queue.json`, Nebenfenster
  verwenden `operations-queue-window-….json`.
- Ein nicht blockierendes exklusives `flock` auf einer separaten Lockdatei verhindert,
  dass eine zweite App-/CLI-Instanz irgendeine Queue dieses Konfigurationsordners
  gleichzeitig besitzt. Die Fensterqueues desselben Prozesses teilen den
  `OperationQueueProcessLock`, nicht ihre Journale oder Worker. Erst der letzte
  Fensterbesitzer gibt den Lock ausdrücklich frei, auch wenn ein gestartetes Kind
  noch einen geerbten FD hält. Ein geerbtes Besitzerobjekt darf den Elternlock
  dagegen nicht lösen.
- Der aktive Eintrag wird vor der ersten Mutation dauerhaft als `Running` markiert.
  Meldet der atomare Journalwrite erst nach dem Rename einen Fehler, genügt der
  sichtbare neue Stand nicht: Datei und gehaltener Elternordner müssen erfolgreich
  synchronisiert und der Stand nochmals zurückgelesen werden. Scheitert dies,
  sperrt `outcome_unclear` weitere Journaländerungen bis zum Neustart mit Recovery.
- Nach Prozessabbruch wird `Running` zu `Interrupted`; vorhandene Quarantäne- und
  Papierkorb-Journale werden zuerst recovered.
- Umordnen und Entfernen ist nur für `Pending` erlaubt.
  Ein Umordnen darf keinen laufenden, unterbrochenen oder auf Bestätigung wartenden
  Eintrag überholen. Terminale Historie zwischen zwei Pending-Einträgen ist zulässig.
- Pause bedeutet: keine neue Operation starten. Ein laufender Kopierstrom kann nur
  sauber abgebrochen, nicht mit offenen Handles serialisiert werden.
- Ein Fehler stoppt standardmäßig nur den betroffenen Eintrag. Die UI bietet explizit
  „Queue anhalten“ als Option.
- Ein bidirektionaler Fehler nach Laufbeginn ist die Ausnahme: Ein möglicherweise
  wirksames Teilcommit wird als nicht-terminales `Interrupted` persistiert und
  blockiert die Reihenfolge bis zum bestätigten Resume oder Rollback.
- Fehler tragen neben dem Anzeigetext einen stabilen `error_code`. Insbesondere
  bleibt `outcome_unclear` über App-Neustarts erhalten: Die Queue bietet keinen
  Retry an, sondern verlangt die bestätigte manuelle Prüfung von Zielzustand und
  Recoverypfaden. Erst `acknowledge_unclear_outcome` schließt den Eintrag ohne
  erneute Mutation als `Cancelled`.

Die Verbindungsauswahl für generischen Provider-Sync und Dateioperationen teilt
sich denselben Rootvalidator. Identische Konfiguration und derselbe Servernamespace
reichen für die gemeinsame Sitzung nicht aus: Die beobachtete kanonische
FTP-/FTPS-Profilroot-Abbildung muss ebenfalls übereinstimmen. Andernfalls bleibt
die unabhängig verbundene Zielsitzung erhalten, auch vor Capability-Proben.

## Aufbewahrung von Fensterjournalen und Schreibresten

Die Beförderung eines gespeicherten Nebenfensters zu `main` benennt nur den
Sitzungsstand um. Sein bisheriges Queuejournal bleibt unter dem alten Label
bestehen. Auch `Completed` und `Cancelled` können weiterhin strukturierte
Recoverypfade oder erhaltene Sicherungen tragen; ein terminaler Zustand erteilt
keine Löschfreigabe. Neue Fenster verwenden deshalb kein bereits belegtes
Journallabel.

Die gemeinsame Prozesssperre verhindert konkurrierende App-/CLI-Queuebesitzer.
Sie beweist weder die Herkunft alter Temp-Dateien noch deren Entbehrlichkeit.
Ein Dateipräfix, das Alter oder ein parsebares Journal genügen deshalb nicht für
automatische Löschung oder Verschiebung. Eigene laufende Writes dürfen nur mit
dem bei exklusiver Anlage gehaltenen Besitzbeleg bereinigen. Nach einem Crash
bleiben unbelegte Reste erhalten. Diese bewusste Aufbewahrung hat keine
zeitgesteuerte Bereinigung; [Prüfung und Grenze](release-validation.md).

## Konflikte und Bedienung

Navigation, Tabs, Auswahl, Informationen und weitere Datei-Aufträge bleiben während
der Ausführung erreichbar. Lauf-ID und ursprüngliche Endpunkte binden Fortschritt
und Refresh unabhängig von den sichtbaren Panes. Remote-Dateiabsichten werden bei
belegter Ausführung ohne parallele Providerprobe persistiert und am Queuekopf frisch
geprüft. Der genaue Bedien- und Clipboardvertrag steht unter
[Hintergrundarbeit](background-file-work.md).

Konfliktentscheidungen gehören zum einzelnen Queue-Eintrag. Ein globales „alle
ersetzen“ darf nicht still auf andere Roots oder Provider übergreifen. Fortschritt
zeigt aktiven Eintrag, Gesamtanzahl, logische und tatsächlich übertragene Bytes sowie
den letzten recoverbaren Zustand.
Jede neu entdeckte Plan- oder Ersetzungskonfliktlage besitzt eine aus der zufälligen
`operation_id` abgeleitete exakte Phrase. Erst deren Eingabe ersetzt den gespeicherten
Plan beziehungsweise die gespeicherte Konfliktliste; anschließend validiert der
Scheduler nochmals.

Beide behalten bindet seit Journalformat 22 die vom Backend vergebenen Aliasziele
und Quellbelege. Der Queuekopf prüft sie vor jeder Wirkung frisch; der eigentliche
Commit bleibt exklusiv. Eine späte Kollision führt zu `plan_drift` und verlangt
Neuplanung, statt den neuen Namen still zu ersetzen oder zusammenzuführen. Ältere
App-Versionen dürfen dieses Journalformat nicht als gewöhnliches Skip ausführen.
Details: [Konfliktentscheidungen](conflict-decisions.md).

Für lokale Neunamencommits ohne exklusives Rename hält `LocalFileCommit` die
vollständige Zielroot- und Mountidentität. Journalformat 22 übernimmt ältere
Einträge ohne diese Freigabe; sie benötigen auf einem schwächeren Volume eine
neue Bestätigung. Zustimmung ändert nur den konkreten Queue-Eintrag, bei
`ProviderFile` auch dessen Erwartung für neue Namen. Ein anderer Root oder Mount
kann die Freigabe nicht erben. Der ausführende VFS hält den Rootdeskriptor und
prüft den sichtbaren Root vor dem Commit erneut. Permits und Handles werden
weiterhin nicht persistiert. Dies gilt auch für lokale Downloadziele.

Die CLI nennt die exakte Phrase als `local_copy_new_confirmation` im
`file_preview` und verlangt sie über stdin, bevor der gebundene Eintrag entsteht.
Eine falsche Phrase lässt das Ziel unverändert. GUI und CLI erklären ausdrücklich,
dass ein gleichzeitig nach dem letzten Listing angelegtes Ziel im verbleibenden
kurzen Fenster ersetzt werden kann. Ein unklarer Ausgang erhält keinen Retry.

Eine Rückfrage muss auch ablehnbar sein. `discard_attention` schließt den Eintrag
ausdrücklich als `Cancelled` ab und gibt die Reihenfolge frei; das Popover bietet
den Weg als „Verwerfen“ neben „Plan bestätigen“ an. Ohne ihn führte aus
`AwaitingConfirmation` einzig die Zustimmung heraus — `remove_pending`,
`move_pending`, `retry`, `cancel` und `fail` verlangen alle einen anderen
Ausgangszustand, und ein Neustart ließ den Eintrag samt aller dahinter wartenden
Operationen stehen. Sicher ist das Verwerfen, weil `AwaitingConfirmation`
ausschließlich aus `Validating` entsteht, also vor `mark_running` und damit vor der
ersten Mutation. Zwei Lagen halten Zustand außerhalb des Journals und bleiben
ausgenommen: eine wartende Remote-Vorbereitung (Permit und Verbindung im
Backend-RAM, Abbau über `discard_remote_preparation`) und ein bidirektionaler
Eintrag mit vorhandenem Laufjournal.

Die jeweilige Queue ist über einen Zähler im dauerhaft sichtbaren Fensterkopf erreichbar.
Das Popover hält den einzelnen `Running`-Eintrag oberhalb der scrollenden Restliste
angeheftet; Pause und Stop-on-error bleiben auch während der Mutationssperre
bedienbar. Ein nativer Beenden-Versuch wird abgefangen, sobald ein anderer Zustand
als `Completed` oder `Cancelled` vorliegt. Der Frontend-Hinweis erklärt, dass das
Journal erhalten bleibt und ein laufender Eintrag beim nächsten Start als
`Interrupted` neu geprüft werden muss. Nur die ausdrückliche Bestätigung ruft den
getrennten programmatischen Exit auf.

## Sicherheitsregeln

- Kein gespeicherter Queue-Eintrag umgeht Root-, Sandbox-, Provider- oder Planprüfung.
- Jede Vorbereitung, die eigene Restore-, Capability- oder Quarantäne-Proben
  anlegt, öffnet zuerst den app-weiten Queue-Prozessbesitz und hält die
  fenstereigene Single-Flight-Schranke bis nach Probe, vollständigem Scan und Ablage der
  nicht serialisierbaren Vorbereitung. Das gilt für lokalen Realmodus, alle
  Einweg-Provider und bidirektionale Vorbereitung einschließlich Resume/Rollback.
- Secrets, geöffnete Handles und kurzlebige Sitzungs-Tokens werden nicht persistiert.
- Abbruch entfernt nur eigene uncommittete Temp-Dateien.
- Queue-Wiederaufnahme ersetzt niemals ein inzwischen entstandenes Ziel ohne neue
  Konfliktprüfung.
- Der lokale Realmodus ist absichtlich kein persistierter Queue-Typ: sein Permit hält
  Deskriptoren, ist einmalig und höchstens zehn Minuten gültig. Vor seiner Vorbereitung
  muss die Queue abgearbeitet sein.

## Implementierter Stand

Copy, Move und lokaler Sandbox-/Dry-Run-Sync verwenden seit v0.1.24 den gemeinsamen
GUI-freien Executor in `core/src/queue.rs`. Tauri besitzt je Fenster Worker,
Event-Drosselung und Fensterkopf-UI. Fortschritt wird gezielt nur an das zugehörige
Fenster gesendet und ins eigene atomare Journal geschrieben; ein Persistenzfehler
setzt sofort dessen Abbruchsignal. Die gemeinsame Papierkorb-Recovery läuft vor dem
ersten Worker pro Prozess genau einmal sauber durch.

Journalversion 11 enthält zusätzlich generische `ProviderFile`- und
`ProviderDelete`-Einträge. Sie binden beide öffentlichen Endpunktbeschreibungen,
Mehrfachauswahl, Zielroot, Konfliktliste beziehungsweise den vollständigen
Löschplandigest. Vor jedem Claim werden Profile und Schlüsselbundreferenzen frisch
aufgelöst, Listings und Fähigkeiten neu geprüft. Ein abgesicherter Ersetzungsweg
verlangt eine identitätsgebundene Quarantäne und einen anschließenden No-Replace-Commit.
Die derzeitigen Remote-Provider bieten diese Ersetzungsquarantäne nicht an. FTPS kann stattdessen nach eigener Probe ausdrücklich bestätigte reguläre
Dateien als Best Effort dauerhaft ersetzen; die Altversion ist dann nicht
wiederherstellbar. Die Queue bindet die bestätigte Klasse und Einschränkungen.
Der direkte FTPS-Move benötigt private No-Replace-Belege für alle tatsächlichen
Top-Level-Typen am Zielroot. Nur derselbe aktive Queueeintrag darf sie aus seiner
Zielsicht in die gemeinsam gebundene Quellsicht übernehmen; andere Fenster oder
Vorschau-IDs erhalten dadurch keine Schreibfreigabe. Der reine Rename-Pfad
erlaubt nach Fehlern keinen stillen Copy-/Remove-Rückfall. Fehlt die Garantie
schon bei der Vorbereitung, bleiben Kopie und Quellentfernung getrennt an ihre
Best-Effort-Erwartungen gebunden. Queueformat 27 bleibt unverändert.
Remote-Löschläufe wiederholen den vollständigen Scan und stoppen bei jeder Abweichung
vor der ersten Mutation. Die [Fähigkeitsmatrix](function-audit.md) trennt diese
Wirkungen von normalem Kopieren und Sync.

Journalversion 16 ergänzt jeden GUI-Remoteeintrag um die geheimnisfreien
Profilbindungen aus Protokoll, Profil-ID und Revision. Vor einer neuen Vorbereitung,
vor dem Permit und unmittelbar vor der Ausführung muss diese Revision noch aktuell
sein. Ändert sich ein Profil bei wartender Remote-Bestätigung, entfernt das Backend
das nicht serialisierbare Permit und setzt genau diesen Eintrag atomar nach `Pending`
zurück; fremde Fensterjournale werden weder geleert noch verschmolzen.

Journalversion 18 ergänzt den maschinenlesbaren Fehlercode. Bei der Migration
werden WebDAV-Löschpläne aus Version 17, deren Server-Header-Digest noch über der
leeren Eingabe gebildet wurde, fail-closed stillgelegt: sicher ungestartete Einträge
werden `Failed`, mögliche frühere Teilwirkungen `Interrupted`. Das übrige
Fensterjournal bleibt lesbar.

Journalversion 20 sperrt persistierte FTPS-/WebDAV-Löschpläne aus Version 18,
nachdem beide Protokolle ihre Serveridentitäts-Digests auf längengebundene Rohdaten
umgestellt haben. Sicher ungestartete Einträge verlangen eine neue Vorschau;
Validating-, Running-, Failed- und Interrupted-Einträge bleiben als
`outcome_unclear` für die manuelle Zustandsprüfung erhalten und werden nie blind
wiederholt. Der typisierte Code `replan_required` sperrt den normalen Retry und
erlaubt ausschließlich das atomare Verwerfen des sicher nicht gestarteten Altplans;
auch bereits durch Version 19 migrierte Journale werden darauf angehoben. Die älteren
WebDAV-v15/v17-Sondermigrationen behalten ihren eigenen quittierbaren Fehlerzustand.
Auch ein wieder auf `Pending` gesetzter Auftrag gilt nach einem früheren Claim
nicht als sicher ungestartet. Versuchszahl und vorhandene Teilbilanz bleiben
erhalten; bereits falsch stillgelegte Altstände werden beim Öffnen ebenfalls
auf die notwendige manuelle Prüfung angehoben.

Das Journal migriert ältere Endpunkt- und Konfliktformate fail-closed. Zusätzlich
liegt daneben ein append-only Audit; `main` verwendet weiterhin
`operations-audit.jsonl`, Nebenfenster `operations-audit-window-….jsonl`. Auditversion
4 enthält Zeitstempel, Operation-ID, generische Wirkung (z. B. `provider_copy`),
Status, Quell- und Zielprovider, die nach der frischen Capability-Prüfung tatsächlich
bestätigte Ausführungsklasse (`assured` oder `best_effort`) sowie Datei-/Bytezähler.
Bei einem Löschlauf ist `source_provider` `null`; `target_provider` benennt den
betroffenen Provider. Zusätzlich stehen bestätigte und beobachtete Best-Effort-
Wirkungen sowie strukturierte Recoverypfade für Quarantäneläufe, Tempbestände und
erhaltene Sicherungen darin. Normale Inhaltsnamen, Profile, Secretwerte und
credentialed URLs bleiben aus dem Audit heraus.

Journalversion 6 ergänzte bidirektionale Einträge. Persistiert werden nur Paar-ID und
-name, lokale Roots, Baseline-Pfad, Konfliktentscheidungen sowie bestätigte Baseline-/
Plan-Digests. Vollständige Pläne, Phrasen, Handles und Permits bleiben im RAM. Ein
frischer Doppel-Scan fordert zunächst bei Digestdrift eine eigene Planphrase und
danach immer eine neue zufällige Laufphrase. Resume und Rollback beziehen ihre Roots
ausschließlich aus dem Backend-Queue-Eintrag; erfolgreiche Recovery schließt exakt
diesen Eintrag als `Completed` beziehungsweise `Cancelled`.

13 Queue-Kernfälle sind auf macOS und Linux-Testhost grün: Reihenfolge, Pending-only-
Umordnung, Abbruch, Crash-Recovery, Plan-Drift, späte Ersetzungskollision, beschädigtes
Journal, verwaiste Temp-Datei, Stop-on-error sowie konkurrierende Thread- und
Prozessstarter einschließlich echter lokaler Copy-Ausführung. Vier zusätzliche
App-Crate-Gegenfälle sind auf macOS grün. Die Mock-UI wurde mit
Pause, zwei Einträgen, Umordnung und falscher/exakter Driftphrase ohne Browserfehler
interaktiv und visuell geprüft.

Vier zusätzliche Kernfälle decken bidirektionale Persistenz, Digestdrift, unsichere
Baseline-Pfade und die blockierende Recovery ab. Direkter Lauf, Queue-Enqueue, Resume
und Rollback wurden in
der Mock-UI ohne Browserfehler interaktiv geprüft.

## Abnahmekriterien

Tests decken mindestens Reihenfolge, Umordnung, Abbruch, Prozessabsturz, Plan-Drift,
späte Namenskollision, fehlendes Secret, Provider-Reconnect, beschädigtes Journal und
zwei konkurrierende Startversuche ab. macOS und Linux müssen dieselben Kernfälle
headless bestehen.

Die lokalen Kriterien sind erfüllt. `fehlendes Secret` und `Provider-Reconnect` sind
definitionsgemäß nicht lokal simulierbar und bleiben verbindliche zusätzliche
Abnahmekriterien jedes Remote-Providers.

## Additive WebDAV-Uploads

Lokales WebDAV-`ProviderFile` mit Copy und `overwrite=true` erhält keine neue
Freigabe: Der Executor lehnt vor Secret-/Netzwerkzugriff mit `unsupported` ab,
auch nach Retry oder Reopen. Die gemeinsame Bindung von Temp-, Altziel- und
sämtlichen Elternversionen ist mit Standard-MOVE nicht ausdrückbar.
[Protokollgrenze](webdav-replace-boundary.md). Das Journalformat bleibt 26.

Journalversion 26 bindet die private interaktive Datei-/Baumuploadklasse über die vorhandene
`ProviderFile`-Operation. Offene lokale WebDAV-Kopien verlieren nach Reopen die
alte Best-Effort-Commit-Erwartung und verlangen frische Zustimmung. Remote-
Dateiintents liegen vor PUT, Ordnerintents vor MKCOL dauerhaft im privaten
Fenster-Queueordner; sie speichern nur Operation und mögliche Temp-/Zielpfade.
Der Baumdigest bindet auch lokale Inhalte, Auslassungen und Elternidentitäten.
Eine beim Prozessende laufende lokale WebDAV-Kopie bleibt selbst zwischen bereits
abgeschlossenen Additionen unklar und erhält die möglichen Top-Level-Ziele.
Recovery erhält mögliche Wirkungen als
`outcome_unclear` und sperrt Retry. Einzelheiten: [WebDAV-Uploads](webdav-upload.md).

## WebDAV-Mutationsgrenze

Seit AP13R erteilen allgemeine WebDAV-Datei-, Lösch-, Sync- und Quarantäneabfragen
keine Schreibfähigkeit und erzeugen keine künstliche Probe. Alte Queueabsichten
können diese Grenze nicht übergehen. Gewöhnliche lokale additive Uploads und
bestätigte Namespaceaktionen verwenden weiterhin ihre privaten Best-Effort-Wege.
Genau eine unmittelbare Elternbedingung ergänzt die Request-Version; Vorfahren
werden nur vor/nach der Wirkung geprüft. Unklare Ausgänge bleiben ohne Retry
fenstergebunden erhalten. [Vertrag](webdav-mutation-boundary.md).


### Vollständig übersprungene FTPS-Dateiaufträge

Der Provider-Executor prüft die erwartete vollständige Konfliktmenge vor
mutierenden Teilproben. Der gemeinsame Core wiederholt diese Bindung unmittelbar
vor seiner Wirkungsentscheidung. Die vollständige rekursive Wirkungsbilanz
unterscheidet ausgelassene Blätter, neue Ordner, Dateiinhalt und tatsächlich
vollständig übertragbare Move-Quellen. Vollständig ausgeschlossene Bäume starten
keine Datei-, Rename- oder Quellentfernungsprobe. Sind alle ausgeschlossen,
ruft der Executor keine VFS-Mutation auf und schließt mit dem Skip-Zähler über das
bestehende persistente Dateiabschlussverfahren ab. Der Zähler zählt die wirklich
übersprungenen Blätter beziehungsweise Typkonflikte. Bei einer Teilkopie mit Skip
bleibt der gesamte betroffene Top-Level-Quellbaum erhalten; andere vollständig
kopierte Auswahlbäume behalten ihre benötigte Quellentfernungsbindung. Die vor
den Proben geprüften Skip-Pfade bleiben bis in die einzelne Kopierwirkung gebunden.
Best-Effort- oder Schreibflags
werden dafür weder benötigt noch freigegeben. Ein später verschwundenes Ziel
begründet keinen neuen Kopierversuch. Neue oder fehlende Konflikte vor dieser
Entscheidung verlangen eine neue Bindung; Stat-/Providerfehler bleiben Fehler.
Cancel und unklare bereits begonnene Proben behalten ihren bisherigen Vertrag.
Journalformat27 und Recovery ohne Providerverbindung bleiben unverändert.
