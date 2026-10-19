# Konflikte vergleichen und entscheiden

Stand: 2026-10-18. Verbindlicher Bedien- und Backendvertrag für Sync sowie
interaktives Kopieren und Verschieben.

## Vergleich und Rollen

Das Backend liefert den verlustfrei darstellbaren relativen Namen, beide Endpunkte,
Vorhandensein, Typ, Dateigröße und Änderungszeit. Die bidirektionale Ansicht stammt
aus den gehaltenen vollständigen Snapshots. Interaktive Zielkonflikte verwenden
dieselben vollständigen Listings und Identitäts-/Inhaltsbelege wie die
Ersetzungsbindung. Die begrenzte SFTP-Inhaltsquelle bindet stattdessen ausschließlich
ihre ausgewählten regulären Inhalte samt vollständigen Hashes und bei Ordnern
den beobachteten begrenzten Namensraum; daraus entsteht kein striktes
vollständiges Elternlisting oder Ersetzungsrecht. Sie bietet nur Überspringen und
zulässige lokale Konfliktkopien. [SFTP-Vertrag](sftp-contents.md).
Ordner besitzen hier keine behauptete Inhaltsgröße. Fehlende Größe oder Zeit wird
als unbekannt angezeigt; eine bekannte Dateigröße von null bleibt null.

Der gemeinsame Dialog stellt Quelle/Ziel beziehungsweise Links/Rechts in derselben
Struktur gegenüber. Mehrfachkonflikte haben aufklappbare Vergleiche. Lange Namen und
Endpunkte umbrechen, der Inhalt scrollt bei kleinen Fenstern und die Abbruchaktion
bleibt erreichbar. Tastatur, Escape, Fokusfalle und Fokusrückgabe folgen dem
bestehenden Modalvertrag. Beide Themes verwenden die vorhandenen Farben und Abstände.

Nach einem Pane-Tausch ordnet das Frontend ausschließlich sichtbare Rollen neu zu.
Paarordnung, Digests, Konfliktpfade und ausführbare Entscheidungen bleiben im Core.

## Bidirektionale Entscheidungen

Die Vorschau enthält die konkret auflösbaren Entscheidungen. Der Core berechnet
nach jeder Einzelwahl die restlichen Optionen erneut aus denselben Snapshots;
frühere Entscheidungen zählen bei den bestehenden Mengengrenzen mit.

- Eine einseitige Erstbaseline-Differenz erhält kein ausführbares Skip. Der Dialog
  erklärt, warum sie nicht dauerhaft als offener Erstkonflikt gespeichert werden kann.
- Eine fehlende Gewinnerseite bedeutet eine Entfernung der vorhandenen Gegenseite
  in Quarantäne. Bestehende Löschgrenzen können diese Wahl mit ihrem konkreten Grund
  deaktivieren. Ein Typwechsel gilt ausdrücklich für den vollständigen Unterbaum.
- Beide behalten steht nur für zwei vorhandene, strukturell materialisierbare
  Versionen zur Verfügung. Der Core vergibt den Konfliktnamen vor der Wahl und zeigt
  ihn an. Beide Konfliktkopien laufen vor der Änderung am Originalpfad.
- Mit bestehender Baseline erhält Skip weiterhin die alte gemeinsame Wahrheit für
  den betroffenen Unterbaum; Neustart und erneutes Öffnen ändern das nicht.

Die erneute Vorbereitung vergleicht ihren ungeklärten Plan mit dem Digest des
ursprünglichen Konfliktdialogs. Geänderte Bäume oder Baselines verlangen Neuplanung,
statt alte Entscheidungen auf neue Versionen anzuwenden. Abbruch in einem späteren
Dialog verwirft alle noch nicht ausgeführten Entscheidungen. Verspätete Antworten
werden nur im weiterhin zugehörigen Bedienzustand angezeigt.

## Beide behalten bei Copy und Move

Für eine kollidierende Auswahl vergibt der Core neue Namen im gebundenen Zielordner.
Bei einem kollidierenden ausgewählten Ordner wird dessen vollständiger Quellbaum
daneben übertragen. Bereits vorhandene Zielinhalte bleiben erhalten. Die üblichen
Erweiterungen bleiben erhalten; lange Namen werden an UTF-8-Zeichengrenzen gekürzt.
Extrem lange Erweiterungen teilen sich das begrenzte Namensbudget mit dem Stamm.
Namensprüfung berücksichtigt Groß-/Kleinschreibung und Unicode-Normalisierung;
der Commit verhindert zusätzlich jede tatsächliche Dateisystemkollision exklusiv.

Der Dialog zeigt die neuen Namen relativ zum bereits verglichenen Zielordner. Das Frontend erhält eine kurzlebige fenstereigene
Vorschau-ID. Es konstruiert keine ausführbaren Aliasziele. Die ID bindet Auswahl,
Endpunkte und originale Konfliktbelege. Geänderte Versionen oder eine abgelaufene
Vorschau werden vor dem Einreihen abgelehnt. Eine ältere, langsamer fertig werdende Anfrage kann die Bindung einer neueren
Vorschau nicht ersetzen. Die reine Anzeigebindung ist weder ein
Permit noch eine Fenster- oder Exit-Sperre.

Journalformat 22 persistiert die vergebenen Aliasziele und Quellbelege. Am Queuekopf
werden alle Namen und Belege frisch geprüft. Beide behalten benötigt einen
abgesicherten No-Replace-Commit; fehlt er, erklärt der Dialog die fehlende Variante.
Die übrigen vorhandenen Copy-/Move-Ausführungsklassen bleiben erreichbar.
Eine Kollision nach der Vorschau oder während des Commits verlangt Neuplanung und
erlaubt niemals Ersetzen oder ein stilles Zusammenführen am neuen Namen. Das gilt
auch für ein später fremd angelegtes Kind im neuen Konfliktordner. Kann der
Rückweg wegen dieses fremden Inhalts nicht vollständig erfolgen, bleiben Quelle,
fremder Inhalt und Recoverypfade samt unklarem Teilausgang sichtbar.

Copy und Move verwenden weiterhin die vorhandenen Abbruch-, Readback-, Audit- und
Recoveryverträge. Nach Teilwirkung bleiben Quellen und Prüfpfade gemäß dieser
Bilanz sichtbar. Die native Zwischenablage wird nur nach vollständig erfolgreichem
Move aller Gruppen verbraucht. Ein Paste in einem anderen Fenster wartet auf eine
noch ausstehende eigene Dateiübergabe; eine fehlgeschlagene Übergabe darf dabei
nicht die vorherige Auswahl einfügen. Dafür gibt es keine zusätzliche Fenstersperre.

## Vorschau, Simulation und Ausführung

Vorschau vergleicht ausschließlich. „Testlauf simulieren“ verändert keine Dateien,
auch in einer validierten Testsandbox. Der [normale lokale AddOnly-/Update-Start](local-sync.md)
bereitet eine echte, wirkungsbezogen bestätigte Ausführung vor. Mirror behält den
getrennten Realmodus und seine bestehenden Bestätigungen. Remote- und bidirektionale Starts
kündigen die Vorbereitung einer echten Ausführung an, die erst nach der gebundenen
Bestätigung beginnt. Startstatus und Abschluss benennen Simulation oder echte Wirkung.

Dieser Vertrag erweitert weder Mirror noch Providerfähigkeiten und führt für
gewöhnliche Additionen keine weitere Bestätigungsphrase ein.
