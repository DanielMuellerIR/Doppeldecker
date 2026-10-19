# Sicherer Papierkorb-Uebergang

Stand: 2026-10-07

Status: Der Zielvertrag für private Claims ist im gemeinsamen Kern und in den
nativen macOS-/XDG-Schreibadaptern implementiert. Neue Läufe verwenden Journal v2;
die gemeinsame native macOS-GUI-Sandbox-Abnahme ist ebenfalls belegt. Die folgenden v1-Abschnitte
bewahren den historischen Ablauf und seinen Sicherheitsbefund als Beleg; dieser
Schreibweg wird ausschließlich für Regressionstests übersetzt. Die produktive
Altjournal-Recovery bleibt konservativ gebunden.

## Ziel

Der Papierkorb-Befehl bindet Quelle und Ziel descriptor-relativ und verlässt eine
aktive Test-Sandbox nicht. Ein Erfolg wird nur für die am Papierkorbziel erneut
geprüfte Identität gemeldet.

Historische Grenze des v1-Schreibwegs: Der Claimname liegt bis zum letzten Rename im Quellordner. Ein anderer
Schreibberechtigter kann ihn nach seiner Prüfung austauschen. Der finale Readback
erkennt einen falschen oder nicht prüfbaren Zieleintrag, verhindert die bereits
vollzogene Verschiebung aber nicht. Doppeldecker meldet dann einen unklaren Ausgang,
erhält Journal sowie Ziel-/Claimpfade und versucht keine automatische Rückbenennung
oder Entfernung. Ein privater Claim-Namensraum mit geklärtem macOS-/XDG-Lifecycle
schließt dieses Rennen im neuen v2-Schreibweg. Der finale Readback bleibt zusätzlich erforderlich.

Die zuvor verwendete `trash`-Crate 5.2.6 konnte diese Zusage nicht allein geben:
Sie kanonisiert den Elternpfad und reicht danach wieder einen Pfadstring an Finder,
`NSFileManager` oder die XDG-Implementierung weiter. Zwischen Pruefung und Zugriff
bleibt damit ein TOCTOU-Fenster.

## Bisherige Entscheidungen (historischer v1-Ablauf)

1. Die korrektheitskritische Logik kommt als GUI-freies Modul in `core/`. Die
   Tauri-Schicht nimmt nur Strings entgegen, ruft den Kern auf und meldet das Ergebnis.
2. Die ausgewaehlten Eintraege werden zuerst descriptor-relativ und ohne Folgen von
   Symlinks geoeffnet. Elternkomponenten verwenden weiterhin
   `openat(O_NOFOLLOW | O_DIRECTORY)`.
3. Vor dem eigentlichen Papierkorb-Verschieben beansprucht der Kern jeden Namen durch
   einen atomaren Rename auf einen kollisionsfreien, versteckten Claim-Namen im
   bereits geoeffneten Elternordner.
4. Direkt nach dem Claim wird dessen Identitaet (`st_dev`, `st_ino`, Dateityp) mit der
   zuvor erfassten Identitaet verglichen. Nur ein identischer Claim darf weiter in den
   Papierkorb. Bei Abweichung wird nichts geloescht oder in den Papierkorb verschoben.
5. Der anschliessende Transfer in den Papierkorb erfolgt mit descriptor-relativem,
   atomarem `NOREPLACE`-Rename. Es gibt keinen unsicheren Pfadstring-Fallback und kein
   rekursives Copy-and-delete-Fallback.
6. Die Zielauswahl ist plattformspezifisch, die Claim-/Identitaets-/Journal-Logik bleibt
   gemeinsam und headless testbar.
7. Kann Doppeldecker die Sicherheitszusage nicht halten, bricht der Vorgang sichtbar
   ab. Verfuegbarkeit ist hier nachrangig gegenueber dem Schutz fremder Daten.

## Ablauf des historischen Stapels (v1)

### 1. Gesamten Stapel vorpruefen

- Leere Listen, doppelte Pfade, `/` und Eltern-/Kind-Ueberlappungen ablehnen.
- Fuer jeden Eintrag den Elternordner descriptor-relativ oeffnen und den finalen Namen
  mit `fstatat(..., AT_SYMLINK_NOFOLLOW)` erfassen. Ein finaler Symlink ist selbst ein
  gueltiger Papierkorb-Eintrag; seinen Zielpfad betreten wir nie.
- Bei aktiver Sandbox den Pfad relativ zum bereits geoeffneten Sandbox-Deskriptor
  nachlaufen. Ein reiner `starts_with`- oder `canonicalize`-Vergleich genuegt nicht.
- Passendes Papierkorb-Ziel bestimmen und dessen Verzeichnisse ebenfalls ohne
  Symlink-Folgen oeffnen und auf Eigentum/Rechte pruefen.
- Fuer jeden Eintrag einen eindeutigen sichtbaren Zielnamen reservieren. Kollisionen
  erhalten einen Suffix; vorhandene Papierkorb-Inhalte werden nie ersetzt.

Wenn eine Vorpruefung scheitert, ist noch kein Quellname veraendert.

### 2. Claim journalisieren

