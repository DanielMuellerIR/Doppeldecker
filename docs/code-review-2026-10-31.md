# SMB-Readback und exakte Objekt-IDs

Stand: 2026-10-31. Zwei Befunde aus dem Delta zu den nativen SMB-Metadatenverträgen.

## Erkannte Drift beim Zeitabschluss erhalten

Ein neuer leerer SMB-Ordner hält zunächst sein CREATE-Receipt. Nach bestätigtem
Zeit-SET darf bei einer unklaren lesenden Beobachtung das Receipt aufgegeben und
rein lesend erneut geprüft werden. Dieser Weg durfte bisher auch eine bereits
beobachtete fremde Objekt-ID übergehen, wenn der spätere Readback wieder passte.

Der zweite Readback bleibt auf unklare Beobachtungen ohne Closefehler begrenzt.
Bekannte Objekt- oder Zeitabweichungen und Abbruch eröffnen diesen Weg nicht.
Insbesondere bleibt `PlanDrift` unverändert erhalten. Ein SET wird nie wiederholt.

Der echte Samba-Queuegegenfall tauscht den erzeugten Ordner nach dem Zeit-SET
physisch gegen ein anderes Objekt aus. Der erste SMB-Readback beobachtet dessen
fremde ID. Anschließend wird das Original zurückgesetzt; dessen Zeit-/ID-Readback
passt wieder. Trotzdem muss der Auftrag mit `PlanDrift` enden. Der gewöhnliche
Metadatenabschluss bleibt unbestätigt und unabhängige Folgeinhalte bleiben aus.

## Große V7-IDs verlustfrei anzeigen

V7 speichert `u64`-Objekt-IDs weiterhin numerisch. JavaScript kann benachbarte
Zahlen oberhalb 2^53 nicht allgemein unterscheiden. Nach einem Abbruch und einer
frisch bestätigten erneuten Anlage desselben Pfads konnte die Anzeige deshalb
zwei verschiedene Objekte zusammenfassen.

Eine eigene Tauri-Anzeigeprojektion wandelt ausschließlich die IDs der
Neuordnerberichte vor dem JavaScript-Parser in Dezimalzeichenfolgen um.
Sie umfasst Queueantworten, fenstergebundene Ereignisse und direkte strukturierte
Command-Fehler einschließlich verschachtelter Fortschrittsberichte.
V7-Journale, Digestverträge, CLI und Audit behalten ihre bisherige Serialisierung.
V8 verwendet bereits seine eigene exakte Stringform.

Der Browsergegenlauf verwendet die vom Rust-Serializer erzeugten IPC-Daten mit
`9007199254740992`, `9007199254740993` und `18446744073709551615`.
Zwei Anlagen desselben Pfads, davon eine mit unbestätigten gewöhnlichen Metadaten,
bleiben in Queue und Fortschritt getrennt. Die alte numerische Form reproduziert
das Zusammenfallen auf eine Anlage in beiden Themes.

## Prüfstand

Auf macOS sind Core, App, CLI, Workspace-Clippy, gesperrter Fuzz-Lockstand und
Formatprüfung bestanden. Die vollständige Browser-Suite verwendet die erzeugte
Rust-Fixture; helle und dunkle Queueansichten sind anhand von Screenshots visuell
geprüft. Unter Linux sind der vollständige Corelauf, App und CLI sowie die echte
Samba-V7-Matrix mit 26 Fällen einschließlich des neuen Austauschgegenfalls
bestanden. Der Gegenlauf mit ausschließlich wiederhergestelltem altem
Readback-Zweig scheitert erwartungsgemäß: derselbe echte Objektwechsel führt dort
nach Rücktausch zu einem erfolgreichen Sync. Workspace-Clippy ist auf beiden Plattformen ohne Warnungen bestanden.

Die Browserläufe verwenden Originaloberfläche und kontrollierte Tauri-Brücke.
Die Rust-Fehlerprüfung nutzt die tatsächliche Tauri-IPC-Fehlerkonvertierung; der
Samba-Lauf verwendet die Core-Produktqueue. Ein gemeinsamer nativer
Tauri-/SMB-GUI-Lauf ist damit nicht belegt. Die Prüfungen begründen keine
Veröffentlichung, neue Metadatenfähigkeit oder allgemeine Serverfreigabe.
