# OpenCore-Plugin in WordCraft: Erfahrungsbericht

Dieser Bericht gehört zum Fork (Branch `kosmos`), nicht zu Upstream. Er hält fest, wie das OpenCore-Plugin
(EDMS: öffnen, speichern, neu laden, Nebendateien; Craft Signal: Zusammenarbeit) in diese App eingebunden ist,
was dabei gut ging und was dem Plugin fehlte. Zweck: das Plugin so weit bringen, dass die nächste App schnell und
sicher angebunden ist. Gegenstück in jedem Fork; der Plan steht in `opencraft/PLAN_EDMS_APPS_20261122.md`.

Stand: 22.11.2026, Fork-Tag `v0.18.1-kosmos.16` (`9aed340e`), Basis Upstream `v0.18.1`.

## 1. Kennzahlen

| | |
|---|---|
| Patch gegen Upstream | 46 Dateien, 6435 Zeilen dazu, 57 weg, 19 Commits |
| davon Engine | 3938 Zeilen (`edms.rs` 671, `cmd/edms.rs` 379, `signal.rs` 1036, `signal_frame.rs` 202, Tests 1562) |
| davon Oberfläche | 1519 Zeilen (Ortswähler `edms_browser.rs` 1048) |
| davon Web-Fassung | 620 Zeilen (fetch, Ereignisstrom, Signal-Sockets) |
| davon native Fassung | 213 Zeilen |
| Fähigkeitsstufe | 1 Hauptdokument, 2 Nebendateien (ohne Oberfläche), 3 Ereignisse (Sperre nur als Befehl), 4 Relais (lokal geprüft) |
| Ausgerollt | Web-Paket auf xwork; Relais läuft dort nicht |

Die Patchgröße ist der wichtigste Wert dieses Berichts: fast nichts davon ist Fachlogik von WordCraft. Ziel nach
dem Herauslösen (Plan, Stufe 0): unter 400 Zeilen.

Verlauf während Stufe 0 (gemessen mit `git diff --shortstat` gegen Upstream `main`):

| Stand | Patch | Was ins Kit `craft-opencore` ging |
|---|---|---|
| kosmos.20 / .21 (22.11.) | 51 Dateien, 7067 Zeilen dazu | nichts; .21 bindet nur das Kit statt drei Crates ein |
| `62b183f6` (22.11.) | 51 Dateien, 6248 Zeilen dazu | Bereich `session`: Zustand, Pumpe, Befehle `edms.*` (945 Zeilen aus `edms.rs` und `cmd/edms.rs`); WordCraft behält den Adapter (`DocumentHost`, 126 Zeilen) und das Vorschalten der Zusammenarbeit beim Speichern |
| offen | | Web-Transporte, native Transporte, Ortswähler, Tests von `edms.rs` in den Prüfsatz |

## 2. Wo das Plugin in der App hängt

| Stelle der App | Was dort hängt | Bewertung |
|---|---|---|
| `Session` (Engine) | Felder `edms`, `signal`; Pumpe je Frame | nötig, aber als ein Feld und ein Aufruf genug |
| Befehlsregister (`cmd.rs`) | `edms.*`, `signal.*` | gut: eine Zeile je Gruppe |
| `Session::run` | Aufzeichnen eigener Befehle für Signal, Sperre während des Abgleichs | Eingriff in den Kern der App, klein halten |
| `file.save` | geht über das Plugin, wenn das Dokument einen EDMS-Ort hat | gut |
| Dokument ersetzen (`set_document`) | vergibt eine neue Dokument-Kennung | Falle, siehe 4 |
| Web-Fassung | fetch-Transport, Ereignisstrom, zwei WebSockets | gehört ins Plugin, nicht in die App |
| Oberfläche | Ortswähler, Statuszeilen, Startansicht | Ortswähler gehört ins Plugin |
| `Cargo.toml` | Pfad-Abhängigkeit `../opencraft/crates/...` | hält nur, solange opencraft daneben liegt |

## 3. Was gut ging

- **Alles ist ein Befehl.** Hülle, Tests und Automation sprechen das Plugin über dieselben JSON-Befehle an wie
  alles andere. Der Browser-Test (`opencraft/tests/browser/test_selfload.py`) braucht keine Sonderwege.
- **Kern ohne I/O** (`craft-edms`, `craft-signal`) mit aufzeichnendem Transport: fast alle Fehler ließen sich
  als Engine-Test ohne Netz nachstellen.
- **Die Hülle ist nur Anstoß.** Sie startet die App, gibt Sprache und Anmeldung, ruft `edms.connect` und
  `edms.open`. Alles Weitere betreibt die App. Das hat sich bewährt; jede Logik in der Hülle war später im Weg.