Vor dem ersten Rename wird ein kleines Journal im App-Konfigurationsordner angelegt
und dauerhaft geschrieben. Pro Eintrag enthaelt es:

- urspruenglichen absoluten Pfad,
- erwartete Identitaet aus `st_dev`, `st_ino` und Dateityp,
- Claim-Namen,
- reservierten Papierkorb-Namen,
- Status `planned`, `claimed`, `trashed` oder `drifted`.

Vor dem finalen Transfer erhält `claimed` im bestehenden optionalen `failure`-Feld
einen Hinweis auf den noch ausstehenden Identitätsabschluss. Dieser Hinweis bleibt
auch erhalten, wenn das spätere Speichern eines Readbackfehlers scheitert. Alte
Journale ohne dieses Feld bleiben lesbar. Ein automatischer Rückweg benötigt
inzwischen zusätzlich einen heute belegten privaten Quell-Elternnamensraum.
Ein `claimed`-Eintrag mit Hinweis verlangt dagegen manuelle Prüfung und blockiert
neue Papierkorb-Aktionen bis zur Klärung.

Das Journal enthält keine Dateiinhalte. Die automatische Recovery gilt für belegte
Claims vor dem Transferhinweis und für ältere Journale ausschließlich aus einem
heute belegten privaten Quell-Elternnamensraum. Ein fehlgeschlagener finaler
Identitätsreadback bleibt durch den vorher dauerhaft geschriebenen Transferhinweis
zur manuellen Prüfung erhalten. Scheitert erst der Eltern-fsync beim Schreiben des
bereits positiv geprüften `trashed`-Abschlusses, kann dieser terminale Stand trotzdem
lesbar sein; die Recovery darf ihn dann als abgeschlossen erkennen.

### 3. Namen sicher beanspruchen

Der Kern fuehrt innerhalb des geoeffneten Quell-Elternordners aus:

1. `renameat_with(..., NOREPLACE)` vom sichtbaren Namen zum zufaelligen Claim-Namen,
2. `fstatat(..., AT_SYMLINK_NOFOLLOW)` auf dem Claim,
3. Vergleich mit der vorab erfassten Identitaet.

Stimmt die Identitaet nicht, wurde der sichtbare Name im Zeitfenster ausgetauscht. Der
fremde Claim wird atomar auf seinen Namen zurueckgesetzt, sofern dieser frei ist. Ist
eine Ruecksetzung wegen einer erneuten Kollision nicht moeglich, bleibt der Eintrag
unangetastet unter dem dokumentierten Claim-Namen und der Fehler nennt den
Wiederherstellungspfad. Er wird niemals in den Papierkorb weitergereicht.

### 4. In den Papierkorb verschieben

Nach erfolgreicher Identitaetspruefung wird der Claim descriptor-relativ und mit
`NOREPLACE` in das reservierte `files`-Verzeichnis verschoben. Der sichtbare Name im
Papierkorb entspricht dem urspruenglichen Namen beziehungsweise seinem
Kollisionssuffix; der interne Claim-Name wird nicht sichtbar. Erst der finale
Identitätsreadback erlaubt den Status `trashed`. Ein abweichender oder unbeantworteter
Readback wird als `unclear` bilanziert; das `claimed`-Journal bleibt mit `failure`
erhalten. Diese Bilanz meldet die mögliche Wirkung statt eines falschen Erfolgs.

Der Kern meldet eine strukturierte Bilanz (`trashed`, `failed`, `unclear`, `recovery_paths`). Ein
Fehler nach bereits erfolgreichen Eintraegen darf nicht als vollstaendiger Fehlschlag
erscheinen. Erfolgreich verschobene Eintraege bleiben wiederherstellbar im Papierkorb.

## Plattformadapter

### macOS

- Das Ziel liegt auf demselben Volume wie die Quelle: Benutzer-Volume in
  `$HOME/.Trash`, andere Volumes nach macOS-Konvention unter `.Trashes/<uid>`.
- Volume und Mount-Wurzel werden vom bereits geoeffneten Quell-Deskriptor abgeleitet,
  nicht durch erneutes Aufloesen des Quellpfads.
- Zielverzeichnisse werden descriptor-relativ geoeffnet. Unsichere Eigentums- oder
  Rechteverhaeltnisse fuehren zum Abbruch.
- Der Transfer nutzt den nativen exklusiven Rename (`RENAME_EXCL`, ueber
  `rustix::fs::renameat_with(..., NOREPLACE)`). Finder und `NSFileManager` erhalten
  keinen Quellpfad mehr.
- Ein direkter Rename liefert nicht auf jeder macOS-Version Finders private
  „Zuruecklegen“-Metadaten. Wiederherstellung durch Herausziehen aus dem Papierkorb
  bleibt moeglich; zusaetzlich bewahrt Doppeldecker den Ursprung im Journal auf. Eine
  spaetere eigene Wiederherstellen-Funktion darf dieses Journal verwenden.

### Linux / XDG Trash 1.0

- Auf dem Home-Dateisystem wird `$XDG_DATA_HOME/Trash` verwendet, mit Fallback auf
  `$HOME/.local/share/Trash`.
