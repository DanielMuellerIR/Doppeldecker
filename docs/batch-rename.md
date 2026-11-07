# Stapelumbenennung

Stand: 2026-11-01.

„Befehle → Stapelumbenennen …“ und das Dateikontextmenü bearbeiten die Auswahl
der aktiven Pane. Unterstützt werden lokale, SFTP-, FTP-/FTPS-, WebDAV- und
SMB-Einträge. Einzelumbenennen behält seinen eigenen Providervertrag.

## Regeln und Vorschau

- Suchen/Ersetzen arbeitet wörtlich und case-sensitiv. Präfix und Suffix folgen
  danach; die letzte Dateiendung regulärer Dateien bleibt auf Wunsch erhalten.
- Optional werden äußere gewöhnliche Leerzeichen entfernt. Innere Leerzeichen,
  Tabs und Unicode-Schreibweise bleiben erhalten.
- Nummerierung verwendet Start, positiven Schritt und Mindeststellen. Jede
  sichtbare Auswahlzeile behält ihre Nummer, auch bei Fehler oder Auslassung.
- Die Vorschau folgt der Pane-Reihenfolge und zeigt Altname, Zielname, Blockaden,
  Ausführungsklasse und Grenzen. Nur das Backend bildet und bestätigt Namen.
  Änderungen an Eingaben, Pane oder Profil entwerten die Vorbereitung.
- Symlinks, Spezialdateien, unlesbare Namen und ungültige Namen werden ausgelassen.
  Kollisionen werden nach NFC-Normalisierung und Kleinschreibung geprüft;
  mehrere äquivalente Ziele bleiben gesperrt.

Ein vollständiger frischer Bestand bindet die Auswahl. Ein nicht ausgewählter,
unveränderter oder blockierter Eintrag gibt seinen Namen nicht frei. Ketten
laufen in der nötigen Reihenfolge: Bei Präfix `a` und Auswahl `a`, `aa` wird
zuerst `aa → aaa`, dann `a → aa` ausgeführt. Die sichtbare Reihenfolge bleibt gleich.

## Namenstausch und Schreibweise

Lokale exakte Zweierzyklen verwenden einen atomaren Exchange, auch für nichtleere
Ordner. Längere lokale Zyklen verwenden mehrere Exchanges ohne Zwischenname.
Der gesamte Mehrschrittzyklus ist nicht atomar. Nach jedem Schritt werden
Identitäten und Elternsync geprüft; Abbruch löst keinen automatischen Rücktausch aus.

Entfernte geschlossene **Datei- und Ordnerzyklen** verwenden einen zufälligen freien Namen
`.doppeldecker-rename-…`. Ein Zyklus mit N Einträgen benötigt N+1 gewöhnliche
Rename-Schritte. Vorschau und Ergebnis nennen Zwischenname und Teilwirkungen.
Ordner werden samt ihrem Unterbaum umbenannt; Inhalte werden nicht vorab gehasht.
Case-/Unicode-mehrdeutige Gruppen und beobachtete gleiche Objektidentitäten
innerhalb eines Zyklus bleiben blockiert.

Reine Case-/Unicode-Schreibweisenänderungen verwenden einen eigenen einzelnen
Provider-Rename ohne Ersetzen. Die vollständige Namensgruppe muss eindeutig
sein; nur der exakte neue Servername mit passender Identität beziehungsweise
beobachtbaren Attributen bestätigt den Erfolg. Der Server darf die Schreibweise
verweigern. Es gibt keinen überschreibenden Ausweichweg.

## Ausführung und Teilbilanz

Eine Vorbereitung gilt zehn Minuten und nur im verursachenden Fenster. Handles,
Sitzungen, Quellbelege und Abbruchtoken bleiben im Backend-RAM. Jede Einzelwirkung
prüft Profil, Sitzung, Root, Eltern und Quelle frisch und verwendet den vorhandenen
Renamekoordinator. Große Inhalte werden dafür weder geöffnet noch vorab gehasht.

