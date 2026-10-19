# Produktprinzipien

Stand: 2026-10-18

## Identität

Doppeldecker ist zuerst ein praktischer Dual-Pane-Dateimanager. Erwartbare
Dateiarbeit soll lokal und über unterstützte Provider tatsächlich funktionieren.
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

- Anlegen, Umbenennen, Kopieren, Verschieben und Upload sollen als Best Effort
  verfügbar bleiben, wenn der Provider den normalen Protokollbefehl unterstützt und
  der angeforderte Bereich eindeutig gebunden ist.
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

SFTP-Inhalte besitzen eine eigene begrenzte interaktive Leseklasse: reguläre
Dateien als geprüfter lokaler Snapshot oder additive lokale Kopie; begrenzte
Verzeichnisbäume ausschließlich als additive lokale Kopie. Gehaltene
READ-Handles und vollständiger Inhalts-Readback begrenzen den Transfer, ersetzen
aber keine atomare Bindung des ersten OPEN beziehungsweise OPENDIR an den Pane-Root. Dieses Restrisiko
bleibt sichtbar; striktes Sync-VFS und Remote-Mutationen übernehmen die Freigabe
nicht. Grenzen und Recovery: [SFTP-Inhalte](sftp-contents.md).

Gewöhnliche lokale Dateiadditionen nach WebDAV verwenden eine private bedingte
Best-Effort-Zielsicht ohne künstliche Collection-/Löschprobe. Gehaltene Quellen,
vollständige Inhaltsprüfungen, Tempintents und Readback bleiben erforderlich;
begrenzte Baumuploads verwenden denselben Vertrag; Ersetzungen und strikter Sync
übernehmen die Freigabe nicht.
[Vertrag und Grenzen](webdav-upload.md).

Eine vollständig atomare WebDAV-Elternbindung ist durch mehrere getaggte
`If`-Listen nicht belegt: Diese gelten standardgemäß alternativ. Gewöhnliches
lokales Dateiersetzen erhält daher keine Freigabe für eine scheinbare gemeinsame
Bindung von Temp, Altziel und allen Eltern. Additionen, mkdir und Rename binden genau den unmittelbaren Elternordner;
Vorfahrenwechsel bleiben sichtbar Best Effort und melden beobachtete Drift als unklar.
Strikte WebDAV-Proben erzeugen ohne gemeinsame Root-/Elternbindung keinen Bestand.
[Mutationsvertrag](webdav-mutation-boundary.md).
[Tragfähigkeitsgrenze](webdav-replace-boundary.md).

### Löschen und Ersetzen

- Ein nativer Papierkorb, Versionierung oder Doppeldecker-Quarantäne ist der
  bevorzugte Weg.
- Fehlt ein Wiederherstellungsweg, darf eine klar als dauerhaft bezeichnete
  Operation angeboten werden. Umfang, Ziel und Irreversibilität müssen vorab
  sichtbar sein; größere Wirkungen verlangen eine stärkere Bestätigung.
- Unvollständige Listings oder unklare Zielidentität sperren rekursive Entfernungen,
  nicht aber unabhängige Lese- oder Additionsfunktionen.

### Synchronisation

- Der normale lokale AddOnly-/Update-Start bindet Roots und Plan im Backend und
  bestätigt die konkrete Wirkung. Reine Additionen brauchen keine Zufallsphrase
  oder Quarantäneprobe. Ersetzungen verwenden bevorzugt Quarantäne; eine begrenzte
  schwache Zielklasse bestätigt Zahl, Ziele und fehlende Wiederherstellung
  ausdrücklich als dauerhafte Ersetzung. ContentHash und die Grenze von 100
  Ersetzungen bleiben erhalten. Datei- und Verzeichnis-Flushzusagen werden getrennt
  vom atomaren Neunamencommit bewertet und im bestätigten Backendplan gebunden.
  Simulation und echte Ausführung sind getrennte Starts. Vertrag und verbleibende
  lokale Zielgrenzen: [Normaler lokaler Sync](local-sync.md).
- AddOnly, Update, Mirror und bidirektionaler Sync werden pro geplanter Wirkung
  bewertet. Eine schwache Zielgarantie darf nicht automatisch alle Richtungen und
  alle Aktionen sperren.
- Additionen und bestätigte Einzelersetzungen können Best Effort laufen, wenn
  Vorschau, frischer Bestandscheck, Readback und Audit vorhanden sind.
- Mirror und bidirektionale Löschpropagierung benötigen wegen ihres großen
  Wirkungsradius weiterhin vollständige Listings, sichtbare Mengengrenzen und eine
  eindeutige Bestätigung. Fehlt nur für einzelne Aktionen eine starke Garantie,
  soll Doppeldecker den ausführbaren Teil klar vom ausgelassenen Teil trennen, statt
  einen Scheinerfolg oder eine pauschale Absage zu liefern.
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

- Mutation außerhalb des ausdrücklich gewählten Roots,
- Geheimnisse in Argumenten, URLs, Journalen oder Auditdaten,
- stillen Überschreibungen oder Löschungen,
- erfundene Vollständigkeit, Dauerhaftigkeit oder Wiederherstellbarkeit,
- mutierenden Blind-Retry nach einem Verbindungsabbruch,
- Erfolgsmeldung ohne passenden Readback beziehungsweise ehrliche Teilbilanz,
- Fortsetzung nach einem unerwarteten Wechsel von Konto, Server, Share, Zertifikat,
  Hostkey oder Root.

## Produkt- und Entwicklungsregel

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