- Auf anderen Mounts wird zuerst ein gueltiges `.Trash/<uid>`, sonst `.Trash-<uid>`
  auf demselben Dateisystem verwendet.
- `files/` und `info/` werden descriptor-relativ geoeffnet beziehungsweise angelegt.
  Eigentum und vorgeschriebene Sticky-/Zugriffsrechte werden geprueft.
- Vor dem Daten-Rename entsteht die zugehoerige `.trashinfo`-Datei mit `O_EXCL`; sie
  enthaelt den prozentkodierten Ursprungspfad und eine lokale ISO-8601-Loeschzeit.
  Datei und Ordner werden vor dem Rename synchronisiert.
- Scheitert der Rename, wird nur die selbst reservierte Info-Datei entfernt. Ein
  `EXDEV` ist ein Layoutfehler und fuehrt zum Abbruch, nicht zu Copy-and-delete.

## Fehler- und Crash-Vertrag

- Vorpruefungsfehler: keine Aenderung.
- Identitätsabweichung beim ersten Claim: Transfer stoppt; Rücksetzung oder klar
  gemeldeter Claim-Wiederherstellungspfad.
- Identitätsabweichung beziehungsweise Statfehler nach dem finalen Transfer: unklarer
  Ausgang mit Ziel- und Claim-Recoverypfad; kein automatischer Rückweg oder Cleanup.
- Abbruch mit weiterhin persistiertem Transferhinweis: Die Recovery erhält das
  Journal und meldet beide Pfade zur manuellen Prüfung, auch wenn der Ursprung wieder
  existiert. Ein lesbarer `trashed`-Abschluss belegt dagegen den positiven Readback.
- Fehler nach Teilfortschritt: genaue Teilbilanz statt pauschalem Fehler.
- App-Absturz mit Claim: beim naechsten Start Recovery vor neuen Papierkorb-Aktionen.
- Nicht eingebundenes Quell-Volume: Journal bleibt erhalten und Recovery wird vertagt.
- Journal darf erst entfernt werden, wenn alle Eintraege `trashed` oder nachweislich
  zurueckgesetzt sind und der enthaltende Ordner synchronisiert wurde.
- Verschwindet ein nur als `planned` journalisierter Eintrag noch vor dem ersten Claim
  durch externe Drift, wird der Zustand dauerhaft als terminal `drifted` markiert. Da
  Doppeldecker zu diesem Zeitpunkt keinen Namen verändert hat, entsteht weder ein
  erfundener Recovery-Pfad noch eine Sperre für spätere Papierkorb-Aktionen.

## Testplan

Die Kern-Tests verwenden echte Temp-Verzeichnisse auf demselben Dateisystem und einen
injizierten Papierkorb-Aufloeser. Sie beruehren nie den echten System-Papierkorb.

Pflichtfaelle:

1. Datei, Ordner und finaler Symlink werden als jeweiliger Eintrag verschoben.
2. Vorhandener gleichnamiger Papierkorb-Inhalt wird nicht ersetzt.
3. Quell-Elternordner wird nach dem Oeffnen umbenannt und am alten Pfad durch einen
   Symlink ersetzt: Nur der descriptor-verankerte Eintrag darf betroffen sein.
4. Sichtbarer Quellname wird zwischen Vorpruefung und Claim ausgetauscht:
   Identitaetsvergleich stoppt den Papierkorb-Transfer.
5. Sandbox-Elternkomponente wird ausgetauscht: kein Zugriff ausserhalb der Sandbox.
6. Fehler vor dem ersten Claim veraendert keinen Eintrag.
7. Fehler nach einem erfolgreichen Eintrag liefert eine korrekte Teilbilanz.
8. Simulierter Absturz nach `claimed` wird bei belegtem privatem Quell-Elternordner
   zurückgesetzt; öffentliche Claims bleiben manuell erhalten.
9. Ein älteres `claimed`-Journal ohne Transferhinweis erkennt den bereits sicheren
   Abschluss. Solange ein neues Journal nur den Transferhinweis enthält, bleibt es
   bis zur manuellen Prüfung als Blocker erhalten; ein lesbares `trashed` ist terminal.
10. Linux: `.trashinfo` kodiert Sonderzeichen korrekt und verweist auf den Ursprung.
11. Unsichere Trash-Rechte, anderes Dateisystem und `EXDEV` brechen fail-closed ab.
12. Tests laufen identisch auf macOS und Linux; nur Layout-Adapter sind `cfg`-spezifisch.
13. Ein deterministischer Austausch nach der Claimprüfung erzeugt keinen Erfolg;
    Fremdobjekt und Journal bleiben auch nach wiederholter Recovery unangetastet.

## Umsetzungsschritte

1. Gemeinsame Claim-, Identitaets- und Journal-Typen in `core/src/trash.rs` anlegen.
2. Injizierbaren Test-Layout-Adapter und die Race-/Recovery-Tests implementieren.
3. XDG-Adapter implementieren und auf dem Linux-Testhost headless pruefen.
4. macOS-Adapter implementieren und ohne echten Papierkorb gegen ein Test-Layout
   pruefen.
