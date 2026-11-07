# SFTP-Inhalte, Öffnen und Remote-Bearbeitung

Stand: 2026-10-24.

## Interaktive Leseklasse

SFTP verwendet eine private interaktive Best-Effort-Lesesicht für reguläre Dateien
und Verzeichnisbäume. Das strikte VFS für Sync, Mirror und Quarantäne erhält daraus
keine allgemeine Schreibfreigabe. Gewöhnliche Dateiaktionen besitzen ihren eigenen
[Schreibvertrag](sftp-writes.md).

Dateigröße, Auswahlzahl, Baumtiefe und Gesamtbytes sind keine festen Produktgrenzen
mehr. Die Baumvorbereitung arbeitet iterativ und besitzt keine pauschale
60-Sekunden-Frist. Sie liest READDIR bis zum tatsächlichen EOF; Abbruch wird zwischen
Requests und Einträgen geprüft. Einzelne Transportrequests, Parser und belegter
Speicher bleiben begrenzt. Ein Ressourcenfehler wird sichtbar gemeldet und niemals
als vollständiges Listing ausgegeben. Symlinks, Spezialdateien und unbrauchbare
Attribute werden mit Pfad und Grund bilanziert; unlesbare oder ungültige Listings
werden nicht als leere Ordner behandelt.

Die verbliebene 64-Komponenten-Sperre für Inhaltspfade ist ebenfalls entfernt.
Damit bleiben direkte Dateien und ausgewählte Teilbäume in einer bereits tief
navigierten Pane lesbar. Einzelne virtuelle Inhaltspfade dürfen weiterhin
höchstens 4096 Bytes umfassen. Lexikalische Prüfung, direkte Auswahlbindung und
die vollständigen beobachteten Elternketten gelten unabhängig von der Tiefe.

Seit 2026-10-23 lässt der additive Baumdownload außerdem sämtliche Mitglieder
möglicherweise am lokalen Ziel gleichwertiger Case-/NFC-Namensgruppen pfadgenau
aus. Er wählt keinen Gewinner und überträgt unabhängige Geschwister weiterhin.
Ein ausgelassener Ordner zählt einmal für seinen gesamten unangetasteten Teilbaum.
Die vollständigen beobachteten Namen und READDIR-Attribute einschließlich der
Auslassungen bleiben im Elternbeleg und Bestätigungsdigest; deren Änderung stoppt
den alten Lauf vor einer weiteren lokalen Wirkung. Inhalte ausgelassener Einträge
werden nicht geöffnet. Die Größenbeobachtung zählt unterschiedliche Servernamen
weiterhin vollständig und übernimmt diese Zielannahme nicht.

Bereits betretene Eltern mit rekursiven Auslassungen werden global und
dedupliziert an Dateiöffnung, Streamabschluss und Commitvorbereitung erneut
vollständig verglichen. So stoppt auch eine Änderung ausschließlich am
ausgelassenen Kind, deren Elternattribute gleich bleiben, den nächsten Commit.
Diese Vergleiche erzeugen zusätzliche Listings abhängig von betroffenen Eltern
und Prüfgrenzen; ohne Auslassungen entfallen sie. Direkt ausgelassene
Auswahlordner werden dafür weiterhin nicht betreten.

Auch direkte Inhaltsauswahlen lassen sämtliche Mitglieder möglicherweise
am lokalen Ziel gleichwertiger Case-/NFC-Namensgruppen aus. Unabhängige Dateien
und Ordner bleiben übertragbar. Ausgelassene direkte Ordner werden weder per
READDIR untersucht noch inhaltlich geöffnet; ihr Teilbaum zählt einmal. Dafür
wird kein vollständiges Listing der nicht ausgewählten Pane-Geschwister verlangt.
Jedes direkt ausgelassene Mitglied bleibt mit LSTAT-Attributen, REALPATH und
Elternkette an den Bestätigungsdigest gebunden. Getter, Dateiöffnung,
Streamabschluss, Commitvorbereitung und weitere Ordnerwirkungen prüfen diese
Belege frisch. Änderungen halten den alten Lauf vor der nächsten Wirkung an;
eine nach Neustart veränderte Auswahl verlangt eine neue Umfangsbestätigung.
Die Prüfung erfolgt an diesen Übergängen, nicht für jeden gelesenen Byteblock.

Diese Ausnahme gilt ausschließlich für Inhaltskopien. Die Metadatenbeobachtung
zählt die unterschiedlichen Servernamen weiterhin unabhängig von der lokalen
Zielsemantik. Tatsächlich doppelte Auswahlpfade oder
READDIR-Namen, unsichere Pfadsegmente und fehlerhafte oder unvollständige Antworten
bleiben Absagen. Nicht als UTF-8
isolierbare Protokollnamen behalten die Parsergrenze des Anzeigelistings.
Strikter Sync, Move und Quellentfernung erhalten dadurch keine neue Freigabe.

Die Sicht bindet Hostkey, Sitzungsgeneration, gewählten Pane-Root und beobachtbare
Pfadattribute. LSTAT und REALPATH prüfen Eltern und letzten Namen; OPEN ist lesend,
FSTAT muss zum Beleg passen. Der gehaltene Handle liefert einen begrenzten Stream;
Größe und Attribute werden am Ende erneut geprüft. Normales Öffnen und Vergleichen
liest jeden Inhalt einmal remote und bildet dabei BLAKE3. Ein unabhängiger lokaler
Hash prüft die gespeicherten Bytes. Zusätzlicher vollständiger Remote-Readback ist
über das Prüfprofil `SnapshotVerification::FullReadback` wählbar.

