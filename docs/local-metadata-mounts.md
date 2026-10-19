# Lokale Metadatenfähigkeiten je Mount

Stand: 2026-10-18.

`LocalVfs::metadata_capabilities(root)` öffnet den konkreten Root ohne Symlinks.
Bei einem bereits gebundenen VFS beginnt die Abfrage am gehaltenen Root-Deskriptor.
Die Mountidentität wird vor und nach der Abfrage verglichen. Eine Vorschau legt
keine Probedatei an und ändert weder Rechte noch Attribute.

macOS liefert `ATTR_VOL_CAPABILITIES` über denselben Deskriptor. Nur gültige Bits
belegen Rechte, ACLs, Hardlinks, Sparse-Dateien, native xattrs und Ressourcen-Forks.
Nicht gültige Bits bleiben `Unknown`; ein gültiges ausgeschaltetes Bit bedeutet
`Unsupported`. `MNT_IGNORE_OWNERSHIP` sperrt die Owner-/Rechtezusage. Der Adapter
meldet xattrs weiterhin höchstens `ReadOnly`, weil er auf macOS keinen exakten
xattr-Schreibvertrag besitzt. Die Definitionen stammen aus Apples SDK `sys/attr.h`.

Linux ermittelt den tatsächlichen Treibernamen aus `/proc/self/mountinfo` anhand
der am Deskriptor gelesenen normalen Mount-ID. Dadurch erhält ext2 trotz derselben
statfs-Kennzahl keine ext4-Sparse-Zusage. ext4, XFS, Btrfs und tmpfs besitzen die
verwendeten nativen Verträge; ACL und der user-xattr-Namensraum werden zusätzlich
rein lesend abgefragt. Ein fehlender Wert (`ENODATA`) ist keine fehlende Fähigkeit
(`EOPNOTSUPP`). FAT/exFAT besitzen diese nativen Metadatenklassen nicht; unbekannte
Treiber einschließlich FUSE, Overlay und eingehängter Remote-Dateisysteme bleiben
konservativ `Unknown`. Mountinfo wird auf vier MiB begrenzt; fehlende Mount-IDs,
Lesefehler und widersprüchliche Antworten bleiben Fehler. Die Zuordnung zur Mount-ID
folgt [mountinfo](https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html);
die Leseabfragen folgen [xattr](https://man7.org/linux/man-pages/man7/xattr.7.html).

Ein nur lesbar eingehängter Mount stuft exakte Schreibfähigkeiten auf `ReadOnly`
zurück. Das ACL-Modell beschreibt dabei weiterhin das gelesene Format. Fähigkeiten
ersetzen weder Dateirechte noch die frische Plan-, Identitäts- und Readbackprüfung.
`Portable` bleibt der Standard; diese Änderung schaltet keine neue Metadatenklasse
oder mutierende Probe frei.

## Gegenlauf mit eigenen Images

Der opt-in-Test `core/tests/local_metadata_mounts.rs` erwartet ausschließlich
frische eigene Testmounts. Der absolute Elternordner heißt
`doppeldecker-metadata-e2e-…` und enthält `.doppeldecker-metadata-test` mit exakt
`doppeldecker-metadata-e2e-v1` plus Zeilenumbruch. `native/` und `exfat/` sind die
beiden Mountpunkte. Quelle und Ziel müssen leer und für den Testnutzer schreibbar
sein; vorhandene Quelldateien werden vor jedem Schreiben zurückgewiesen.

```sh
export DOPPELDECKER_METADATA_TEST_ROOT=/tmp/doppeldecker-metadata-e2e-…
cargo test -p doppeldecker-core --test local_metadata_mounts \
  local_metadata_mounts_report_matches_native_and_exfat \
  -- --exact --ignored --nocapture
```

Nach diesem Lauf den eigenen nativen Mount nur lesbar einhängen und separat prüfen:

```sh
cargo test -p doppeldecker-core --test local_metadata_mounts \
  local_metadata_readonly_mount_never_claims_write \
  -- --exact --ignored --nocapture
```

Die Vorbereitung erzeugt neue reguläre Image-Dateien, formatiert ausschließlich
diese Dateien und registriert jedes eigene Image vor dem Einhängen für das Cleanup.
Auf macOS wurden ein 128-MiB-APFS-Image und ein 64-MiB-exFAT-Image verwendet; das
exFAT-Image wurde als reguläre Datei mit `mkfs.exfat` erzeugt. Unter Linux liefen
128-MiB-ext4 und 64-MiB-exFAT in einem eigenen Container-Mount-Namensraum. Nur die
für diese Images zugeteilten Loop-Geräte wurden freigegeben. Keine Nutzerlaufwerke,
Systempapierkörbe oder echten App-Konfigurationen gehörten zum Versuch.

| Gegenlauf 2026-10-06 | Vorschau und tatsächlich geprüfte Wirkung |
| --- | --- |
| exFAT auf macOS/Linux | Keine exakte Owner-/Rechte-/ACL-/Sparse-/Hardlink-/Fork-/xattr-Zusage; NativeStrict vor Mutation abgelehnt; Hardlink und Sonderrechte tatsächlich nicht darstellbar. |
| APFS | NativeBestEffort kopiert Inhalt, Owner, Rechte/Sonderbits, NFSv4-ACL, Sparse-Bereiche, Hardlinks und Ressourcen-Fork mit exaktem Rücklesen. Vorhandene xattrs werden sichtbar ausgelassen; NativeStrict lehnt sie ab. |
| ext4 | NativeStrict-Vorschau freigegeben und tatsächliche Kopie einschließlich Owner, Rechte/Sonderbits, POSIX-ACL, Sparse-Bereiche, Hardlinks und user-xattrs exakt zurückgelesen. |
| Native Mounts nur lesbar | Owner, Rechte, Sparse-Bereiche und Hardlinks werden `ReadOnly`, nicht `Exact`. |

Erneuter Gegenlauf am 2026-10-07: Linux-exFAT kopiert und aktualisiert Inhalte
unter `NativeBestEffort`, während nicht darstellbare native Klassen pfadgenau
bilanziert werden. macOS-exFAT liest das fähigkeitsgebundene Manifest und erlaubt
diese Metadatenvorschau ebenfalls. Sein exklusiver Rename liefert jedoch
`ENOTSUP`; der strikte Sync-Inhaltslauf erhält deshalb keinen gewöhnlichen
Rename-Fallback. Die separate interaktive Ausführung ist unten beschrieben.
Die APFS-/ext4-Übertragung und beide Read-only-Gegenläufe wurden erneut ausgeführt.

Ein Namensaustausch gegen einen Symlink lenkt die Abfrage nicht um: Der ungehaltene
VFS lehnt den Symlink ab, der gebundene VFS liest weiterhin den gehaltenen Root.
Vorschauen hinterlassen leere Ziele. Der normale Gate-Lauf ignoriert die beiden
Mounttests ausdrücklich; ihre erfolgreichen opt-in-Läufe sind getrennte Belege.
Die Abnahme umfasst APFS und ext4 mit nativen Treibern sowie exFAT auf beiden
Plattformen; weitere Dateisysteme wurden nicht als reale Image-Gegenläufe geprüft.

## Interaktive Dateioperationen je Volume

`file_commit_class_at(parent)` liest die Fähigkeiten am gehaltenen konkreten
Elternordner und vergleicht seine Mountidentität vor und nach der Abfrage.
macOS verwendet das gültige `VOL_CAP_INT_RENAME_EXCL`-Bit. APFS meldet
`Assured`, exFAT `BestEffort`; die tatsächlichen exklusiven Rename- und
Hardlink-Proben auf einem eigenen macOS-exFAT-Image liefern jeweils `ENOTSUP`.
Linux ordnet bekannte Kernel-Treiber mit No-Replace-Vertrag anhand des tatsächlich
gehaltenen Mounts zu; ext4 und der native exFAT-Treiber bestehen den exklusiven
Gegenlauf. Unbekannte oder FUSE-Treiber erhalten keine pauschale lokale Zusage.
Der Metadatenvertrag bleibt davon getrennt. Insbesondere braucht gewöhnliches
`rwx` nicht die stärkere Owner-/Sonderbitzusage; FAT/exFAT stellt diese POSIX-Rechte
nicht exakt dar.

Bei Best Effort pausiert die Queue vor jeder Übertragung, bis die konkrete
Zielroot- und Mountidentität bestätigt wurde. Ein neuer Claim vergleicht sie
frisch. Der Commit prüft das vollständige Elternlisting, benennt genau einmal
gewöhnlich um, synchronisiert beide Eltern und liest den Inhalt zurück. Das
verbleibende Kollisionsfenster wird vorab ausdrücklich erklärt. Eine zuvor
sichtbare Kollision bleibt unangetastet; ein unklarer Ausgang wird nicht blind
wiederholt. Verschieben nutzt auf einem schwächeren Ziel Kopie, Inhalts-Readback
und den bestehenden begrenzten Quellabbau. Die separate strikte Sync-Vorbereitung
nutzt weiterhin exklusives Rename. Der normale lokale AddOnly-/Update-Start besitzt
seit 2026-10-17 ein eigenes gebundenes Wirkungsprofil und verwendet dieselben
Commit-/Readbackhelfer wie interaktive Dateioperationen. Datei- und
Verzeichnis-Flushzusagen werden je gehaltenem Mount unabhängig vom atomaren
Neunamencommit bestimmt. Nicht darstellbare POSIX-Rechte bleiben auch unter
`Portable` pfadgenau sichtbar; die Best-Effort-Ausführung erteilt keine neue native
Metadatenzusage. Vertrag: [Normaler lokaler Sync](local-sync.md).

`core/tests/local_sync_mounts.rs` ergänzt ausdrückliche Opt-in-Gegenläufe auf
eigenen APFS-/ext4-/exFAT-Images: normaler AddOnly-/Update-Start, dauerhafte schwache
Ersetzung oder Quarantäne, Inhaltsdrift bei gleicher Größe/Zeit, Metadatenprofile,
Case-/Unicode-Kollisionen, Elterntausch und echter Prozessabbruch mit Queue-Reopen.
Die drei öffentlichen Fälle wurden auf beiden Plattformen erfolgreich ausgeführt;
der zusätzliche Kindprozessfall wird ausschließlich vom Crash-Test gestartet.

`core/tests/local_file_commit_mounts.rs` verwendet denselben Marker- und
Mountvertrag wie die Metadatentests. Fünf opt-in-Tests prüfen tatsächliche
Volumeklassen, Queue-Pause vor jeder Schreibwirkung, falsche Phrase,
Queue-Neustart, Roottausch vor und während der Übertragung, ProviderFile-Ziele,
späte Kollision, Abbruch, Datei-/Ordner-Move und die getrennte Sync-Grenze.
Auf macOS prüft ein zusätzlicher tatsächlicher CLI-Lauf falsche und gültige
stdin-Bestätigung. Eine native markierte GUI-Sandbox prüft F5, sichtbaren
Risikotext und Eingabefeld sowie den vollständigen bytegleichen Transfer.
Weitere Dateisysteme und ein Stromausfall sind durch diese Gegenläufe nicht belegt.
