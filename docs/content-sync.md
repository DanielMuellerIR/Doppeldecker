# Gewöhnlicher Inhaltssync

Stand: 2026-10-30. `ContentSync` ist die eigene Ausführungsklasse für AddOnly,
Update und Mirror über lokale, SFTP-, FTP/FTPS-, WebDAV- und SMB-Endpunkte. Sie
verwendet denselben Planer, Datei-Transferkern, Metadatenbericht und die privaten
gewöhnlichen Provideroperationen. Sie erteilt weder globale strikte
Providerfähigkeiten noch eine vorgetäuschte Remote-Quarantäne. Bestehende
`ProviderSync`-Journalaufträge behalten ihren stärkeren Vertrag.
Remote-Mirror darf nach der ausdrücklichen Produktentscheidung vom 2026-10-28
endgültig ersetzen und entfernen, wenn genau diese Wirkung bestätigt wurde.

## Wirkung und Bestätigung

AddOnly fügt ausschließlich fehlende Inhalte und Ordner hinzu. Update benötigt
ContentHash und ersetzt nur bei einem abweichenden Inhalt beziehungsweise
angeforderten nativen Metadaten und einer mindestens gleich neuen Quelle. Eine
unbekannte Zeitordnung oder ein neueres abweichendes Ziel bleibt ein sichtbarer
Versionskonflikt. Mirror benötigt ebenfalls ContentHash; sein vollständiger
Wirkungsplan bindet zusätzliche Zielobjekte schon vor der Bestätigung.

Die Vorschau nennt beide Provider- und Rootidentitäten, den konkreten Plan,
Metadatenauslassungen, technischen Einschränkungen sowie getrennte Zahlen für
quarantänisierte und dauerhafte Ersetzungen beziehungsweise Entfernungen. HOME-
und Git-Ziele erhalten die vorhandenen Bereichswarnungen. Die Dateisystemwurzel
bleibt als lokales Ganzbaumziel gesperrt. Der Digest bindet diese Wirkung und ihre
konkrete Ausführungsklasse. Die Zustimmung gilt ausschließlich für die gehaltene
Vorbereitung und höchstens zehn Minuten. Über 1.000 dauerhaft entfernte Einträge
oder 10 GiB endgültiger Entfernung benötigen zusätzlich die angezeigte exakte
Phrase. Große Quarantäneverschiebungen sind davon getrennt.

## Quellen, Transfer und lokale Wiederherstellung

Beide Bäume werden vollständig und lesend beobachtet. SFTP benutzt dafür seine
private pfadbasierte Best-Effort-Sicht mit frischen LSTAT-/REALPATH-Prüfungen; ein
beobachteter Symlink wird niemals verfolgt. Der verbleibende serverseitige
Elternpfadwettlauf wird ausdrücklich angezeigt. Profile, Sitzungen, Roots und
Listings werden vor Bestätigung und Ausführung erneut geprüft. Nach der initialen
Hashplanung werden Inhalte nicht bei jedem Weiterreichen vollständig neu gehasht:
Der tatsächliche Transferstrom muss unmittelbar vor dem Commit dem bestätigten
Quellhash entsprechen; vorhandene Zielversionen werden frisch gegen ihren
bestätigten Hash und Identitätsbeleg geprüft. Ein mutierender Befehl wird nach
Antwortverlust niemals blind wiederholt.

Neue Dateien werden vollständig in exklusive Temps übertragen und vor dem Commit
zurückgelesen. Neue Namen und dauerhafte Einzelersetzungen verwenden die
bestehenden gewöhnlichen Commithelfer mit frischem Elternbestand,
Kollisionsprüfung, genau einer Mutation und Readback. Remote↔Remote streamt über
diese Kerne und benötigt keinen lokalen Klartext-Spool.

Ein tatsächlich starkes lokales Ziel behält auch bei einer SFTP-Quelle den
vorhandenen Quarantäneweg: gehaltene Eltern, konkrete Mount- und Flushzusagen,
Restore-Probe, manifestierter Lauf und identitätsgebundener Claim. Die Vorschau
weist diese gesicherten Versionen ausdrücklich aus. AddOnly fordert weder
Geschwister-Schreibrechte noch eine Restore-Probe. Fehlt am konkreten Ziel die
starke Fähigkeit oder ist die Geschwisterquarantäne sauber als nicht verfügbar
belegt, darf die exakt benannte Wirkung nach sichtbarer Zustimmung dauerhaft
laufen. Ein möglicherweise wirksamer oder unklar aufgeräumter Probenfehler ist
keine solche Freigabe. Quarantäne- und Mountaustausch sowie fehlender Platz stoppen
vor weiterer Wirkung.

