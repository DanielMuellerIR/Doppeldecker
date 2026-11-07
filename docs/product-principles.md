# Produktprinzipien

Stand: 2026-10-20

## Identität

Doppeldecker soll ein komfortabler, umfassender, performanter und effizienter
Two-Pane-Dateimanager für macOS und Linux sein. Erwartbare Dateiarbeit soll lokal
und über unterstützte Provider tatsächlich funktionieren. Klare Bedienung, kurze
Arbeitswege, reaktionsfähige Ansichten und ein angemessener Aufwand für CPU, Speicher
und Netzwerk gehören zum Produktziel.
Datenintegrität, Transparenz und Wiederherstellbarkeit erhöhen den Nutzwert; sie
dürfen nicht als pauschale Begründung dienen, eine übliche Funktion wegzulassen.

Die Welt der Dateisysteme und Server ist nicht einheitlich. Doppeldecker verwendet
deshalb die stärkste nachweislich verfügbare Ausführung, benennt schwächere Garantien
ehrlich und lässt den Nutzer bei verbleibenden, verständlich erklärbaren Risiken
entscheiden. Eine nicht perfekte, aber gut begrenzte und sichtbare Operation ist
meist nützlicher als eine vollständige Verweigerung.

## Drei Ausführungsklassen

Jede Wirkung wird einer der folgenden Klassen zugeordnet. Fähigkeiten sind damit
abgestuft und nicht nur ein boolesches „erlaubt oder gesperrt“.

1. **Abgesichert:** Der Provider belegt die benötigten atomaren Operationen,
   Identitäten, vollständigen Listings und gegebenenfalls Quarantäne- oder
   Restore-Fähigkeiten. Die Operation läuft im normalen Weg.
2. **Best Effort:** Der Provider unterstützt die fachliche Operation, aber nicht
   jede lokale Garantie. Doppeldecker prüft den aktuellen Bestand unmittelbar
   vorher, zeigt die konkrete Einschränkung, fragt bei möglichem Datenverlust
   passend zur Wirkung nach, führt genau den bestätigten Befehl aus und liest das
   Ergebnis danach zurück. Eine mutierende Operation wird nach einem
   Verbindungsabbruch nie blind wiederholt; ein unklarer Ausgang wird als solcher
   gemeldet.
3. **Nicht ausführbar:** Doppeldecker kann Ziel, Bereich oder Wirkung nicht
   verlässlich begrenzen, die Serveridentität ist unerwartet gewechselt, ein für
   eine Entfernung nötiges Listing ist unvollständig, Secrets wären gefährdet oder
   der Nutzer könnte die tatsächliche Wirkung nicht sinnvoll bestätigen. Die
   Absage nennt die konkrete Ursache und sperrt nur die davon abhängige Wirkung.

Fehlendes `fsync`, fehlendes atomisches No-Replace-Rename oder fehlende
Providerquarantäne reichen für sich allein nicht mehr für eine pauschale Absage.
Sie bestimmen Ausführungsklasse, Rückfragen, Audit und verfügbare
Wiederherstellungswege.

## Wirkungsspezifische Leitlinien

### Interaktive Dateioperationen

Gewöhnliches Kopieren, Verschieben, Ordneranlegen und Umbenennen wird direkt aus
den Panes gestartet. Eine allgemeine Vorschaupflicht oder ein Sicherheitsassistent
vor jeder Dateiaktion gehört nicht zum Produktziel. Rückfragen betreffen konkrete
Kollisionen, Löschungen oder eine erklärungsbedürftige Wirkung. Eine optionale
Vorschau darf die Arbeit unterstützen; bei Sync und komplexen Stapelaktionen ist
sie ein eigener Bestandteil des jeweiligen Ablaufs. Das ist keine Zusage, dass
jede Operation unter allen Umständen ohne Datenverlust gelingt.

- Anlegen, Umbenennen, Kopieren, Verschieben und Upload sollen als Best Effort
  verfügbar bleiben, wenn der Provider den normalen Protokollbefehl unterstützt und
  der angeforderte Bereich entsprechend der Ausführungsklasse geprüft ist.
  Eine atomare Elternbindung ist kein allgemeines Eintrittskriterium; die
  ausdrücklich akzeptierte SFTP-Ausnahme steht unten.
- Vorhandene Zielnamen werden unmittelbar vorher ermittelt. Doppeldecker bietet
  Ersetzen, Überspringen, Beide behalten oder Abbrechen entsprechend der
  tatsächlich verfügbaren Wirkung an. Es gibt kein stilles Überschreiben.
- Kann ein Provider eine späte Kollision nicht atomar verhindern, zeigt die UI
  diesen begrenzten Wettlauf verständlich an. Nach der Operation bestätigt ein
  Readback Name, Typ und – soweit verfügbar – Inhalt beziehungsweise Identität.
- Ein serverseitig möglicher Rename darf nicht allein deshalb verschwinden, weil er
  nicht zugleich als atomarer Sync-Commit taugt.

