# Additive FTP-/FTPS-Downloads

Stand: 2026-11-18.

Gewöhnliche additive Kopien einer FTP-/FTPS-Auswahl in einen lokalen Ordner
verwenden eine private Best-Effort-Lesequelle. Sie umfasst reguläre Dateien,
Verzeichnisbäume und direkte Mehrfachauswahl. Vorhandene lokale Ordner werden
zusammengeführt, vorhandene Blätter nach der konkreten Konfliktentscheidung
übersprungen. Ersetzen, Verschieben, Remote-Ziele, Snapshots und strikter Sync
behalten ihre bisherigen Quellenverträge.

## Fortsetzbarer Download

**Befehle → Dateiinhalt fortsetzbar kopieren** nimmt reguläre Dateien aus einem
verifizierten FTPS-Profil und einen lokalen Zielordner an. Ein kompatibler
Server meldet `REST STREAM` und `HASH SHA-256` und liefert eine Prüfsumme für
die komplette Datei samt passendem Pfad und Byteumfang. FTPS schützt dabei den
Kontroll- und den Datenkanal. Der Server muss die ganze Datei für die
Prüfsummenabfrage lesen können. Getestet ist der Antwortvertrag von ProFTPD
`mod_digest`. [REST STREAM nach RFC 3659](https://www.rfc-editor.org/rfc/rfc3659.html#section-5),
[ProFTPD mod_digest](https://github.com/proftpd/proftpd/blob/master/doc/contrib/mod_digest.html).

Nach einer Pause oder einem Neustart prüft die App Profil, Serveridentität,
Dateigröße, Änderungszeit, Server-SHA-256 und den gespeicherten lokalen
Abschnitt. `REST` fordert anschließend den fehlenden Inhalt über `RETR` an.
Erst der erfolgreiche Transferabschluss und der SHA-256-Vergleich der
zusammengesetzten Datei erlauben den atomaren lokalen Abschluss unter dem
Zielnamen. Änderungen oder ein falsch gelieferter Rest bleiben als
Teilkopiefehler in der Queue sichtbar. Ein falsch zusammengesetzter Inhalt
benötigt einen neuen Lauf. Unverschlüsseltes FTP, ungeprüfte Zertifikate und Server ohne die
beiden Protokollfähigkeiten verwenden weiterhin die normale Kopieraktion.

## Namensgruppen und beobachteter Umfang

Der Core hält vollständige MLSD-Listings der ausgewählten Bäume und ihrer
beobachteten Eltern. Innerhalb ausgewählter Verzeichnisse bindet der Beleg den
vollständigen Bestand. Außerhalb dieser Bäume bindet er nur die benötigten
Auswahl- und Vorfahrnamen; Änderungen unselektierter Geschwister blockieren
keine unabhängige Kopie und ändern nicht deren Digest. Direkte Auswahlgruppen
und rekursive Geschwistergruppen
verwenden dieselbe konservative NFC-/Kleinschreibungsäquivalenz wie die übrigen
interaktiven Quellen. Sämtliche Mitglieder einer mehrdeutigen Gruppe werden
ausgelassen; es gibt keinen zufälligen Gewinner. Ausgelassene Ordner werden nicht
betreten. Unabhängige Dateien und Ordner bleiben übertragbar. Symlinks und
Spezialdateien erscheinen ebenfalls als konkrete Auslassungen.

Vorbereitung und Abschlussbilanz erhalten exakte Pfade, Gründe, Datei-/Ordnerzahl
und Bytes. Ein Baum oder eine Auswahl mit Auslassungen besitzt einen
restartstabilen Digest. Reine reguläre Dateiauswahl ohne Auslassungen benötigt
keinen Baumbeleg. Ungültige oder exakt doppelte Auswahlpfade bleiben Fehler.

## Fakten, Sitzung und Inhaltsabschluss

Private Quellenbelege erhalten sämtliche tatsächlich gelieferten, validierten
MLSD-Fakten, einschließlich präzisem `modify` und angebotenem `unique`. Die
angebotene Faktenauswahl wird mit dem optionalen `OPTS MLST` angefordert; eine
zulässige Protokollabsage bleibt toleriert. `CWD` und argumentloses `MLSD` werden
unter derselben gehaltenen Sitzung ausgeführt. Allgemeine Listing-/Sync-Tokens
werden durch diese private Sicht nicht verändert.

Die aktive Vorbereitung hält Server-/Kontoidentität und Sitzungsgeneration.
Ein Reconnect oder Identitätswechsel stoppt den laufenden Versuch. Der
persistierte Digest enthält die Generation dagegen nicht: Ein unveränderter
Baum muss nach einer frischen Verbindung denselben Umfang ergeben. Neue oder
veränderte Gruppen erzeugen in der Fensterqueue `SourceTreeChanged` vor der
ersten Zielwirkung und benötigen erneut Zustimmung.

Vor `RETR`, nach dessen erfolgreichem Transportabschluss und vor dem lokalen
Commit werden die beobachteten Quellvorfahren frisch geprüft. Auch ausgelassene
Mitglieder und ihre gehaltenen Vorfahren bleiben vor unabhängigen Wirkungen
driftgebunden. Der Stream prüft die angekündigte Länge mit Überlängenprobe;
verkürzte Daten, ein fehlerhafter Abschluss, Abbruch oder Sitzungsdrift liefern
keinen erfolgreichen lokalen Commit. Ein verlorener Versuch startet keinen
blinden zweiten `RETR`.

## Lokale Temp-Recovery

Queue-Läufe verwenden bevorzugt einen privaten dauerhaften Tempindex. Ein
Prozessabbruch nach Teilübertragung lässt die Queue beim Neustart als unterbrochen
stehen; bekannte Tempdateien bleiben konkrete Prüfpfade und starten keinen
zweiten Versuch automatisch. Fehlt ein exklusiver Queue-Namespace für diesen
Index, bleibt die normale Kopie möglich. Ihre Ergebnisbilanz benennt dann die
fehlende dauerhafte Temp-Recovery und mögliche ungebundene Nachbardateien nach
einem Prozessabbruch. Daraus entsteht kein Lösch- oder Wiederholungsrecht.

## Aufwand und Best-Effort-Grenze

Reine Navigation verwendet gehaltene Metadaten mit Abbruch-/Generationsprüfung.
Ordnergrenzen vergleichen ihr eigenes Listing frisch; nach den Kindern folgt
derselbe Schlusscheck. Dadurch werden die Vorfahren eines tiefen Baums nicht
für jeden Navigationsaufruf erneut vollständig gelistet. Die Inhaltsprüfungen
und der vorbereitete Vorfahrensatz ausgelassener Mitglieder bleiben erhalten.
Datei-/Auswahlzahl und Baumtiefe besitzen keine neuen pauschalen Produktgrenzen;
Segment-/Protokollvalidierung und Parsergrenzen gelten weiter. Baumbelege
benötigen Speicher proportional zum beobachteten Bestand.

FTP bietet kein `FSTAT` und keine atomare Pfadbindung. Eine Änderung mit
identischen gemeldeten Fakten, ein zwischen Requests umgeleiteter Pfad oder
unterschiedlicher Inhalt gleich konfigurierter Serverknoten kann unbemerkt
bleiben. Diese Grenze steht im vorhandenen GUI-/CLI-Lesehinweis. Der lokale
Temp-, No-Replace- und Abschlussvertrag bleibt erhalten; die Quellenklasse
erteilt keine neue Mirror-, Quarantäne- oder strikte Sync-Fähigkeit.