Diese Prüfungen sind kein atomarer Dateisystem-Snapshot. Ein gleichzeitiger
Pfadtausch oder eine gleich große Änderung mit unverändert erscheinenden Attributen
bleibt möglich. Zusätzliche Vollverifikation erkennt weitere Änderungen, erzeugt
aber ebenfalls keine atomare Rootbindung. Nach Antwortverlust gibt es keinen
mutierenden Blind-Retry; ein neuer Leseversuch bindet die Verbindung frisch.

## Öffnen, passives Ansehen und Bearbeiten

„Öffnen“ übergibt eine lokale Momentaufnahme im Originalformat an die System-App.
„Passiv ansehen“ verwendet für aktive oder unbekannte Formate die passive
Textbehandlung. Externe Anwendungen laufen mit ihren eigenen Rechten; Doppeldecker
behauptet keine Sandbox dieses Betrachters. Remote-Namen werden niemals zu
Shellbefehlen. Snapshotdateien bleiben privat (0600), ihre Ordner privat (0700).
Normale Downloads übertragen darstellbare Dateirechte entsprechend dem gewählten
Profil; die privaten Cacherechte werden nicht als Downloadprofil missverstanden.

„Bearbeiten und zurückspeichern“ öffnet eine private Arbeitskopie mit dem
Originalnamen. Die Seitenleistenaktion „Remote-Bearbeitung“ zeigt die geöffneten
Dateien und die konkrete Ersetzungswirkung. Nach dem Speichern in der externen
Anwendung reiht „Zurückspeichern“ den normalen Provider-Dateiauftrag ein.
Ein frischer Vergleich mit dem ursprünglichen Serverinhalt verhindert das stille
Überschreiben einer zwischenzeitlich geänderten Remoteversion. Das Backend bindet
Originalbeleg, Profilrevision, konkrete Ersetzungsklasse und lokale Uploadkopie;
spätere Editoränderungen verändern keinen bereits bestätigten Rückupload.

Die Bearbeitungsliste gilt bis zum Schließen ihres Fensters oder App-Neustart.
Lokale Arbeitskopien bleiben im privaten Cache; es gibt keinen automatischen
Dateiwächter und keinen automatischen Upload bei jeder Editoränderung. Nach einem
Konflikt bleibt die Arbeitskopie erhalten und der Nutzer kann das neue Original
öffnen und die Fassungen vergleichen. Ein fehlgeschlagener oder unklarer Upload
wird nicht blind wiederholt.

## Übertragung und Recovery

Kopien nutzen dieselben frischen Quellbelege und normalen Zielverträge wie andere
Dateiaktionen. Vorhandene Ziele erhalten Überspringen, zulässiges Ersetzen oder
backendseitig vorbereitete neue Namen. Quellentfernung bleibt eine gesonderte,
nach vollständig geprüftem Ziel bestätigte Wirkung.

Snapshotcache und Manifest führen nur eigene Dateien. Abbruch oder Drift entfernt
nur die eigene unvollständige Momentaufnahme. Das sieben Tage betragende
Aufbewahrungsziel erlaubt ausschließlich die Bereinigung manifestierter,
unveränderter eigener Snapshots; fremde Dateien, bearbeitete Arbeitskopien und
unmanifestierte Crashreste bleiben erhalten. Fortschritt und Abbruch gehören zum
verursachenden Fenster; andere Fenster behalten ihre eigene Queue.

Die aktuellen Prüfungen und verbliebenen Grenzen stehen beim
[Audit-Umsetzungsstand](usability-audit-2026-10-20.md#umsetzungsstand).
Die folgenden älteren Belege dokumentieren ihren damaligen Stand einschließlich
inzwischen aufgehobener Grenzen.

## Historische Prüfbelege vor dem Abbau der Produktgrenzen

Künstliche SSH-/SFTP-Paketgegenläufe prüfen leere und große Inhalte, Namen,
Typ-/Größengrenzen, Root-/Elterntausch, Inhaltsdrift bei gleicher Größe/Zeit,
abgeschnittene Streams, Readbackfehler, Antwortverlust, Sitzungswechsel, Abbruch,
späte lokale Kollision und Queue-Reopen. Ein deterministischer Metadatenabbruch
stoppt vor dem nächsten Elternrequest und vor jedem OPEN. Ein echter Kindprozess endet nach
Teiltransfer; der Reopen findet seinen privaten Tempintent und startet ihn nicht.
Der isolierte OpenSSH-Gegenlauf prüft zusätzlich den bisherigen strikten
Negativvertrag und eine erfolgreiche Momentaufnahme.

Die rekursive additive Auswahl ist als eigener begrenzter Best-Effort-Schnitt
umgesetzt. SFTP-mkdir, Rename, Upload, Ersetzen und Move sind inzwischen im
[Schreibvertrag](sftp-writes.md) umgesetzt; strikter Sync bleibt gesperrt.
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
Die damalige, inzwischen aufgehobene Tiefenabsage ohne Zielwirkung ist zusätzlich
vor einer später fehlgeschlagenen Hilfserwartung geprüft. Frühere Hilfsläufe
erwarteten falsche Fokus-, Konflikt- oder Recoveryzustände und zählen nicht als
bestandene Blöcke.
Die Quellhashes blieben bis auf die ausdrücklich injizierte neue Datei gleich;
kein SFTP-Mutationsrequest wurde gesendet.