5. Tauri-Command auf die neue Kern-API umstellen und die `trash`-Crate entfernen.
6. Erst nach gruener Headless-Suite den ausdruecklich freigegebenen GUI-Papierkorbtest
   in `tools/run-test-sandbox.sh` ausfuehren.

Alle sechs Schritte sind umgesetzt; die externe `trash`-Crate ist entfernt. Der
ausdruecklich freigegebene GUI-Test verschob eine synthetische Datei im echten
macOS-Fenster wiederherstellbar in den Papierkorb. Echte Papierkorbtests finden
weiterhin ausschliesslich in der markierten Wegwerf-Sandbox und nur nach
ausdruecklicher Loeschfreigabe statt.

## Zielvertrag für private Claims (Journal v2)

### Sicherheitsgrenze und Beleg im bisherigen v1-Code

`trash_one()` in `core/src/trash/legacy.rs` benennt zuerst innerhalb von
`source_parent` um, prüft den Claim und verwendet denselben öffentlichen Elternordner
später erneut als Rename-Quelle. Der Test
`claim_austausch_nach_pruefung_bleibt_unclear_auch_nach_neustart` tauscht den
Claim an genau dieser Grenze aus: Der fremde Nachfolger erreicht den Papierkorb;
Readback und Journal verhindern nur eine falsche Erfolgsmeldung und Recovery.
Auch der alte `restore_claim()` besitzt keinen privaten Quellnamensraum.

Der neue Ablauf schützt gegen andere Benutzer, die den ursprünglichen Quellordner
beschreiben dürfen, einschließlich eines bereits gehaltenen Quell-Elternhandles.
Er schützt die Identität des umbenannten Eintrags, nicht dessen Inhalt gegen
Schreibzugriffe über bereits geöffnete Datei-/Unterverzeichnishandles. Prozesse mit
derselben effektiven UID und privilegierte Prozesse sind keine isolierbaren Gegner
dieses Vertrags. Gewöhnliche gleichzeitige Papierkorb-Aktionen müssen trotzdem durch
eigene Laufnamen, exklusive Renames und laufbezogenen Journalbesitz funktionieren.

Der erste Rename kann weiterhin einen zwischenzeitlich ausgetauschten Quellnamen
beanspruchen: Die verwendeten lokalen Rename-APIs nehmen keine erwartete Inode als
Bedingung entgegen. Ein solcher Eintrag darf ausschließlich im privaten Claim-Bereich landen
und bei Identitätsabweichung nicht in den sichtbaren Papierkorb gelangen. Ein
Rücksetzen ersetzt niemals einen inzwischen neu angelegten Originalnamen.

### Privaten Bereich ohne anfängliches Zugriffsfenster anlegen

Quelle, Claim-Bereich und Papierkorbziel müssen auf **demselben Mount** liegen.
Gleiche `st_dev`-Werte allein ersetzen keine Mountbindung; die vorhandenen
`LocalMountIdentity`-/`RootHandle`-Verträge liefern die Grundlage. Mountpunkte als
ausgewählte Einträge, fremde Mounts und `EXDEV` werden vor dem ersten Claim abgelehnt.
Eine aktive Sandbox bindet auch Anker, Claim-Bereich, Papierkorb und Journal an ihren
gehaltenen Root. Keine dieser Hilfswurzeln darf in einem ausgewählten Teilbaum liegen.

Ein Lauf erhält einen exklusiv angelegten Bereich
`.doppeldecker-trash-<Zufallskennung>` außerhalb der sichtbaren Papierkorbeinträge.
Die Kennung kommt aus der Betriebssystem-Zufallsquelle; jeder Eintrag hat darin einen
eigenen Namen. Quelle, Bereich, Trash- und Info-Verzeichnis bleiben bis zum Abschluss
über Deskriptoren gebunden. Sichtbare Pfade dienen der Anzeige und Wiederherstellung.

Die Auswahl erfolgt in dieser Reihenfolge:

1. Einen bereits privaten, eigenen Anker auf dem gebundenen Mount verwenden: zunächst
   das Benutzer-Papierkorbverzeichnis, danach einen geeigneten Quell-Vorfahren.
2. Fehlt dieser, darf ein eigener Vorfahr mit exklusivem Namensraum als Anlageort
   dienen, sofern seine Rechte **schon beim `mkdirat(0700)`** fremden Zugriff auf das
   neue Kind ausschließen. Nicht nur aktuelle Zugriffe, auch vererbbare ACLs prüfen.
3. Ist kein solcher Ort belegbar, bleibt der Quellname unverändert. Kein Anlegen im
   gemeinsam beschreibbaren Quellordner mit anschließendem Reparieren seiner ACL.

