# OpenCore-Plugin in WordCraft: Erfahrungsbericht

Dieser Bericht gehört zum Fork (Branch `kosmos`), nicht zu Upstream. Er hält fest, wie das OpenCore-Plugin
(EDMS: öffnen, speichern, neu laden, Nebendateien; Craft Signal: Zusammenarbeit) in diese App eingebunden ist,
was dabei gut ging und was dem Plugin fehlte. Zweck: das Plugin so weit bringen, dass die nächste App schnell und
sicher angebunden ist. Gegenstück in jedem Fork; der Plan steht in `opencraft/PLAN_EDMS_APPS_20261122.md`.

Stand: 24.11.2026, Fork-Tag `v0.18.1-kosmos.24` (`e5f0b1d8`) und die Sperre beim Beitritt (danach), Basis Upstream
`v0.18.1`. Abschnitte 1 bis 3 beschreiben den Anfang (22.11.), der Verlauf in Abschnitt 1 und die Abschnitte 4 bis 8
sind nachgeführt.

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
| `89cb287a` (23.11.) | 45 Dateien, 5427 Zeilen dazu | Features `web` und `native`: fetch, Ereignisstrom, Signal-Sockets, `nocache` (766 Zeilen); WordCraft gibt nur noch den Weckruf aus egui (`craft_opencore::egui::wake`) |
| `0e7386b3` (23.11.) | 44 Dateien, 4449 Zeilen dazu | Bereich `session::collab`: Journal, Rücknahme, Konfliktregel, Abgleich, Speichern über den Master, Befehle `signal.*`; WordCraft behält den Adapter `CollabHost` (`signal.rs` 1254 → 380: Momentaufnahme, Anker als Absatzbereich, Zustand als docx, Bildrahmen) |
| `65def218` (23.11.) | 44 Dateien, 4169 Zeilen dazu | Bereich `session::picker`: Modell und Ablauf des Ortswählers; WordCraft zeichnet ihn unverändert mit seinen Bedienelementen (`edms_browser.rs` 1048 → 753) |
| `eb52853b` (23.11.) | 43 Dateien, 3914 Zeilen dazu | Bildrahmen: Buchführung (`session::collab::frames`) und Bau der Rahmen (`session::collab::stand_in`, Feature `pictures`); WordCraft behält das Ersetzen im Dokument und das Wiederholen der Bildformat-Änderung. Rahmen byte-gleich mit dem alten Stand |
| `308a3db4` (23.11.) | 43 Dateien, 3745 Zeilen dazu | Ortswähler zeichnet das Kit (`egui::picker`) mit WordCrafts Aussehen (`PickerLook`); gleiche egui-Formen in 48 Fällen nachgewiesen, mit Gegenprobe (`edms_browser.rs` 753 → 578) |
| `35dae997` (24.11.) | unverändert | Meldungen sagen „Core“ statt „the cloud“ (Wortlaut im Kit) |
| `9afc3ce7` (24.11.) | 43 Dateien, 3710 Zeilen dazu | Ablauf der Bildrahmen im Kit (`collab::pictures`, `PictureHost`); Rahmen-Tests byte-genau grün |
| `4c53a021` (24.11.) | 43 Dateien, 3732 Zeilen dazu | Rückgängig/Wiederholen aus, solange beigetreten (Kit `refuses`, Variante C des gemeinsamen Rückgängig) |
| `cd59c74f`, `e5f0b1d8` (24.11.) | 43 Dateien, 3749 Zeilen dazu | Desktop: EDMS-Transport und Ereignisstrom des Kits (`native`); vorher hatte die Desktop-Fassung kein EDMS |
| `19cbefa5` (24.11.) | unverändert | Kopfzeile nimmt die Befehle zu den Wünschen aus dem Kit (`wish_command`) |
| Sperre beim Beitritt (24.11., nach kosmos.24) | Befehlslauf: eine Prüfung statt einer | Kit `collab::edit_lock`: bis feststeht, ob man führt oder nachfolgt, beim Nachzügler bis zu seiner Wahl, mit Leserecht und nach einem Abbruch ist das Dokument nur lesbar (opencraft `SIGNAL.md`, „Sperre beim Beitritt“) |