Beide behalten verwendet backendseitig vergebene, vorab sichtbare neue Namen und
atomaren No-Replace-Commit. Ein Ordner wird dabei vollständig daneben übertragen.
Die konkrete Variante bleibt bei fehlender Garantie mit Grund deaktiviert; normale
Copy-/Move-Wege behalten ihre vorhandenen Ausführungsklassen. Die UI erfindet keine
Ausführungspfade oder Planbelege. Details: [Konfliktentscheidungen](conflict-decisions.md).

SFTP-Inhalte verwenden eine interaktive Best-Effort-Lesesicht. Normale Snapshots
lesen jeden Inhalt einmal remote, bilden den Transferhash und prüfen die
abschließenden Attribute. Eine zusätzliche vollständige Verifikation ist optional.
Dateigröße, Auswahlzahl, Baumtiefe und Gesamtbytes sind keine festen Produktgrenzen;
Speicher-, Parser- und Requestgrenzen bleiben sichtbar. „Passiv ansehen“ erhält seine
entschärfte Formatbehandlung; bewusstes Öffnen und Bearbeiten erhält das Originalformat.
Explizites Zurückspeichern prüft das Serveroriginal und verwendet die normale Queue.
Der erste pfadbasierte Zugriff bleibt ohne atomare Elternbindung.
[Vertrag](sftp-contents.md).

Für gewöhnliche SFTP-Schreiboperationen ist seit 2026-10-20 ausdrücklich eine eigene
Best-Effort-Klasse umgesetzt: Benutzerpfade bleiben lexikalisch unter dem gewählten
Root, beobachtete Symlinks werden abgelehnt, Bestand und Ergebnis werden passend
zur Wirkung geprüft. Standard-SFTP v3 kann einen gleichzeitig serverseitig
umgeleiteten Elternpfad jedoch nicht atomar ausschließen. Dieses Restrisiko ist
akzeptiert und darf normale Uploads, mkdir, Rename, Move sowie bestätigtes Ersetzen
und Löschen nicht pauschal verhindern. Der Vertrag verspricht keine atomare
Rootgarantie. Umsetzung und Prüfstand stehen im
[Schreibvertrag und Stand](sftp-writes.md). Strikter Sync erhält dadurch keine
pauschale Freigabe.

Gewöhnliche WebDAV-Uploads, Ersetzungen, mkdir, Rename und dauerhaftes Löschen
verwenden eigene Best-Effort-Verträge ohne künstliche Fähigkeitsproben. Lokale
Quellen werden während des Transfers gehasht; ein vollständiges Vorhashen ist kein
Eintrittskriterium. WebDAV-`If`-Listen werden standardgemäß alternativ ausgewertet,
niemals als gemeinsame atomare Bindung sämtlicher Eltern ausgegeben. Ersetzungen
binden das Altziel und verwenden eine fertig übertragene Tempdatei; eine fehlende
Quarantäne wird ausdrücklich angezeigt. Strikter Sync behält seinen eigenen Vertrag.
[Uploadvertrag](webdav-upload.md), [Mutationsvertrag](webdav-mutation-boundary.md).

### Löschen und Ersetzen

- Ein nativer Papierkorb, Versionierung oder Doppeldecker-Quarantäne ist der
  bevorzugte Weg.
- Fehlt ein Wiederherstellungsweg, darf eine klar als dauerhaft bezeichnete
  Operation angeboten werden. Umfang, Ziel und Irreversibilität müssen vorab
  sichtbar sein. Erst mehr als 1000 Einträge oder 10 GiB bei endgültigem Löschen
  verlangen zusätzlich eine exakte Phrase; Quarantäneleeren behält seine Phrase.
- Unvollständige Listings oder unklare Zielidentität sperren rekursive Entfernungen,
  nicht aber unabhängige Lese- oder Additionsfunktionen.

### Synchronisation

- Der normale lokale AddOnly-/Update-Start bindet Roots und Plan im Backend und
  bestätigt die konkrete Wirkung. Reine Additionen brauchen keine Zufallsphrase
  oder Quarantäneprobe. Ersetzungen verwenden bevorzugt Quarantäne; eine begrenzte
  schwache Zielklasse bestätigt Zahl, Ziele und fehlende Wiederherstellung
  ausdrücklich als dauerhafte Ersetzung. ContentHash bleibt erforderlich;
  eine feste Grenze von 100 Ersetzungen besteht nicht. Datei- und Verzeichnis-Flushzusagen werden getrennt
  vom atomaren Neunamencommit bewertet und im bestätigten Backendplan gebunden.
  Simulation und echte Ausführung sind getrennte Starts. Vertrag und verbleibende
  lokale Zielgrenzen: [Normaler lokaler Sync](local-sync.md).
- AddOnly, Update, Mirror und bidirektionaler Sync werden pro geplanter Wirkung
  bewertet. Eine schwache Zielgarantie darf nicht automatisch alle Richtungen und
  alle Aktionen sperren.