Für beide Ankerarten muss der Eigentümer die effektive UID sein. Ein exklusiver
Anlageort hat keine fremden Schreibrechte und keine ACL-Rechte zum Umbenennen,
Entfernen, Ändern seiner Rechte oder Übernehmen des Eigentums. Auf macOS ist als
kleine, überprüfbare erste Unterstützung nur eine fehlende ACL oder eine gültige
ACL mit ausschließlich `Deny`-Einträgen zulässig; zusätzlich gelten die passenden
POSIX-Modusgrenzen (`0700` privat, keine Gruppen-/Fremdschreibrechte exklusiv).
Auch vererbbare Einträge dürfen keine `Allow`-Rechte enthalten. Damit können normale
Trash-Schutz-ACLs erhalten bleiben. Weitere ACL-Klassen brauchen einen eigenen
positiven Beleg; das bloße Nichtfinden eines einzelnen gefährlichen Bits reicht nicht.

Unter Linux darf ein privater Anker keine wirksamen Access-Rechte für andere Benutzer
haben. Ein exklusiver Anlageort darf keine wirksamen fremden Schreibrechte besitzen;
POSIX-ACL-Masken müssen bei der Auswertung berücksichtigt werden. Default-ACLs sind
nur zulässig, wenn die Anlage mit `0700` die Access-Maske des neuen Kindes von Beginn
an schließt. Der genaue Kernel-Readback ist Pflicht. Unbekannte Rechte-/Owner-Modelle,
Ownership-deaktivierte Volumes und unbelegte Sicherheitskontexte erfüllen das Gate
nicht. Aktives SELinux benötigt einen gesondert belegten Kontextadapter.

Nur am **neu erzeugten Hilfsverzeichnis** darf die eigene geerbte ACL entfernt und
`0700` descriptor-relativ gesetzt werden. Den Elternordner und den normalen
Papierkorb verändert dieser Schritt nicht. Danach Eigentümer, Modus, ACL, Mount und
Identität zurücklesen, einen Laufmarker mit `O_EXCL` dauerhaft schreiben und Kind
sowie Elternordner synchronisieren. Erst ein dauerhaftes `area_ready` erlaubt einen
Quell-Rename. Ein vorhandener gleichnamiger Bereich wird niemals übernommen.

Die vorhandenen `RootHandle::is_private()`-/`create_private_child()`-Helfer sind
Anknüpfungspunkte. Ihre macOS-Prüfung akzeptiert derzeit nur leere ACLs; normale
`Deny`-ACLs und die sichere Anlage aus einem exklusiven Vorfahren brauchen die oben
genannte Ergänzung. `ensure_private_directory()` aus dem alten Papierkorbschreiber
prüft dagegen nur Modus/Eigentümer und genügt dafür ausdrücklich nicht.

### Plattform-Lifecycle und Metadatenerhalt

**macOS:** Das Endziel bleibt `$HOME/.Trash` beziehungsweise
`<Mount>/.Trashes/<uid>`. Die dort vorhandene ACL bleibt bytegleich erhalten;
insbesondere gibt es kein `chmod -N` am Trash-Root. Kann dieser Root als privater
Anker belegt werden, liegt der Hilfsbereich als verstecktes Kind darin. Andernfalls
liegt er unter einem belegten Vorfahren auf demselben Mount. Finder-Zugriffe und
Leeren durch denselben Benutzer können den Bereich verändern; dann greifen die
Identitäts-/Fehlergates. Die verborgene Darstellung ist kein Zugriffsschutz und
keine Zusage, dass Finder den Bereich vom Leeren ausnimmt. Nach erfolgreichem Lauf
bleibt dort kein Hilfsbereich. Finders private Zurücklegen-Metadaten werden durch
diesen Vertrag weiterhin nicht erzeugt.

**Linux/XDG:** Der Root bleibt `$XDG_DATA_HOME/Trash`, `<Mount>/.Trash/<uid>` oder
`<Mount>/.Trash-<uid>` nach dem bestehenden Layoutvertrag. Zusätzlich wird der
tatsächliche Mount von `$XDG_DATA_HOME` geprüft; ein getrennt gemountetes Datenhome
darf keinen späteren Cross-Device-Transfer verursachen. Unter einem geeigneten
Trash-Root liegt der Hilfsbereich neben `files/` und `info/`, niemals darin. Die
gemeinsame Sticky-`.Trash` ist kein privater Anker. Fehlt ein geeigneter Root, gilt
die oben beschriebene Auswahl eines sicheren Vorfahren; beide XDG-Mountlayouts
bleiben unterstützt.