Von den 3914 Zeilen sind 1681 Tests (`tests/signal.rs` 1130, `tests/edms.rs` 551), der Code ohne Tests liegt bei
rund 2230 Zeilen. Die Tests bleiben in WordCraft (Bewertung 23.11.): fast jeder prüft Kit und Adapter zusammen mit
echtem WordCraft-Inhalt (Absätze als Anker, Zustand als docx, Rahmen in den Rückgängig-Schritten, PDF-Export,
Serienbrief-CSV). Sie sind der Nachweis, dass sich am Verhalten nichts ändert. Nur vier kleine Tests (rund 70
Zeilen) prüfen allein das Kit, und das Kit deckt diese Fälle schon ab.

Grundsatz beim Herauslösen (Auftraggeber 23.11.): Das Verhalten der App bleibt identisch, nur die Technik dahinter
wird zerteilt. Jeder Schritt wurde Zeile für Zeile gegen den alten Code verglichen; gefundene Abweichungen
(Auswahl nach einer Rücknahme, eine Fehlermeldung, ein zusätzliches Neu-Layout, zwei Log-Zeilen) sind behoben.
Aussehen bleibt bei der App: das Kit liefert dort Modell und Ablauf, keine Ersatz-Bedienelemente.

### Wie klein kann der Patch werden (Bewertung 23.11.2026)

Ohne Mitwirkung von Upstream bleiben Stellen, an denen das Plugin in WordCraft eingreifen muss, weil WordCraft
dafür keine Andockstelle hat. Schätzung je Teil nach vollständigem Herauslösen:

| Teil | heute | erreichbar | warum es bleibt |
|---|---|---|---|
| Signal-Sitzung (`signal.rs`, `signal_frame.rs`, `cmd/signal.rs`) | 1558 | ~250 | Adapter: Schnappschuss, Anker als Absatzbereich, Befehl fremd ausführen, Zustand als docx, Bildrahmen (WordCraft-eigen) |
| EDMS-Adapter und Befehle | 252 | ~120 | `DocumentHost`; Speichern mit Zusammenarbeit geht ins Kit |
| Eingriffe in den Kern (`run`, history, snapshot, documents, lifecycle, picture) | ~80 | ~80 | Aufzeichnen jeder Änderung, Sperre beim Abgleich, Bild-Cache-Zähler |
| Ortswähler | 1048 | ~150 | Modell und Zeichnen ins Kit; bleibt: was nach der Wahl geschieht (`AfterPick`) |
| Oberfläche Datei-Menü, Backstage, Dialoge, Kopfzeile, Einbettung | ~430 | ~350 | WordCraft hat keine Andockstelle für einen zweiten Speicherort |
| Hüllen web und nativ | ~110 | ~70 | Einbauen, Weckruf, Abfragen der Einbettung |
| Tests | 1680 | ~1680 | prüfen Kit und Adapter mit echtem WordCraft-Inhalt; kleiner nur mit einem allgemeinen Prüfsatz des Kits, den jede App mit ihren Befehlen füttert (nicht begonnen) |
| Cargo | ~15 | ~15 | |
| **Summe** | **~5400** | **~1200 Code, dazu die Tests** | |

Unter 400 Zeilen geht nur mit Andockstellen in Upstream: ein Speicherort-Anbieter für Datei-Menü und Dialoge, ein
Haken in `Session::run` für Erweiterungen, Befehle aus einem Plugin registrierbar. Dann blieben Adapter und
Einbau (~300). Das ist eine Entscheidung mit Upstream, nicht im Fork zu machen.


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

### Nachtrag 24.11.: was das Kit seither trägt