## Mirror

Alle Kopien und Dateimetadaten kommen vor den Entfernungen. Vor der Löschphase
wird die Quelle vollständig neu beobachtet und mit dem bestätigten Bestand
verglichen. Neue Quellmitglieder sperren sämtliche Entfernungen. Sobald der Plan
Symlinks, Spezialfälle oder Konflikte auslässt, werden ebenfalls alle
Mirror-Entfernungen sichtbar aus dem ausführbaren Teil genommen; unabhängige
Kopien bleiben möglich. Ein fehlerhaftes oder unvollständiges Listing gilt niemals
als leere Quelle.

Dauerhafte Remote-Entfernungen verwenden den vorhandenen `RemoteDelete`-Kern:
bereits bestätigte Records, frischer Vergleich derselben Objekte, Inhalts- und
Versionsprüfung je Blatt, genau eine Einzelmutation und Abwesenheitsreadback.
Ordner verschwinden ausschließlich nach ihren bestätigten Kindern und nur leer.
Frisch entdeckte Zielmitglieder werden nicht nachträglich in die Löschwirkung
aufgenommen. Mehrere Entfernungsgruppen führen Fortschritt und bestätigte
Teilwirkungen kumulativ; ein späterer Fehler erhält deren Queue- und Auditbilanz.
Starke lokale Ziele verschieben die bestätigten zusätzlichen Objekte stattdessen
in die laufbezogene Quarantäne. Endgültige Ordnerzeiten folgen nach dem letzten
Kindcommit beziehungsweise der letzten geplanten Entfernung.

## Metadaten

`Portable`, `NativeBestEffort` und `NativeStrict` behalten ihre unterschiedlichen
Verträge. Inhalte und am konkreten Ziel darstellbare Metadaten werden übertragen.
SFTP überträgt im privaten Inhaltssync Dateizeit und POSIX-Modus per SETSTAT mit
Readback. SMB benutzt seinen bestehenden FILE_BASIC_INFORMATION-Zeitkern auch
für diesen privaten Syncweg. Neue portable Ordner erhalten ihre bekannte Zeit
postorder. ACL-Modelle werden nicht ineinander erfunden; nicht beobachtbare oder
nicht darstellbare Klassen bleiben im pfadgenauen Bericht sichtbar. Ein striktes
Profil wird niemals still herabgestuft.

Nur im gewöhnlichen `NativeBestEffort`-Profil darf SMB eine eindeutig bestätigte
`NTSTATUS_ACCESS_DENIED`-Antwort auf das `WRITE_ATTRIBUTES | SYNCHRONIZE`-OPEN
vor jedem Zeit-SET als pfadgenaue `Mtime`-/`PrivilegeFailure`-Auslassung bilanzieren.
Die Inhalte und unabhängigen Metadaten bleiben übertragbar; die Quellzeit wird für
dieses Objekt ausdrücklich nicht als übertragen bestätigt. Eine bloße
`PermissionDenied`-Zuordnung, Authentifizierungs- oder Signingabsage reicht nicht.
`Portable` und `NativeStrict` erhalten diese Privilegienausnahme nicht. Erkannter
Identitätswechsel und Abbruch bleiben Fehler. Nach möglichem SET sowie bei
unklarem Close oder Readback bleibt der Ausgang unklar; es gibt keinen weiteren
mutierenden Versuch.

