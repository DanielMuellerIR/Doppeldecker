# Gemeinsamer privater Papierkorb-Kern

Stand: 2026-10-07. Der gemeinsame Kern arbeitet mit bereits gehaltenen,
injizierbaren Layouts. Anlage, Claim, Transfer, Rücksetzung und nichtrekursive
Bereinigung sind implementiert. Die produktive Recovery bindet v2-Journale frisch;
macOS und Linux verwenden ihn auch für neue produktive Läufe. Die gemeinsame
native macOS-GUI-Abnahme besteht in der markierten künstlichen Sandbox. Maßgeblich bleibt der
[Zielvertrag](trash-toctou-design.md#zielvertrag-für-private-claims-journal-v2).

## Anlage und Bindung

`core/src/trash/journal_v2/claim.rs` hält Quelle, Anker, Daten-, Info- und
Journalordner über `RootHandle`. Vor jeder Wirkung prüft der Kern die gebundenen
Identitäten, tatsächlichen Mounts, Eigentümer, Modi und ACL-Stände erneut.
Quelle, Bereich und Papierkorbdaten liegen auf demselben Mount; das Journal darf
auf einem anderen liegen. Hilfswurzeln im ausgewählten Teilbaum und Wurzeln
außerhalb einer vorgegebenen Sandbox werden abgelehnt. Eine vorhandene
Sandbox-Bindung bleibt im Journal für die Recovery erhalten.

Ein geeigneter Anker gehört der effektiven UID und besitzt keinen fremd
beschreibbaren Namensraum. macOS akzeptiert fehlende ACLs oder ausschließlich
Deny-Einträge auf APFS/HFS mit aktivem Ownership. Linux wertet wirksame
POSIX-ACL-Rechte einschließlich Maske aus. Eine Default-ACL ist nur zulässig,
wenn das neue Kind bereits durch die Anlage mit `0700` geschlossen ist; dieser
Zustand wird vor einer ACL-Normalisierung zurückgelesen. Unbekannte Dateisysteme,
nur lesbare Mounts und unbelegte SELinux-Kontexte erfüllen das Gate nicht.

Das erste dauerhafte `planned`-Journal enthält bereits den genauen Anlageintent:
Ankerbeleg, zufälliger Bereichsname und Markername/-Nonce. Ein vorhandener Bereich
wird nie übernommen. Nur am neu angelegten Kind dürfen geerbte ACLs entfernt und
`0700` gesetzt werden; Quellobjekt, Anker und normaler Papierkorb bleiben dabei
unverändert. Marker, Kind und Anker werden vor `area_ready` synchronisiert.

Journalwrites erfolgen relativ zum exklusiven gehaltenen Journalordner: zufällige
Tempdatei mit `O_EXCL`, vollständiger Write, Dateisynchronisierung, Identitätscheck,
atomarer Rename und Eltern-fsync. Ein fremder Nachfolger unter dem Journalnamen
wird nicht übernommen. Ein fehlgeschlagener Write sperrt weitere Wirkungen des
Laufs und erhält Hinweise auf die wiederherzustellenden Pfade.

## Claim, Info und Transfer

Nach dauerhaftem `claim_intent` führt genau ein exklusiver Rename vom öffentlichen
Quellnamen direkt in den privaten Bereich. Es gibt keinen öffentlichen
Zwischenclaim als spätere Transferquelle. Der tatsächliche Claim wird frisch
zurückgelesen und mit seiner beobachteten Identität gespeichert.
Stimmt er nicht mit dem ausgewählten Objekt überein, darf nur dieser beobachtete
Claim exklusiv zum Originalnamen zurückgesetzt werden. Eine späte Kollision
ersetzt nichts. `Drop` führt keine Dateiwirkung aus.

`claim/transfer.rs` erzeugt XDG-Info zunächst vollständig im privaten Bereich.
Identität, Inhaltshash und Größe werden vor `info_publish_intent` gespeichert.
Der exklusive Publish nach `info/`, sein Readback und die Synchronisierung beider
Verzeichnisse gehen dem Daten-Transfer voraus. Home-Info enthält den absoluten
Ursprung; Mount-Info wird relativ zur frisch geprüften tatsächlichen Mountwurzel
gebildet. Namen bleiben bytegenau prozentkodiert. Eine unbekannte Info-Datei,
etwa nach einem Abbruch vor ihrer Journalbindung, bleibt zur Prüfung erhalten.

Nach `transfer_intent` geht genau ein exklusiver Rename vom privaten Claim nach
`files/`. Ein passender Ziel-Readback und die geprüfte Info-Paarung belegen die
beobachtete Wirkung. Erst Verzeichnissynchronisierung und Journalcommit belegen
den dauerhaften Abschluss `trashed`. `TransferOutcome::Observed` erhält einen
positiven Readback bei späterem Durability-Fehler ausdrücklich getrennt davon;
der Lauf bleibt gesperrt und liefert Journal-, Bereichs- und betroffene Zielpfade.

## Recovery und Cleanup

`claim/recovery.rs` öffnet Journal, Anker, Daten-/Info-Verzeichnis, Sandbox und
gegebenenfalls Quellen erneut ohne Symlinkfolgen. Es vergleicht UID, Objekt- und
Mountidentität, Modus, ACL sowie Marker. Ein passender Pfadstring genügt nicht.
Die Quellbindung bleibt dem ursprünglichen Journalindex zugeordnet. Nur Phasen,
die die Quelle tatsächlich lesen oder zurücksetzen, öffnen ihren Quell-Elternordner
neu. Terminale Einträge benötigen ihn auch in gemischten Journalen nicht; ein
nachträglich entfernter Ordner blockiert deshalb keinen unabhängigen offenen Claim.
Rein manuell zu prüfende Transferphasen binden ebenfalls keine ungenutzte Quelle.
Nicht terminale Claims dürfen nur mit eindeutiger belegter Identität exklusiv
zurück. Original und Claim gleichzeitig bleiben manuell, auch bei gleicher Inode.
Ein `restore_intent` mit eindeutig bereits zurückgesetztem Objekt wird frisch
synchronisiert und als `restored` gespeichert.

Ein noch `planned` gebliebener Eintrag wird erst nach belegter Abwesenheit seines
privaten Claims `untouched`. Er besitzt keine erfundene Claim-Identität.
Eine nicht abgeschlossene Bereichsanlage ohne dauerhaftes `area_ready` bleibt
konservativ manuell erhalten, selbst wenn die Quelle noch unverändert ist.
Ein nicht terminales `transfer_intent` wird ausschließlich lesend behandelt;
ein gerade passender Trash-Name ersetzt keinen dauerhaften Abschlussbeleg.
Terminale `trashed`-Einträge werden nach späterem Leeren durch den Benutzer nicht
neu erzeugt.

Cleanup verlangt ausschließlich terminale Einträge (`trashed`, `restored`,
`untouched`) und eine vollständige Liste der erwarteten privaten Restmenge.
Fremder Inhalt sperrt jede Entfernung. Eigene Info-Reste dürfen nur bei passender
Identität und Inhalt, ausgeschlossenem Daten-Transfer und fehlendem sichtbarem
Datenobjekt entfernt werden. Fremde Info- oder Dateneinträge erhalten Info und
Journal. Alle beteiligten Verzeichnisse werden vor dem Markerabschluss synchronisiert.

Der Marker wird zuletzt entfernt; danach folgen Leerheits-/Identitätsprüfung,
descriptor-relatives `rmdir`, Anker-fsync und `settled`. Erst dann darf das eigene
Journal weg. Ein markerloser Rest oder ein unbestätigter Bereichsabschluss bleibt
manuell. Ein lesbares `settled`-Journal kann bei weiterhin fehlendem Bereich ohne
neue Datenwirkung beendet werden. Ein gleichnamiger Nachfolger sperrt auch diesen
Schritt. Die bestehende prozessweite Papierkorbsperre bleibt verpflichtend;
direkte Core-Aufrufer müssen Recovery und Mutation desselben Journalordners
serialisieren. Andere Fensterqueues werden dadurch nicht zusammengeführt.

## Native macOS-Anbindung

`trash/native.rs` bindet zunächst alle Quell-Eltern und ausgewählten Identitäten.
Home- und Volume-Ziele werden nach tatsächlicher Mountidentität getrennt:
`$HOME/.Trash` beziehungsweise `<Mount>/.Trashes/<uid>`. Die gemeinsame
`.Trashes` muss bereits existieren. Ein fehlendes eigenes UID-Verzeichnis wird
mit `0700` exklusiv vorbereitet, nachdem Owner, Sticky-Schutz, native Rechte und
vererbbare ACLs des gehaltenen gemeinsamen Elternordners geprüft wurden.
Vorhandene Trash-ACLs werden niemals normalisiert.

Jeder Zielroot erhält eine eigene vorbereitete Laufgruppe. Sämtliche Gruppen und
Journale sind vor dem ersten Quell-Rename bereit; während der Layoutvorbereitung
ausgetauschte Quellauswahlen werden abgewiesen. Umbenannte Eltern bleiben über
die ursprünglichen Handles gebunden. Ein ungeklärter Ausgang stoppt die weitere
Ausführung und erhält auch die nicht begonnenen Gruppen zur Recovery. Positive
Transfer-Readbacks zählen weiterhin als verschoben, selbst wenn anschließend
Durability oder Cleanup scheitern; Fehler und Wiederherstellungspfade bleiben sichtbar.

Eine vorgegebene Sandbox verwendet ausschließlich
`<Sandbox>/.doppeldecker-test-trash` und ihren gebundenen Journalordner. Der
Systempapierkorb und HOME werden dabei nicht geöffnet. Der bisherige v1-Schreiber
liegt getrennt in `trash/legacy.rs`; er wird ausschließlich für die
Altjournal- und ursprünglichen Race-Regressionen übersetzt.

## Native Linux-/XDG-Anbindung

`trash/native/xdg.rs` wählt den Home-Papierkorb anhand des tatsächlichen Mounts
von `$XDG_DATA_HOME`, bei fehlendem Wert unter `$HOME/.local/share`. Relative
XDG-Werte werden gemäß dem [Basisverzeichnisvertrag](https://specifications.freedesktop.org/basedir/latest/) ignoriert. Ein fehlendes
Datenhome wird aus einem gehaltenen, exklusiven eigenen Vorfahren mit `0700`
angelegt; Mountwechsel, fremde Schreibrechte und Symlink-Eltern sperren den Lauf.

Auf anderen Mounts wird ein root-eigener Sticky-Ordner `.Trash` mit privatem
UID-Kind verwendet, andernfalls `.Trash-<uid>` direkt unter der gehaltenen
Mountwurzel. Der zweite Weg wird auch bei verweigerter UID-Anlage, unlesbarer
gemeinsamer `.Trash` oder ungeeignetem UID-Kind versucht, jeweils vor einem Claim. Die gemeinsame `.Trash` wird niemals zum privaten Claim-Anker.
Ein vorhandener exklusiver Home-Trash-Root darf `0755` besitzen; `files/`, `info/`
und der neue Claim bleiben privat. Access- und Default-ACLs vorhandener Ordner
bleiben unverändert. Geerbte POSIX-Zugriffsrechte neuer Kinder sind bereits durch
die Kernel-Maske der Anlage mit `0700` geschlossen.

Home-Info bindet den absoluten Ursprung, Volume-Info den relativen Ursprung unter
der tatsächlichen gehaltenen Mountwurzel. Die vollständig geschriebene Info-Datei
wird vor den Daten veröffentlicht. Kollisionen ersetzen keine fremden Dateien.
Die Sandbox verwendet denselben XDG-Ablauf ausschließlich unter ihrem künstlichen
Papierkorb mit `files/` und `info/`; HOME wird dabei nicht ausgewertet. Neu von der
CLI angelegte Konfigurationsordner erhalten `0700`, damit ihre Recovery-Journale
nicht von einer großzügigen Umask abhängen. Vorhandene Ordner werden nicht
umgeschrieben und müssen weiterhin das Rechte-Gate erfüllen.

## Journal v1

Alte Journale erhalten keine rückwirkende private Herkunft. Ein v1-Claim darf nur
mit heute geprüftem privatem Quell-Elternordner, tatsächlichem Mount und passender
Identität zurück. Öffentliche Claims, gleichzeitige Originale und Transferhinweise
bleiben erhalten. Unbekannte Felder und Versionen werden nicht als bekannte
Wirkung interpretiert. Terminale Abschlüsse bleiben erkennbar.

Auch die v1-Recovery hält ihren eigenen exklusiven Journalordner: Lesen, atomarer
Driftcommit und Entfernen bleiben am gebundenen Deskriptor. Ein ausgetauschter
Pfad oder Journalname erbt keine Berechtigung. FIFO und andere nicht reguläre
Journale werden ohne blockierendes Lesen abgewiesen.

### Terminale Drift in Altjournalen

Der frühere v1-Schreibweg ist ausschließlich Testcode. Neue Papierkorbaktionen
verwenden auf beiden Plattformen den privaten v2-Claim. Bei einer vor dem Claim
verschwundenen oder ersetzten Quelle schließt v2 den Eintrag nach belegter
Abwesenheit des privaten Claims als `untouched`. Es entfernt nur den frisch
gebundenen eigenen Bereich und das abgeschlossene Journal. Original und fremde
Ersatzdatei bleiben erhalten; der nächste Recoverylauf meldet denselben Eintrag
nicht nochmals.

Bereits vorhandene v1-Journale mit terminalem `Drifted` bleiben dagegen bewusst
als Altbeleg erhalten. Der Core meldet ihre Drift bei weiteren Recoveryläufen
weiter; die CLI kann sie im JSON-Ergebnis ausgeben. Das ist keine Blockade neuer
Aktionen: `RecoveryStats::is_clean` verlangt fehlende Recoverypfade und Fehler,
nicht eine leere Warnungsliste. Ein fehlender Claim braucht keinen erfundenen
Wiederherstellungspfad. Neue v1-Läufe und damit ein laufendes Anwachsen dieses
Altbestands entstehen durch die Anwendung nicht mehr. Es gibt weder automatische
Alterslöschung noch eine nachträglich erfundene Nutzerquittierung.
[Prüfbericht und Grenze](release-validation.md).

## Prüfung und verbleibende Arbeit

Die künstlichen Layouttests behandeln alle Intentklassen, späte Kollisionen,
Marker-/Bereichs-/Journaltausch, Rechteänderung, Write-/fsync-Fehler, Fremdinhalt,
markerlose Reste und umbenannte gehaltene Eltern. Symlink, Ordnerinhalt, Hardlink,
maximale Namen und Quell-/Anker-ACLs bleiben geprüft. Linux behandelt zusätzlich
rohe Nicht-UTF-8-Namen, relative XDG-Info und eine geerbte Default-ACL vor ihrer
Normalisierung. macOS behandelt native Allow-/Deny-ACLs. Alle Daten und
Papierkorbwurzeln sind künstlich.

Zusätzliche Mount-Gegenläufe benötigen
`DOPPELDECKER_PRIVATE_CLAIM_TEST_MOUNT` und führen ausschließlich die entsprechend
markierten ignorierten Tests aus. Eine Datenwurzel auf anderem Mount wird
abgelehnt; ein getrennt gemountetes Journal ist zulässig. Der Fremd-UID-Gegenlauf
ist auf Linux als ignorierter Test begrenzt und benötigt einen isolierten
Root-Prozess. Das Kind wechselt zu UID/GID 65534 und erhält selbst über einen
bereits gehaltenen privaten Ordner-FD `EACCES`; öffentliche Nachfolger bleiben
beim tatsächlichen gemeinsamen Transfer unverändert.

Native macOS-Gegenläufe auf eigenen APFS-, case-sensitiven APFS- und HFS+-Images
bestehen einschließlich gemischter Volumegruppen und erhaltener Trash-ACLs.
Ownership-deaktiviertes APFS und exFAT bleiben ohne Quellwirkung abgewiesen.
Native Linux-Gegenläufe prüfen Home-/Volume-Auswahl, beide XDG-Mountlayouts,
ein getrennt gemountetes Datenhome, Default-ACL-Erhalt, Symlink-Ablehnung,
Read-only-Mounts und eine unsichere zweite Laufgruppe. Der gemeinsame native
macOS-GUI-Meilenstein vom 2026-10-07 besteht: Auswahl und Papierkorb-Kürzel
verschieben die künstliche Datei in den Sandbox-Papierkorb. Der Quellpfad fehlt,
die ursprüngliche Dateiidentität und der Inhalt sind am Ziel erhalten, das Journal
ist leer und der private Bereich vollständig abgebaut. Die Test-App ist anschließend
beendet. Ein unabhängiger LAN-Suchfehler beim Start der isolierten App berührt
diese Prüfung nicht; die Suche war vor dem eigentlichen Test abgeschaltet.
Die interaktive Linux-GUI-/Installationsabnahme des Releasekandidaten bleibt separat offen.