- Aus Abschnitt 5 erledigt: 1 (eigene Crate `craft-opencore` mit Bereichen `session`, `collab`, `picker`, `web`,
  `native`, `egui`), 2 zum Teil (Adapter `DocumentHost`/`CollabHost`/`PictureHost`; Prüfsatz nur im Kit mit einer
  Schein-App, nicht gegen jede App), Ortswähler-Oberfläche aus 7.
- Die zweite App (DeckCraft) zeigte, was im Kit fehlte, und das ist jetzt drin: Zeit und Autor eines Befehls
  (`clock`, `command_author`), Sperre für Rückgängig, Meldungstexte je App (`signal_notices`), Kopfzeilen-Widget als
  ganzes (`egui::header`), native Transporte. Siehe `deckcraft/OPENCORE_REPORT.md`.
- Sperre beim Beitritt (Entscheidung Auftraggeber 24.11.): ein Dokument weicht nie vom Stand seiner Sitzung ab.
  Ein Nachzügler erfährt, dass ein anderer die Datei bearbeitet, und wählt Beitreten oder Nur lesen; das Widget
  klappt dafür von selbst auf.

### Weitere Fallen (23./24.11.)

| Falle | Folge | Lehre für das Plugin |
|---|---|---|
| Abbruch der Verbindung, dann weiter getippt | nichts ging hinaus; beim Wiederbeitritt ersetzte der Stand des Masters das Getippte | nach einem Abbruch nur lesbar (Kit) |
| zwischen Öffnen und Beitritt bearbeitbar | Eingaben vor dem Beitritt gingen beim Übernehmen verloren | ab dem Öffnen gesperrt, bis feststeht, ob man führt |
| selbsttätiger Beitritt je Datei statt je Öffnen | wer schloss und neu öffnete, trat nicht wieder bei | je geöffnetem Dokument |
| Read-Only-Teilnehmer tippten lokal | ihr Dokument wich still ab | Leserecht sperrt im Kit, nicht in der App |

## 6. Offene Punkte dieser App

- ~~Ein Teilnehmer im Modus Read-Only kann lokal tippen.~~ Gesperrt seit 24.11. (Kit).
- ~~Beim späteren Beitritt geht ein ungespeicherter lokaler Stand ohne Rückfrage verloren.~~ Kann nicht mehr entstehen
  (gesperrt bis zum Beitritt).
- „Speichern unter“ schlägt bei Dokumenten aus dem EDMS `….docx.docx` vor (`title_stem` nimmt den Dateinamen samt
  Endung; vorbestehend, nicht vom Umbau).
- Der Hinweis „File › Reload from Core“ nennt einen Menüpunkt, den es nicht gibt; Befehle und Ortswähler sagen „EDMS“.
- Gemeinsames Rückgängig Variante A (Unterschied als Befehl) braucht einen Befehl für Absatzbereiche.
- Wer die Sitzung selbst verlässt, bearbeitet allein weiter (nicht entschieden).
- Vorbestehend rot, nicht von uns: `cmd::packs::tests::swiss_german_and_portugal_portuguese_keep_their_languages_rules`.
- Der Fork ist bei GitHub öffentlich abrufbar (geprüft 22.11.2026); ob das gewollt ist, entscheidet der Auftraggeber.

## 7. Wie geprüft wird

- Engine: `cargo test -p wordcraft-engine --lib` (24.11.: 1402 grün, 1 vorbestehend rot), Clippy, WASM-Check.
  Oberfläche: 1220 grün, 2 rot nur ohne `craft-fonts` (Glyphen).
- Zusammenarbeit: `opencraft/testbench/run.py --headless` (zwei native Instanzen gegen das Go-Relais).
- Browser gegen xwork nach jedem Rollout: `test_selfload.py`, `test_picker.py`, `test_collab.py` (zwei Fenster; prüft
  seit 24.11. auch Frage, Nur lesen und Beitreten und misst die Zeiten), `test_two_users.py` (zwei Nutzer, Bilder).