| Provider | Einzelwirkung und Grenze |
| --- | --- |
| Lokal | Descriptor-relativer No-Replace-Rename beziehungsweise Exchange; Identitätsreadback und Elternsync. |
| SFTP | Ein Standard-RENAME mit Zielcheck und Attributreadback; pfadbasierter Best Effort. Keine stabile Objekt-ID oder atomare Elternbindung. [Vertrag](sftp-writes.md). |
| FTP/FTPS | Eine RNFR/RNTO-Folge mit Zielcheck und MLSD-Rückleseprüfung. Ein spätes Ziel kann bei überschreibendem Server-RNTO verloren gehen. FTPS ändert diese Namensgarantie nicht. |
| WebDAV | Ein bedingtes MOVE mit `If-Match`, Elternbedingung und `Overwrite: F`; exakter Nachbestand. Nur belegte eigene Collection-ETag-Änderungen werden übernommen. [Vertrag](webdav-rename.md). |
| SMB | Serverseitiger No-Replace-Rename mit FileID-Readback. Das schützt das Ziel, friert Quellinhalte aber nicht ein. Ohne Directory-FLUSH keine zugesicherte Dauerhaftigkeit. |

Der Stapel ist keine Transaktion. Die Bilanz trennt erreichte Ziele, bestätigte
Zwischennamen, unveränderte Fehler, unklare Ausgänge und abgebrochene Restzeilen.
Eine abhängige Zeile beginnt erst nach bestätigtem Vorgänger. Klare Kollisionen
lassen unabhängige Zeilen weiterlaufen; Bindungswechsel und unklare Wirkungen
stoppen den Rest. Eine verlorene Antwort wird niemals mutierend wiederholt.
Passende spätere Namen heilen keinen vorherigen unklaren Readback.

## Dauerhafte Recovery entfernter Zyklen

Vor dem ersten Schritt speichert die Fensterqueue Zuordnung und Zwischenname.
Vor jedem Rename steht dessen Absicht dauerhaft im Journal; danach wird der
bestätigte Name fortgeschrieben. Gemeinsame gebundene Intents verhindern, dass
andere Fenster offene Zwischenstände in einen Sync übernehmen. Das gilt auch
für Sync-Unterwurzeln innerhalb eines ursprünglichen oder temporären Ordners.

Nach Abbruch oder Neustart zeigt die Queue Altname, Zielname, zuletzt bestätigten
Namen und gegebenenfalls den unklaren Einzelschritt. Der Eintrag ist ausschließlich
zur manuellen Prüfung bestimmt: kein Retry, automatisches Fortsetzen, Rücktauschen
oder Löschen. Manuelles Schließen verändert den Server nicht. Ein erhaltener
Zwischenname bleibt für Sync gesperrt, bis er bewusst außerhalb beider Wurzeln
gesichert oder sein Fehlen frisch belegt ist.
[Queuevertrag](operations-queue-design.md).

## Fenster und Abbruch

Jedes Fenster hält seinen eigenen Plan, seine Single-Flight-Schranke und seinen
exakten Abbruchtoken. Andere Fenster können parallel arbeiten. Escape schließt
die Vorschau; während der Ausführung stoppt es vor der nächsten Einzelwirkung.
Eine bereits begonnene Wirkung wird noch bilanziert. Profil-/Panewechsel und
Fensterschließen verwerfen wartende Pläne und stoppen nur den zugehörigen Lauf.
Beim Schließen kehrt der Fokus zur Pane zurück. Gewöhnliche Stapelpläne werden
nach Prozessende nicht erneut ausgeführt.

## Prüfstand

Reale Providergegenläufe und Fehler-/Recoveryfälle sind auf Core-Ebene geprüft.
Die native macOS-SFTP-Abnahme bestätigt geschlossene Dreierzyklen für Dateien und
nichtleere Ordner: alle Inhalte am erwarteten Namen, keine Zwischennamen übrig.
