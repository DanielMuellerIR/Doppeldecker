# Doppeldecker-Patch für smb2 0.13.0

Die unveränderte Crate bietet im SMB-3.1.1-NEGOTIATE Cipher-Kontexte an, setzt aber
`SMB2_GLOBAL_CAP_ENCRYPTION` nicht in den Client-Capabilities. Samba verweigert bei
erzwungener Verschlüsselung deshalb bereits `SESSION_SETUP`.

Doppeldecker ergänzt dieses Bit in `src/client/connection.rs`. Die
SMB2-Spezifikation empfiehlt es für einen SMB-3.1.1-Client, der die angebotenen
Verschlüsselungsalgorithmen unterstützt:

<https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/77f696b8-9aa0-4ed1-abb2-c097c0ccb05a>

Der Empfänger erzwingt außerdem die aktivierte Sitzungsverschlüsselung auch für
eingehende Frames. Eine Klartextantwort beendet die Verbindung vor Dekompression,
Creditänderung oder Routing; wartende Aufrufe erhalten einen Fehler. Zuvor wurden
selbst unsignierte Klartextantworten und ungeschützte Zwischenantworten akzeptiert,
während die Verbindung als verschlüsselt galt. Der Core-Paketgegenlauf prüft
Klartext, korrekt signierten Klartext, eine ungeschützte Zwischenantwort, gültiges
AES-GCM und einen veränderten Authentifizierungstag über den echten Empfänger.

Clientvertrag für die Sitzungsverschlüsselung:
<https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/8174c219-2224-4009-b96a-06d84eccb3ae>

Der Empfängertask hält den gemeinsamen Verbindungszustand während des Wartens
nur schwach. Zuvor bildeten dessen starkes `Arc` und das im Zustand gehaltene
Taskhandle einen Besitzzyklus; selbst nach dem letzten `Connection`-Drop blieben
Transport und Empfänger bis zum Serverabbruch oder Runtimeende erhalten. Ein
Core-Gegenlauf prüft die tatsächliche Freigabe beider Transportreferenzen nach
dem letzten Klon, während ein verbleibender Klon die Verbindung weiter hält.

Die Upstream-Lizenzen bleiben unverändert in `LICENSE-MIT` und `LICENSE-APACHE`.
Zusätzlich heißt ein nicht gebauter Docker-Testfixture-Pfad neutral `Profiles`, damit
keine rechnergebundene Nutzerpfadform in Doppeldeckers Repository liegt.
