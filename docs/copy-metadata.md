# Metadaten gewöhnlicher Kopien

Stand: 2026-11-01. Dateiaktionen verwenden einen eigenen Vertrag; erweiterte
Native-Syncprofile werden nicht still vorausgesetzt.

Rechte und darstellbare Änderungszeiten werden übernommen; lokale Kopien auf
macOS und Linux erhalten Änderungszeiten einschließlich Nanosekunden. Ziele mit
geringerer Auflösung melden die reduzierte Genauigkeit. Lokale macOS-Kopien
übernehmen zusätzlich die genaue Erstellungszeit, lesbare xattrs einschließlich
Tags und Finder-Informationen, Resource Forks, Verstecktmarkierung und Dateisperre.
Das gilt für Dateien und neue Ordner. Beim Zusammenführen behalten bestehende
Zielordner ihre eigenen Metadaten; ihre zusätzlichen Quell-Forks gelten als ausgelassen.

Quelle und Ziel werden über gehaltene Objekte geprüft. Die native Attributkopie
endet vor dem Readback; Daten- und Resource Fork werden getrennt vollständig
geprüft. Zeiten und einschränkende Rechte folgen danach. Eine Tempdatei gelangt
erst nach vollständigem Datenabschluss an den Zielnamen. Die Dateisperre folgt
zuletzt am gehaltenen Zielobjekt, weil gesperrte Temps nicht umbenennbar sind.
Scheitert dieser Abschluss unklar, bleiben Inhalt und Move-Quelle erhalten; das
Ergebnis nennt die Teilwirkung. Fehlende Flagunterstützung wird als Auslassung
gemeldet. Kein blinder Retry.

Fehlende Zielfähigkeiten lassen die Inhaltskopie mit konkretem Hinweis zu.
Nicht lesbare Attribute, Werte über 16 MiB und xattrs über insgesamt 32 MiB
werden bilanziert. Der native Kopierweg lässt Resource Forks über 4 GiB aus;
weitere Systemattribute können vom Betriebssystem angepasst werden. ACL- und
Ownerübertragung sowie Kompressions- und Spezialflagmigration werden nicht zugesagt.

Ein kopierendes Move behält die Quelle, wenn ihr Resource Fork am Ziel nicht
vollständig bestätigt ist. Vor dem Quellabbau werden die genaue Änderungszeit, native Quellmetadaten und
übertragene Forks erneut geprüft. Andere Fehler nutzen die vorhandenen Rückwege
mit sichtbarer Teil- und Recoverybilanz. Ausgelassene Forks und erhaltene Quellen
erscheinen direkt im Ergebnis; die Queue behält den Teilstatus nach Neustart,
die CLI liefert Exitcode 1. Ein Rename im selben Dateisystem erhält
die ursprünglichen Objekte unmittelbar.

Releaseumfang: [ForkLift-Vergleich](forklift-parity.md).