- Additionen und bestätigte Einzelersetzungen können Best Effort laufen, wenn
  Vorschau, frischer Bestandscheck, Readback und Audit vorhanden sind.
- Mirror und bidirektionale Löschpropagierung benötigen wegen ihres großen
  Wirkungsradius weiterhin vollständige Listings, sichtbare Mengenwarnungen und eine
  eindeutige Wirkungsbestätigung. Auch eine bewusst leere Quelle ist zulässig;
  Scanfehler werden niemals als leere Quelle gewertet. Fehlt nur für einzelne Aktionen eine starke Garantie,
  soll Doppeldecker den ausführbaren Teil klar vom ausgelassenen Teil trennen, statt
  einen Scheinerfolg oder eine pauschale Absage zu liefern.
- Normale Sync-Starts verwenden einen Wirkungsdialog ohne Zufallsphrase,
  externe Backupbehauptung oder zusätzliche Auswahl zwischen Direktstart und Queue.
  Backendbelege, Driftprüfung und einmalige Permits bleiben erhalten. Git- und
  HOME-Ziele erhalten konkrete Warnungen; `/` bleibt als Ganzbaumziel gesperrt.
- Ein Sync ist weiterhin kein Backup. Wo keine Quarantäne verfügbar ist, muss die
  fehlende Wiederherstellbarkeit Teil der Vorschau und Bestätigung sein.

### Metadaten und Spezialfälle

- Inhalte und darstellbare Metadaten werden übertragen; nicht darstellbare Klassen
  werden pfadgenau bilanziert. Das Best-Effort-Profil ist ein regulärer,
  ausdrücklich gewählter Arbeitsmodus und keine Fehlersituation.
- Ein strikter Modus darf vollständige Darstellbarkeit verlangen. Seine strengeren
  Regeln dürfen aber nicht unbemerkt den normalen praktischen Standard ersetzen.

## Nicht verhandelbare Grenzen

Best Effort bedeutet nicht „blind weitermachen“. Doppeldecker akzeptiert weiterhin
keine:

- bewusst außerhalb des ausdrücklich gewählten Roots adressierte Mutationen;
  lokal bleiben Root und Eltern descriptor-relativ gebunden. Für die oben
  beschriebene interaktive SFTP-Klasse gilt die ausdrücklich akzeptierte Ausnahme:
  lexikalische Pfadgrenze und Symlinkprüfung statt atomarer Rootgarantie bei
  gleichzeitiger serverseitiger Umleitung,
- Geheimnisse in Argumenten, URLs, Journalen oder Auditdaten,
- stillen Überschreibungen oder Löschungen,
- erfundene Vollständigkeit, Dauerhaftigkeit oder Wiederherstellbarkeit,
- mutierenden Blind-Retry nach einem Verbindungsabbruch,
- Erfolgsmeldung ohne passenden Readback beziehungsweise ehrliche Teilbilanz,
- Fortsetzung nach einem unerwarteten oder ungeprüften Identitätswechsel von Konto,
  Server, Share, Hostkey oder Root. Bei öffentlichem FTPS bestätigt die Prüfung von
  Zertifikatskette, Gültigkeit und Hostname die Identität; eine dabei gültige
  Zertifikatserneuerung ist kein unerwarteter Serverwechsel. Bewusst gewählte
  Pin-Modi verlangen weiterhin den gebundenen Pin. Bereits vorbereitete Läufe
  behalten ihre eigene Sitzungs- und Planbindung.

## Produkt- und Entwicklungsregel

Zusätzliche Prüfungen müssen einen konkreten Nutzen für die betreffende Wirkung
haben. Sie dürfen gewöhnliche Dateioperationen weder unnötig blockieren noch durch
vermeidbare Scans, doppelte Übertragungen oder Rückfragen unverhältnismäßig
verteuern. Stärkere optionale Modi ersetzen den praktischen Standard nicht.
Bestehende Sync- und Recovery-Verträge gelten bis zu ihrer gezielten, geprüften
Änderung; ihre heutigen Sperren sind keine dauerhaften Produktziele.

Bei einer fehlenden Fähigkeit lautet die erste Frage nicht „Wie sperren wir die
Funktion?“, sondern:

1. Welche fachliche Wirkung kann der Provider tatsächlich ausführen?
2. Welche stärkste Absicherung ist verfügbar?
3. Welches Restrisiko bleibt, wie groß ist sein möglicher Schaden und kann der Nutzer
   es verständlich bestätigen?
4. Wie prüfen und bilanzieren wir das beobachtete Ergebnis?

Eine vollständige Absage ist die letzte, nicht die erste Stufe. Frontend, CLI und
Core müssen dieselbe Ausführungsklasse und dieselbe konkrete Einschränkung zeigen.
Vergleiche mit anderen Dateimanagern dienen ausschließlich dazu, erwartbare
Arbeitsabläufe und Qualitätslücken zu erkennen. Implementierung, Texte,
Gestaltungssystem, Assets und technische Lösungen bleiben eigenständig.