SMB beobachtet im nativen Profil geordnete Windows-DACLs und Owner-/Group-SIDs.
Gewöhnlicher `NativeBestEffort`-Sync kann eine begrenzte geschützte explizite DACL
auf die eigene neue Basisdatei übertragen, wenn ein exakter Ziel-Roundtrip und
der passende SID-Kontext belegt sind. Reine ACL-Abweichungen lösen keinen
Dateitausch aus. Ein eigener Vertrag überträgt geeignete DACLs auch auf bereits
bestehende Unterordner: erst nach Inhalten und übrigen Metadaten, mit dauerhaftem
Intent und getrennter Bilanz bestätigter beziehungsweise unklarer Änderungen.
Die Vorschau nennt Zielordner und endgültige Wirkung ohne Quarantäne. Fehlende
Leserechte, nicht darstellbare ACE-Formen und normalisierende Server bleiben
manifestgebunden als konkrete Auslassung sichtbar und sperren lesbare Inhalte
nicht pauschal. Owner-/Group-SIDs werden nicht gesetzt oder numerisch übersetzt.
Nach einem unklaren SET an einer neuen Datei bleiben deren Temp und Intent zur Prüfung erhalten.
Ein unklarer Ordner-SET bleibt ohne automatische Wiederholung offen. Der besondere
Queueabschluss prüft die gebundene Ordneridentität und den aktuellen Rechtezustand
frisch; er schreibt keine Rechte und restauriert keinen früheren Zustand.
[Modell, Wirkungen und Grenzen](metadata-stage2-design.md#windows-sids-und-dacls-über-smb).

Für bereits bestehende reguläre SMB-Dateien besitzt `NativeBestEffort` außerdem
`SetFileDacl`: eine eigene Rechtewirkung mit null kopierten Dateien und Bytes.
Gleicher Hauptinhalt und gleiche vollständige ADS-Menge bleiben am vorhandenen
Objekt; es gibt keinen Temp-, Daten-, Lösch- oder Renamepfad. Vorherige und gewünschte
DACL müssen zur geschützten expliziten Revision-2-Klasse passen. Der minimale
Zugriff ist `READ_DATA | READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | SYNCHRONIZE`
(`0x00160081`); der stärkere Vertrag eigener Datei-Temps bleibt unverändert.
Owner, Group und SACL werden nicht gesetzt.

Die Wirkung ist ausdrücklich **objektweit**. Mehrere Namen können auf dieselbe
Datei verweisen (Hardlinks); alle bestehenden und künftig angelegten Namen sind
betroffen, auch außerhalb der ausgewählten Ordner. Gehaltene SMB-Handles und eine
beobachtete Linkzahl von eins schließen neue Namen nicht aus. Die Bestätigung
benennt diesen Umfang ausdrücklich. Die bekannte Namensliste ist unvollständig;
ausgewählte Aliasnamen und geänderte Dateiobjekte werden getrennt gezählt.
Der Planner berücksichtigt auch bekannte Noop-/Skip-Aliase desselben gebundenen
Server-/Share-Namensraums und derselben Datei-ID. Gleiche Sollrechte ergeben eine
Objektwirkung; widersprüchliche Sollrechte bleiben vor der Vorschau als Konflikt
ausgelassen. Unabhängige Wirkungen bleiben möglich.

Ein eigener V6-Intent bindet Endpunkt, Root, Konto, Datei-ID, Owner-/Group-SIDs,
Vorher-/Nachher-DACL, Hauptinhalt, ADS und den bestätigten Namensumfang. Ein
atomar gesichertes Ledger und monotone Generationen stehen unter einer gemeinsamen
Registry-Sperre; der dauerhafte `Applying`-Zustand geht genau einem Rechte-SET
voraus. Doppeldecker prüft den gehaltenen und anschließend einen frisch geöffneten
Handle. `Applied` wird vor dem synchronen Queuecheckpoint gesichert; erst danach
folgt der Intentabschluss. Abbruch dazwischen erhält den bestätigten Bericht und
die offene Recovery. `AddOnly` bereitet diesen Inplace-Vertrag nicht vor und liest
keine zusätzlichen Manifeste bereits vorhandener Dateien.

Das heiße Ledger enthält nur offene Vollbelege und monotone Namensraumzähler.
Geschlossene Vollbelege liegen einzeln in einem privaten Unterordner; Ownerzähler
und eine Anzahl/Prüfsumme sichern deren Vollständigkeit bei Recovery. Erst ein
nachweislich dauerhaft geschriebener Queuefortschritt oder bewusster Queueabschluss
gibt den exakt zugehörigen privaten Beleg frei. Direkte Providertransaktionen
behalten ihren vollständigen Closed-Beleg. Auch große Einzelläufe müssen deshalb
keine gesamte abgeschlossene Vollplanhistorie im heißen Ledger halten. Abgeschlossene
fremde Zyklen bleiben nach der Freigabe als monotone Generation sichtbar; eigene
Beiträge eines laufenden Owners werden bei seiner Epochprüfung abgezogen.
Alte V1–V5-Intentformate und Digests bleiben unverändert. Queuejournalversion 35
bindet die neue Aktion, Aliasstimmen und Berichte; ältere Journalversionen dürfen
keine dieser neuen Wirkungs- oder Recoveryklassen enthalten.

Auch diese reine Rechteabweichung zählt als angeforderte Änderung: Update verlangt
unter Berücksichtigung der eingestellten Zeittoleranz eine mindestens gleich neue
Quelle. Ältere oder zeitlich nicht geordnete Quellen bleiben sichtbare
Versionskonflikte. Ein gesperrter ausgewählter Alias verhindert auch die Änderung
über einen anderen Namen desselben Objekts. Mirror und AddOnly behalten ihre
getrennten Verträge.

Offene Dateiintents sperren auch fremde und strikte Syncstarts desselben
Server-/Share-Namensraums vor dem Scan, weil weitere Namen außerhalb des bekannten
Roots liegen können. Der bewusste Queueabschluss liest ausschließlich Identität
und aktuellen Rechtezustand vom selben gehaltenen Objekt. Nur ein erkannter
Vorher- oder Nachherzustand kann geschlossen werden; fremde und unbekannte Zustände
bleiben offen. Es gibt keinen erneuten SET, Restore oder Löschversuch. Bestätigte
und unklare Objektwirkungen bleiben in Fortschritt, Queue, Audit, CLI und Oberfläche
getrennt sichtbar. `Portable`, `NativeStrict` und der alte strikte Syncvertrag
bekommen dadurch keine zusätzliche Schreibfähigkeit.

Ein eigener V7-Vertrag ist auf selbst neu angelegte, nachweislich leere
SMB-Unterordner ohne Ordnerdatenströme begrenzt. Leerheit stammt aus der
ungefilterten vollständigen Quelle; ausgeblendete Kinder zählen als Inhalt.
`AddOnly` darf diese neuen Ordnerrechte übernehmen, verändert dadurch aber keine
vorhandenen Datei- oder Ordnerrechte. Nichtleere Ordner verwenden ausschließlich
den folgenden getrennten Vertrag; unbekannte oder ungeeignete Klassen bleiben
konkrete Auslassungen bei unverändert möglicher normaler Anlage.

Die normale exklusive Anlage verwendet ihre bisherigen Mindestzugriffe. Ihr
Originalhandle und ihre echte Objekt-ID bleiben bis zum Abschluss gehalten.
Gewöhnliche Metadaten folgen zuerst; zusätzliche Eltern- und Rechtehandles
öffnen erst danach und müssen dasselbe ursprüngliche Objekt bezeichnen.
Fehlende optionale Rechte oder Beobachtungen betreffen ausschließlich die DACL.
Scheitert vor dem ersten normalen Zeit-SET die zusätzliche Öffnung oder die nur
für die Objektbindung benötigte ID-Abfrage, darf der Receipt nach bestätigter
Handlefreigabe endgültig aufgegeben werden. Der bisherige Zeitweg schreibt danach
genau einmal. Erkannter Identitätswechsel und Abbruch bleiben Fehler.
Nach einem möglicherweise gesendeten SET findet kein Mutationsretry statt.
Ein aufgegebener Receipt wird niemals aus einem Pfad wiederhergestellt.

Verweigert auch der einmalige normale Zeitweg nach dieser endgültigen Freigabe
nachweislich das OPEN vor SET, übernimmt der Aufrufer dieselbe typisierte
Privilegienauslassung. Er versucht die Zeit weder im frühen Abschluss noch im
späteren Ordnerdurchgang erneut. Der Receipt bleibt aufgegeben, eine V7-DACL-Wirkung
findet nicht statt. Der interne Zeitstatus unterscheidet ausstehend, exakt
bestätigt und legitim ausgelassen: Nur für die ausgelassene Zeit entfällt der
Zeitgleichheitscheck; Quellmanifest, Zieltyp und übrige Metadaten bleiben geprüft.
`ordinary_metadata_applied` bleibt bei einer solchen Auslassung `false`, denn die
gewöhnlichen Metadaten sind nicht vollständig übertragen. Diese Beobachtung ist
keine Wiederaufnahmefreigabe. Anlage und Auslassungen stehen in ihren bestehenden
Berichten; weder Journalversion noch V7-Plan- oder Digestformat ändern sich.

Anfangsrechte und tatsächlicher Owner-/Group-Kontext werden am neuen Objekt
beobachtet. Die Anfangs-DACL darf geerbt oder ungeschützt sein und wird niemals
zurückgeschrieben. Nur die gewünschte DACL durchläuft den geschützten expliziten
Encoder. Source- und Targetgruppen werden nicht gleichgesetzt; der tatsächliche
Zielkontext muss zur vorbereiteten Probe passen. Owner, Group, SACL und Inhalte
werden durch die Rechtewirkung nicht verändert.

V7 bindet Herkunft, Objekt-ID, Endpunkt, Auswahlroot, SID-Kontext und beide
Rechtezustände. `Applying` ist vor dem einzigen DACL-SET dauerhaft, `Applied`
nach exaktem gehaltenem und frischem Readback. Erst der dauerhaft geschriebene
Queuecheckpoint gibt den zugehörigen privaten Beleg frei. Alle zusätzlichen
Handles schließen vor der nächsten normalen Mutation; bereits erledigte normale
Ordner-Metadaten werden im späteren Durchgang nicht erneut geschrieben.
Die normale Anlage und ihr Metadatenabschluss bleiben im eigenen Teilbericht
sichtbar, auch wenn Rechte ausgelassen werden oder ein späterer Abschluss scheitert.

Nach unklarem Rechteausgang prüft der bewusste Queueabschluss ausschließlich
Identität und aktuellen Rechtezustand. Nur ein erkannter Vorher- oder Nachherzustand
kann geschlossen werden; fremde oder unbekannte Zustände bleiben offen. Es gibt
keinen erneuten SET, Restore oder Löschversuch. Ein unklarer normaler CREATE oder
Zeitabschluss bleibt im bisherigen normalen Staging-/Queuevertrag und erteilt
keine spätere DACL-Freigabe. Journalversion 36 schützt alle neuen Plan-, Bericht-
und Recoveryformen. V1–V6, `Portable`, `NativeStrict` und vorhandene Ordnerrechte
werden dadurch nicht erweitert.
[Technischer Umfang und Grenzen](metadata-stage2-design.md#rechte-selbst-neu-angelegter-leerer-smb-unterordner).

Für selbst neu angelegte **nichtleere SMB-Unterordner** gibt es einen getrennten
V8-Vertrag. Nur gewöhnlicher `NativeBestEffort`-Inhaltssync mit `ContentHash` darf
geeignete geschützte explizite Quell-DACLs **vor den Kindern** übernehmen. Der
vollständige ungefilterte direkte Quellbestand muss nachweislich nichtleer sein;
Rohnamen und Dateiarten werden vor CREATE und unmittelbar vor dem Rechte-SET
frisch verglichen. Ausgeblendete Kinder gelten weiter als Inhalt. Ordnerstreams
müssen vollständig beobachtet leer sein. Ein gleicher Sekundenzeitstempel allein
belegt keinen unveränderten Bestand.

Nach der normalen exklusiven Anlage bindet der zusätzliche Rechteweg die originale
Objekt-ID, Root und Eltern sowie tatsächliche Owner-/Group-SIDs und Anfangsrechte.
Genau ein Rechte-SET folgt auf den dauerhaften Intent. Gehaltener und frischer
Readback müssen die gewünschten Rechte bestätigen. Der Queuecheckpoint wird
synchron dauerhaft geschrieben; anschließend schließen alle Original-, Rechte-
und Elternhandles und der Intent. Erst danach beginnen normale Kinderaktionen.
Die Ordnerzeit wird dabei noch nicht gesetzt. Sie folgt nach den Kindern und muss
auch am tatsächlich zum SET geöffneten Handle sowie im frischen Readback dieselbe
originale Ordner-ID belegen. Der Receipt wird nicht über die Kinder gehalten und
niemals aus einem späteren Pfad rekonstruiert.

Die frühe Rechtewirkung und der gewöhnliche Ordnerabschluss bleiben getrennt.
Bestätigte Rechte bleiben bei Kindfehler, Abbruch oder späterem Zeitfehler bestehen.
Eine legitime Zeitauslassung behält `ordinary_metadata_applied=false`; ohne bekannte
und setzbare Quellzeit wird ebenfalls kein vollständiger Zeitabschluss behauptet.
Die Beobachtung kann nur für den exakten aktuellen Herkunfts-/Namespace-/Pfad-/ID-
Schlüssel auf `true` wechseln. Reine Anlagehistorie darf einen neu bestätigten
Folgeplan überleben, liefert aber keinen Receipt oder späteren Schreibauftrag.

Die Datei-DACL-Probe und jeder tatsächliche Datei-Temp müssen denselben beobachteten
Owner-/Group-Kontext besitzen. Ein vererbender Root und eine danach geschützte
explizite Parent-DACL können unterschiedliche Group-Kontexte erzeugen. Dann bleiben
Datei-DACLs mit der konkreten Kontextauslassung bilanziert; unabhängige Inhalte,
Dateizeiten und darstellbare Streams bleiben übertragbar. Doppeldecker übernimmt
keine Quellgruppe und bindet die Probe nicht still auf einen anderen Kontext um.
Der geprüfte Folgeweg unter einem geeigneten geschützten expliziten Root ist
keine allgemeine Zusage vollständiger Windows-Metadatenübertragung.

V8 besitzt eigene Payload-/Digest- und Speicherformen. Journalversion 37 schützt
alle Plancontainer, geänderte Vorschauen, Fortschritts-/Abschlussberichte und jede
Recoveryphase einschließlich `Prepared` ohne behauptete Rechtewirkung. V7-Payloads,
Digests, Speicherpfade und Epoch ohne sichtbaren V8-Beitrag bleiben unverändert;
beide Klassen schützen dieselbe kanonische Server-/Share-/Objektidentität vor
Überlappung. Nach möglichem SET prüft Recovery ausschließlich den bekannten Vorher-
oder Nachherzustand am ursprünglichen Objekt. Sie setzt weder Rechte noch Zeit
erneut, stellt nichts zurück, löscht nichts und setzt keine Kinderarbeit fort.
`Portable`, `NativeStrict`, vorhandene Ordner und andere Anbieter erhalten dadurch
keine neue Schreibfähigkeit.
[Technischer Umfang und Grenzen](metadata-stage2-design.md#rechte-neuer-nichtleerer-smb-unterordner).

WebDAV bindet im nativen Profil die Beobachtung von `DAV:acl` und `DAV:owner`
an jedes Objekt. Fehlende Properties und Berechtigungen bleiben mit ihrem
konkreten Status sichtbar; auch erfolgreiche Werte werden nur opak beobachtet.
Sie begründen keine vollständige übertragbare ACL oder numerische Ownerzuordnung.
Beide Endpunkte bleiben im Bericht und in der Driftprüfung berücksichtigt, auch
bei einem leeren Folgelauf. Lesbarer Inhalt benötigt dafür keinen künstlichen
Dateitausch. [Beobachtungs- und Statusvertrag](metadata-stage2-design.md#webdav-sicherheitsbeobachtungen).

Reguläre SMB-Dateien erhalten im nativen Profil ihre benannten Datenströme auf
einem dafür geeigneten SMB-Ziel. Stream-only-Änderungen lösen auch bei identischem
Hauptinhalt ein Update aus; der Commit ersetzt die ganze zuvor geprüfte Datei
samt vollständiger Streammenge. Auf nicht darstellbaren Zielen bleibt der Inhalt
übertragbar und die ausgelassene Klasse sichtbar, auch bei einem Folgelauf ohne
erneute Inhaltskopie. Bereits gleiche Streammengen erzeugen keine Auslassung;
unveränderte Zieldateien behalten ihre vorhandenen Streams.

SMB-Verzeichnisstreams werden vollständig beobachtet und über einen getrennten
Ordnervertrag im Profil `NativeBestEffort` übertragen. AddOnly verändert
vorhandene Ordnerstreams nicht; neue Ordner erhalten ihre Quellmenge. Update und
Mirror bestätigen Ergänzungen, Ersetzungen und überzählige Streamentfernungen
pfadgenau. Ersetzen und Entfernen sind dauerhaft ohne Quarantäne. Zwischen frischer
vollständiger Zielprüfung und einzelnem Rename bleibt ein Best-Effort-Fenster für
parallele Änderungen; ein atomarer Versionsvergleich ist nicht verfügbar.
Alle Streamkopien laufen vor Stream- und Dateientfernungen. Bestätigte und unklare
Einzelwirkungen bleiben in Queue, Ergebnis und Audit getrennt erhalten, auch bei
Abbruch oder Neustart. Zusammengeführte Ordner werden dabei nicht ersetzt.

Auf ungeeigneten Zielen bleiben abweichende Streammengen auch beim sonst leeren
Update konkret als Auslassung bilanziert. Eine nicht unterstützte
Verzeichnisabfrage bleibt als unbeobachtet sichtbar. `Portable` und der getrennte
strikte Sync erhalten dadurch keine neue Metadatenzusage. [Manifest, Grenzen und Abschlussvertrag](metadata-stage2-design.md#benannte-smb-datenströme-regulärer-dateien).

Bei benannten SMB-Datenströmen halten echte Lesehandles die gewählte Wurzel,
deren Nachfahren und die konkrete Basisdatei. Auflistungsrechte oberhalb der
gewählten Wurzel sind dafür nicht erforderlich. Die dortige Pfadauflösung bleibt
mit frischen Identitätsprüfungen Best Effort und wird bei nativen Profilen in der
Vorschau benannt. Öffentliche starke Reader benötigen dagegen die vollständige
Bindung ihrer Pfadkette.

SFTP prüft vor jedem SETSTAT den bereits gebundenen echten Datei- beziehungsweise
Ordnertyp erneut; ein beobachteter Typwechsel erhält keine Metadatenmutation.
Quellzeiten außerhalb des SFTP-v3-Zahlenbereichs werden bei Portable und
NativeBestEffort pfadgenau als Protokollgrenze bilanziert, während der Inhalt
übertragen wird. NativeStrict wird dadurch nicht herabgestuft.

Numerische Owner-/Gruppen-IDs werden ohne bestätigte gemeinsame UID/GID-Domäne
nicht zwischen entfernten Systemen und dem lokalen Ziel übertragen. Bekannte
Quell-IDs und eine fehlende Beobachtung sind unterschiedliche Berichtsfälle.
NativeBestEffort lässt die Ownerzuordnung mit konkretem Domänengrund aus;
NativeStrict lehnt den unvollständigen Vertrag ab. Die vorhandene lokale
Ownerübernahme bleibt erhalten.

FTP/FTPS überträgt beobachtete normale POSIX-Rechte über SITE CHMOD mit exaktem
MLSD-Readback, auch nach dem abschließenden Rename. Fehlendes `unix.mode`, konkrete
Ablehnung und ignorierte Rechte werden getrennt bilanziert; Antwortverlust bleibt
ein Fehler ohne Wiederholung. Sonderbits werden nicht aus normalen rwx-Bits
abgeleitet. Beobachtete Quellrechte bleiben auch beim FTP-Download nutzbar.

FTP/FTPS versucht im privaten Inhaltssync MFMT am eigenen Uploadtemp und an
gebundenen Ordnern. WebDAV versucht bedingtes PROPPATCH für genau
`DAV:getlastmodified`; die vollständige 207-Antwort wird pro Property ausgewertet.
Beide Wege prüfen den Zeitwert exakt durch frisches Lesen. Konkrete
Setterablehnung, geschützte beziehungsweise ignorierte Properties oder bewiesene
abweichende Zeitauflösung werden pfadgenau als Auslassung bilanziert, während der
Inhalt übertragen wird. Verbindungsverlust, mehrdeutige Antworten und Drift bleiben
Fehler; eine möglicherweise erfolgte Mutation wird nicht wiederholt. Ein fehlender
Quellzeitwert wird ebenfalls ausdrücklich bilanziert. Die Vorschau bezeichnet die
Zeitübernahme als Versuch ohne Erhaltungszusage vor der Serverantwort; der Abschluss
ergänzt den gemeinsamen Metadatenbericht um tatsächlich bewiesene Auslassungen.
`NativeStrict` wird durch diese Laufzeitausnahme nicht abgeschwächt.

WebDAV-Dateien verwenden zusätzlich den dokumentierten `X-OC-MTime`-Header am
ersten ohnehin erforderlichen PUT. Ein behauptetes `accepted` ersetzt keinen
exakten Temp- und MOVE-Readback. Ignoriert der Server den Header, folgt höchstens
der vorhandene bedingte PROPPATCH-Zeitversuch; es gibt keinen zweiten
Inhaltsupload. Parallele Uploadkontexte halten ihre Zeitwerte getrennt. Die
HTTP-Gegenfälle sind geprüft; eine echte Nextcloud-Instanz bleibt gesondert
abzunehmen.

## Queue, Audit und Neustart

### Bekannte eigene Zwischenstände

Eine notwendige Abschlussbedingung der laufenden Stabilisierung ist der gemeinsame
Registrycheck vor der Planung, vor Dateiwirkungen und am Laufabschluss. Er muss
bekannte lokale Arbeitsverzeichnisse und Nachbartemps sowie entfernte Uploadtemps
aller Fenster berücksichtigen. Maßgeblich sind Provider, Konto, kanonischer
Namensraum und betroffene Wurzel; ein ähnlich aussehender Dateiname ist kein Beleg.
Unabhängige Server und nicht überlappende Bereiche bleiben gleichzeitig nutzbar.

Eine aktive Übertragung bleibt auch dann offen, wenn ihre Datei noch nicht sichtbar
ist. Nach einem Abbruch genügt ein Statfehler nicht als Abwesenheitsbeleg. Ein
vorhandener oder nicht eindeutig prüfbarer Zwischenstand stoppt den betroffenen
Sync vor weiterer Wirkung. Zwischen Anlage und Entfernung während eines Scans darf
der Guard keinen scheinbar unveränderten leeren Registryzustand akzeptieren.

Ein Tempintent beweist keine heutige Eigentümerschaft am Pfad. Doppeldecker darf
deshalb weder Namen pauschal ausblenden noch gefundene Reste automatisch löschen.
Der Nutzer prüft den konkret gemeldeten Rest und sichert ihn bei Bedarf bewusst
außerhalb beider Syncwurzeln. Danach folgen eine frische Planung und Zustimmung.
Diese Grenze schützt einseitige Quellen vor versehentlichem Kopieren und
Mirrorziele vor versehentlichem Entfernen; der bidirektionale Lauf darf solche
Zwischenstände zusätzlich niemals in die gemeinsame Baseline aufnehmen.

Ein abgebrochener letzter Eintrag des gemeinsamen Temp-Ereignisjournals wird
unter derselben Sperre wie Indexanlage, Append und Abschluss repariert. Ein
atomarer Streamingcommit erhält den gültigen Verlauf, die unveränderten Tailbytes
und alle möglicherweise betroffenen Intents. Er entwertet bestehende
Vorbereitungen; neue unabhängige Wurzeln bleiben nutzbar. Die potenziell
betroffenen Wurzeln bleiben mangels vollständiger Wirkungszuordnung unbekannt,
auch wenn ein späterer normaler Abschluss den Index entfernt. Das bloße Sichern
eines einzelnen Transferrests reicht in diesem Fall nicht als Entwarnung.
Vollständige Korruption oder ein Tail ohne erhaltenen möglichen Intent wird
nicht automatisch umgedeutet oder entfernt. Ein fehlgeschlagener Reparaturcommit
erhält den privaten Zwischenstand als konkreten Recoverypfad.

Seit Journalversion 33 besteht eine eigene `ContentSync`-Absicht mit geheimnisfreien
Endpunktkonfigurationen, Optionen, erwartetem Plan und Digest. Ein älteres Journal
darf diese neue Klasse nicht einschleusen. Handles, Secrets, Permit und Zustimmung
bleiben ausschließlich im Backend-RAM des verursachenden Fensters. Die
Vorbereitung ist außerdem an beide tatsächlichen Providerdefinitionen gebunden;
gleiche Rootzeichenfolgen auf einem anderen Server reichen nicht.

Nach Neustart wird frisch vorbereitet und erneut bestätigt, auch bei gleichem
Plan. Ein beim Prozessende laufender Auftrag bleibt mit möglicher Teilwirkung
unterbrochen und wird niemals automatisch wiederholt. Unklare Mutationsausgänge
behalten Recoverypfade und sperren den gewöhnlichen Retry. `Running` wird vor der
ersten Mutation dauerhaft geschrieben. Der terminale Queuecommit kommt vor dem
Abschlussaudit. Der ausdrücklich übergebene Auditpfad wird append-only mit
NOFOLLOW-, Identitäts- und Hardlinkprüfung bedient; zusätzlich bleibt das Audit der
Fensterqueue erhalten. Journal- oder Auditfehler dürfen bestätigte Teilwirkungen,
Fehlercodes und bekannte Recoverypfade nicht verdecken.
Das Abschlussaudit enthält bei vorliegenden Ergebnisstatistiken den tatsächlichen
`metadata_report` einschließlich pfadgenauer Laufzeitauslassungen und ihrer Zähler.
Fehlen Ergebnisstatistiken, bleibt dieses Feld `null`; es wird kein vollständiger
oder leerer Metadatenabschluss erfunden.