- **Ort je Dokument** (Arbeitsverzeichnis „wie ein pwd“): Nebendateien relativ zum Dokument ließen sich ohne
  Eingriff in die Befehle der App anbinden (`edms.runWith`: erst holen, dann den Befehl genau einmal ausführen).

## 4. Fallen, in die wir getreten sind

| Falle | Folge | Lehre für das Plugin |
|---|---|---|
| fetch ohne Zeitstempel und ohne `no-store` | App öffnete eine alte Fassung aus dem Browser-Cache, Speichern endete mit 412 | Transport gehört ins Plugin, einmal richtig |
| Dokument ersetzen vergibt eine neue Kennung | der EDMS-Ort hing an der alten Kennung | Plugin braucht „Dokument X ist jetzt Y“ als Einstieg |
| Anfragen der Pumpe an die Oberfläche wurden nur nach einem Befehl verarbeitet | Startansicht blieb stehen | Pumpe und Oberfläche im selben Frame; Vertrag festhalten |
| Befehle ohne offenes Dokument lehnt die Oberfläche ab | `edms.connect` ging nicht auf der Startansicht | Plugin-Befehle brauchen keinen Dokumentzustand |
| der web-Dienst schreibt `<base href="/">` in jede `index.html` | relative Pfade des WASM-Builds brachen | Einstieg heißt `app.html` (Bau-Skript) |
| Build ohne `CRAFT_I18N_DIR` | Oberfläche nur englisch | Sprachkataloge sind Bau-Eingabe, im Job prüfen |
| Ortswähler mit festen Maßen | passte nicht in kleine Fenster | Oberfläche des Plugins an die Fläche binden |
| Teilnehmer wartet auf den Stand eines Masters, der nie antwortet | Bearbeiten wäre für immer gesperrt gewesen | jede Wartestelle braucht eine Frist und einen Weg zurück |
| Bild-Bytes als base64 im Befehl | Grenze des Steuerkanals, doppelter Speicher | Bytes als Bytes, nicht durch JSON |
| Zeitstempel `t` pauschal an alle Anfragen (Cache-Fix) | Graph-API lehnte ab (400), „Speichern unter“ ging nicht | Parameter nur, wo der Server sie annimmt; jede Fremd-API im Browser-Test |
| Bild-Bytes unter gleichem Namen getauscht (Rahmen → Bild) | Seitenbild-Cache der App kannte nur den Namen, Bild erschien erst nach der nächsten Eingabe | Caches der App kennen: wer Inhalte unter gleichem Namen tauscht, muss einen Zähler anbieten |
| Tests liefen nur lokal, der Worker baut nur | ein roter Stand wäre ausgeliefert worden | Tests in den Worker-Job |

## 5. Was dem Plugin fehlt (aus dieser App gelernt)

1. Sitzung, Transporte und Ortswähler als eigene Crates statt als Patch (Plan, Abschnitt 4).
2. Ein Adapter-Vertrag (`DocumentHost`) und ein Prüfsatz dagegen.
3. Ein Vertrag mit der Hülle, mit Version, als Datei und als Test.
4. Eigener Textkatalog; die Texte des Forks sind englisch und fest verdrahtet.
5. Konfliktdialog: heute nur eine Statuszeile.
6. Nebendateien schreiben; Bereichslesen; Verweise in einen anderen Space.
7. Oberfläche für `edms.runWith`, für die Rückfrage beim Speichern mit offener Nachlieferliste und für den
   Rahmen eines Bildes, das noch unterwegs ist.

## 6. Offene Punkte dieser App

- Ein Teilnehmer im Modus Read-Only kann lokal tippen.
- Beim späteren Beitritt geht ein ungespeicherter lokaler Stand ohne Rückfrage verloren.
- Vorbestehend rot, nicht von uns: `cmd::packs::tests::swiss_german_and_portugal_portuguese_keep_their_languages_rules`.
- Der Fork ist bei GitHub öffentlich abrufbar (geprüft 22.11.2026); ob das gewollt ist, entscheidet der Auftraggeber.

## 7. Wie geprüft wird

- Engine: `cargo test -p wordcraft-engine --lib` (1395 grün, 1 vorbestehend rot), Clippy, WASM-Check.
- Zusammenarbeit: `opencraft/testbench/run.py --headless` (zwei native Instanzen gegen das Go-Relais).
- Browser: `opencraft/tests/browser/test_selfload.py` gegen xwork nach jedem Rollout.