Die XDG-Info-Datei wird zuerst vollständig im privaten Bereich mit `O_EXCL` erzeugt,
geschrieben, synchronisiert und mit Identität sowie Inhalt an das Journal gebunden.
Danach wird sie mit `NOREPLACE` als `<Zielname>.trashinfo` nach `info/` veröffentlicht.
Ein Readback und fsync beider Verzeichnisse gehen dem Daten-Rename nach `files/`
voraus. Eine Info-Kollision ersetzt nichts; vor jedem geänderten Zielnamen muss das
Journal die neue Paarung dauerhaft festhalten. `info/` erhält keine Unterordner.
Dies erfüllt die Reihenfolge und Kollisionsanforderung der
[XDG Trash Specification 1.0](https://specifications.freedesktop.org/trash/1.0/#contents-of-a-trash-directory).
Home-Info verwendet den absoluten Ursprung; Mount-Info den an der gehaltenen
Mountwurzel gebundenen relativen Ursprung ohne `..`. Namen bleiben bytegenau
prozentkodiert, die Löschzeit bleibt lokal im vorgeschriebenen ISO-8601-Format.

Beide Plattformen verschieben ausschließlich per Rename. Am ausgewählten Eintrag
gibt es weder `chmod`, ACL-Neuschreiben, neue ACL-Vererbung noch Copy-and-delete.
ACL, Owner, Modus, xattrs, Hardlinks und Inhalte bleiben erhalten; durch Rename
bedingte ctime-/Elternzeitänderungen sind keine Metadatenverlustbehauptung. macOS
erbt beim Verschieben keine neuen ACEs; siehe
[Apple: Inheritance of Permissions](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/FileSystemProgrammingGuide/FileSystemDetails/FileSystemDetails.html).
Anker-, Trash- und Datei-ACL müssen in den Plattformproben getrennt verglichen werden.

### Geordneter Ablauf und Journal v2

Der [strikte v2-Codec](trash-journal-v2-codec.md) und der
[gemeinsame private Kern](trash-private-claim.md) sind implementiert. Der produktive
Recovery-Einstieg bindet vorhandene v2-Belege frisch. Neue macOS- und Linux-Läufe
verwenden die nativen privaten Adapter. Der v1-Schreibweg bleibt ausschließlich
als Regressionsevidenz in Tests erhalten.

Das atomar geschriebene Journal verwendet explizit Version 2. Es persistiert keine
Deskriptoren, sondern Laufkennung/Marker, Mount-/Verzeichnisidentitäten, effektive UID,
bytegenaue Namen, gegebenenfalls die Sandbox-Bindung, erwartete Quellidentität, gegebenenfalls tatsächlich beanspruchte
Identität, ursprünglichen Pfad und bestätigte Info-/Zielnamen. Namen dürfen nicht mit
`to_string_lossy()` in eine andere Pfadidentität verwandelt werden. ACL-Snapshots
oder deren Digests beschreiben die belegten Namensraumrechte.

Der Journalordner selbst braucht vor `planned` einen belegten exklusiven
Namensraum und descriptor-relative atomare Writes mit Eltern-fsync. Die heutige
Modusprüfung oder ein `create_dir_all` durch unbekannte Eltern reicht nicht.
Ein Journal auf einem anderen Mount als die Quelldaten ist zulässig; seine eigene
Durability bleibt zusätzlich zu den Datenverzeichnissen verpflichtend.

| Dauerhafte Phase | Erlaubte nächste Wirkung und notwendiger Beleg |
| --- | --- |
| `planned` | Genauen privaten Anlageintent dauerhaft binden, dann Ressourcen vorbereiten; Quellnamen sind noch unverändert. |
| `untouched` | Terminaler Eintrag ohne begonnene Claim-Wirkung; private Abwesenheit ist geprüft, keine Claim-Identität wird erfunden. |
| `area_ready` | Privater Bereich und Marker sind geprüft und synchronisiert. |
| `claim_intent` | Genau ein exklusiver Rename von `source_parent/source_name` direkt nach `claim_dir/entry_name`. |
| `claimed` | Frischer Claim-Readback passt zur erwarteten Identität; Quelle und Claim-Verzeichnis sind synchronisiert. |
| `info_publish_intent` (XDG) | Nur die gebundene, bereits vollständige eigene Info-Datei darf exklusiv nach `info/`. |
| `info_ready` (XDG) | Info-Readback und Durability sind belegt; erst jetzt ist der Daten-Transfer erlaubt. |
| `transfer_intent` | Genau ein exklusiver Rename vom privaten Claim zum sichtbaren Trash-Ziel; vorher alle gehaltenen Namensraumrechte erneut prüfen. |
| `trashed` | Zielidentität und XDG-Paarung passen; beteiligte Verzeichnisse und Abschlussjournal sind synchronisiert. |
| `restore_intent` / `restored` | Belegten privaten Claim exklusiv zum ursprünglichen Namen zurücksetzen, tatsächliche Claim-Identität am Original zurücklesen und beide Verzeichnisse synchronisieren. |
| `drifted` / `unclear` | Keine automatische Weiterleitung oder Entfernung; Beleg, Teilwirkung und Recoverypfade erhalten. |
| `cleanup_intent` / `settled` | Alle Datenaktionen sind terminal; nur belegte eigene Hilfsobjekte dürfen entfernt werden. Journal erst nach Bereichs-Cleanup und Eltern-fsync entfernen. |

Jede Intentphase wird **vor** ihrer Wirkung dauerhaft gespeichert. Nach dem ersten
Rename prüft der Kern die tatsächliche Claim-Identität fehlererhaltend. Bei Abweichung
wird diese Identität journalisiert und ausschließlich dieses beanspruchte Objekt
zurückgesetzt, sofern Originalname und private Bindung das erlauben. Bleibt die
Nachfrage unbeantwortet oder kollidiert der Originalname, bleiben Objekt und Journal
erhalten. Der sichtbare Papierkorb wird bei dieser Drift nicht berührt.

Ein Statfehler ist nie Abwesenheit. Nach einem begonnenen Transfer bleibt ein
fehlender oder falscher Zielbeleg `unclear`; kein blinder zweiter Rename und keine
automatische Rückverschiebung aus dem Papierkorb. Ein positiver Readback mit späterem
fsync-/Journalfehler darf die beobachtete Verschiebung als `trashed` zählen, muss aber
fehlende dauerhafte Absicherung und das erhaltene Journal ausdrücklich bilanzieren.
Ein dauerhafter terminaler Status wird dadurch nicht vorgetäuscht.

### Recovery, Cleanup und alte Journale

Recovery darf kein Journal eines noch laufenden Papierkorb-Stapels übernehmen.
Die vorhandene prozessweite `trash_journal_guard` umfasst deshalb weiterhin
Recovery und den gesamten GUI-Papierkorblauf; die CLI hält ihren bestehenden
Prozessbesitz. Direkte Core-Aufrufer müssen Recovery und Mutation desselben
Journalordners ebenfalls serialisieren. Private Laufnamen erlauben kein Umgehen
dieser Sperre. Eine spätere Parallelisierung von Papierkorbläufen benötigt einen
eigenen Aktivitätsbeleg und gehört nicht zu diesem Entwurf. Fensterqueues und
unabhängige Dateioperationen bleiben nach ihrem bestehenden Vertrag getrennt.

Beim Neustart werden Marker, Verzeichnisidentität, Rechte, UID und Mount **neu**
gebunden. Ein passender Pfadstring oder ein unverändert wirkender `st_dev` reicht
nicht. Fehlender Mount, geänderte ACL, getauschter Bereich, unbekannte Journalversion
und ungeklärter Datenort erhalten das Journal ohne Mutation. Der Marker verbindet
den neu geöffneten Bereich mit dem ursprünglichen Lauf; ein fremder gleicher Name
darf nicht als eigener leerer Rest entfernt werden.

- Vor `claim_intent` ist kein Quell-Rename autorisiert. Ein möglicher leerer
  Anlage-Rest darf nur nach vollständigem Eigentums-/Markerbeleg entfernt werden.
- Nach `claim_intent`, aber vor `claimed`, wird der private Eintrag zurückgelesen.
  Nur ein zur erwarteten Identität passender Claim darf automatisch zurück; ein
  unerwartetes Objekt ohne dauerhaft erfasste Claim-Identität bleibt zur Prüfung.
- Ein belegter `claimed`-Eintrag kann nach erneutem Privatheitsnachweis exklusiv
  zurückgesetzt werden. Ein vorhandener Originalname wird nie ersetzt. Sind Claim
  und Original gleichzeitig vorhanden, ist das kein abgeschlossener Restore.
- Ein nicht terminales `transfer_intent` wird ausschließlich lesend abgeglichen
  und zur Prüfung erhalten. Auch ein aktuell passender Trash-Name ersetzt keinen
  dauerhaften Abschlussbeleg. Eine belegte `trashed`-Phase ist terminal; späteres
  Leeren des Papierkorbs durch den Benutzer löst keine Neuerzeugung aus.
- Info-Cleanup entfernt nur die journalgebundene eigene Datei in einem weiterhin
  privaten Namensraum, nachdem Daten-Transfer ausgeschlossen bzw. Restore belegt
  ist und kein sichtbares Datenobjekt zu dieser Paarung existiert. Stat-/fsyncfehler
  oder fremde Namen erhalten Info und Journal. Kein Cleanup anhand des Suffixes.
- Hilfsbereiche werden niemals rekursiv geleert. Nach terminalen Einträgen nur die
  vollständige erwartete Restmenge prüfen und `cleanup_intent` persistieren.
  Zusätzlicher Inhalt erhält insbesondere den Marker und den Recoverypfad.
  Anschließend belegte eigene Info-Reste und zuletzt den eigenen Marker entfernen;
  Leerheit prüfen, `rmdir` relativ zum gehaltenen, weiterhin sicheren Anker und
  Eltern-fsync ausführen. Ein Crash nach Markerentfernung ohne bestätigten
  Bereichsabschluss bleibt zur manuellen Prüfung erhalten; keine pauschale
  Entfernung eines nun markerlosen Namens. Erst nach `settled` kann das Journal weg.

Journal v1 bleibt explizit lesbar. Es erhält durch neue optionale Felder keine
rückwirkende private Bindung. Öffentliche v1-Claims dürfen nicht anhand eines
Stat-Vergleichs automatisch zurückbenannt werden: Für einen automatischen Rückweg
muss der Leser zusätzlich einen heute belegten privaten Quell-Elternnamensraum
halten können; andernfalls bleibt der Fall manuell. Vorhandene Transferhinweise
bleiben blockierend. Terminale v1-Abschlüsse werden weiterhin erkannt; unbekannte
Versionen werden weder ignoriert noch umgeschrieben.

### Deterministische Interleavings und Abnahmekriterien

Alle Versuche verwenden eigene Temp-Verzeichnisse, injizierte Trash-Roots und
synthetische Daten. Ein fremder Testbenutzer ist nur für vorbereitete isolierte
Fixtures nötig; niemals Nutzer- oder Systempapierkorbzugriffe als Probe verwenden.

| Eingriff / Fehlerpunkt | Erwarteter Nachweis |
| --- | --- |
| Quellname vor dem Claim austauschen | Fremdobjekt bleibt außerhalb des sichtbaren Trash; exklusiver Restore oder privater Recoverypfad. |
| Nach Claimprüfung den alten öffentlichen Claim-Namen erzeugen | Dieser Name und seine Bytes bleiben unangetastet; Transfer verwendet den privaten Deskriptor. |
| Zweiter Benutzer mit vorher geöffnetem Quell-Elternhandle | Kein Zugriff auf den privaten Claim; die frühere Race-Reproduktion kann kein Fremdobjekt zum Trash-Nachfolger machen. |
| Vererbbare macOS-`Allow`-ACL beim Anlageort | Ablehnung vor jeder Quellmutation; kein anfängliches Fenster durch nachträgliches ACL-Entfernen. |
| Normale macOS-`Deny`-ACL; Datei mit eigener/geerbter ACL | Anker sicher verwendbar; Trash- und Datei-ACL sind nach beiden Renames unverändert. |
| Linux-Default-ACL mit zusätzlichem Benutzer | Das frische Kind ist schon vor ACL-Normalisierung unzugänglich; Datei-ACL bleibt unverändert. |
| Ankerpfad durch Symlink/anderen Ordner ersetzen | Lauf bleibt am gehaltenen Objekt oder stoppt; Recovery mutiert den Nachfolger nicht. |
| Crash vor/nach jedem Intent und jedem Rename/fsync | Der dauerhafte Vorzustand führt ausschließlich zum oben erlaubten Recoveryzweig. |
| XDG-Info-Publish scheitert, kollidiert oder bleibt ohne Antwort | Keine Datei erscheint ohne vollständige Info in `files/`; eigene belegte Reste bleiben auffindbar. |
| Späte Trash-Zielkollision / Readbackfehler | Kein Replace; exakte Teilbilanz und erhaltenes Objekt/Journal. |
| Originalname während Restore neu belegen | Neuer Eintrag bleibt bytegleich; Claim bleibt als Recovery erhalten. |
| Hilfsbereich enthält zusätzlichen Inhalt / Marker driftet | Kein rekursives Cleanup und keine Entfernung fremder Daten. |
| Mount fehlt/wechselt; XDG-Datenhome auf anderem Mount | Ablehnung bzw. vertagte Recovery; kein Copy-and-delete und keine Wirkung auf einen Ersatzmount. |
| Mehrere Läufe und verschiedene Fenster | Bestehende Papierkorbsperre verhindert Recovery eines aktiven Stapels; getrennte Bereiche/Journale, kein Cleanup eines anderen Laufs. |
| Datei, Ordner, finaler Symlink, Hardlinks, Rohbytes und lange Namen | Gleiche Eintragsidentität und Metadaten, korrektes Info-Paar, keine Symlinkverfolgung. |
| v1 ohne/mit Transferhinweis, v2 und unbekannte Version | Keine erfundene private Herkunft; nur belegte Recovery und klare Sperre. |

Die Gegenprobe ist der bisherige öffentliche v1-Claim-Ablauf: Dort muss der gezielte
Austausch das fremde Objekt zum Trash verschieben können; im neuen Ablauf darf er
das nicht mehr. Modus-/ACL-Readbacks allein belegen den Fremdbenutzerzugriff nicht.
Ein nicht ausführbarer Fremdbenutzerversuch bleibt ausdrücklich ungeprüft.

### Getrennte Umsetzungspakete

1. **Gemeinsamer Kern:** Journal-v2-Typen, private Claims, Intent-/Restore-/Cleanup-
   Zustände, injizierte bereits gebundene Layouts und gemeinsame Crash-/Drifttests.
   Keine Änderung an Providerlogik oder Fensterqueues. Vor Integration headless auf
   macOS und Linux gegen den bisherigen Austauschfall prüfen.
2. **macOS-Adapter:** Anker-/Deny-ACL-Prüfung, sichere Anlage, Home-/Volume-Trash,
   vorhandene Trash-ACL erhalten und native APFS-/HFS-Gegenproben. Der Adapter
   hängt vom gemeinsamen Kern ab; keine Umgehung des Privatheitsgates für exFAT.
3. **Linux/XDG-Adapter:** Sichere Anlage mit POSIX-/Default-ACL, beide Mountlayouts,
   Info-Publish, relativer Mountpfad und Mountwechselproben. Ebenfalls auf dem
   gemeinsamen Kern aufbauen; SELinux ohne eigenen Beleg bleibt offen.

Jeder Plattformabschluss verlangt die gemeinsame Headless-Suite und die benannten
plattformabhängigen Gegenproben. Die anschließende native macOS-GUI-Abnahme
ist als zusammenhängender Sandbox-Meilenstein vom 2026-10-07 bestanden.
Implementierungsstand, tatsächliche Prüfungen und verbleibende Plattformgrenzen
stehen im [privaten Papierkorb-Kern](trash-private-claim.md); der historische
Entwurf allein ersetzt diese Implementierungsbelege nicht.
