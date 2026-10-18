# procd

`procd` ist ein schlanker Prozess-Supervisor für Docker-Container, in denen
mehr als ein langlaufender Prozess laufen soll, z.B. ein Webserver und ein
Worker. procd läuft als Entrypoint des Containers, startet die in
`process.toml` definierten Prozesse und kümmert sich um sie:

- **Lebenszyklus:** Start beim Hochfahren, geordneter Stop bei `docker stop`
  (SIGTERM an jede Prozessgruppe, SIGKILL nach Timeout)
- **Neustart bei Absturz** mit Verzögerung und Burst-Limit, ähnlich `Restart=on-failure` bei systemd
- **Logs:** Die Ausgabe aller Prozesse erscheint mit Zeitstempel und Prozess-Key
  in `docker logs`, optional zusätzlich in einer Datei pro Prozess
- **Zeitplan:** Prozesse per Cron-Ausdruck automatisch starten (Cron-Jobs)
  oder regelmäßig neu starten
- **Crash-Hook:** ein beliebiges Kommando pro Absturz, z.B. für Benachrichtigungen
- **Status:** Zustand, PID, Laufzeit, RAM und CPU pro Prozess
- **Steuerung:** per CLI (`docker exec … procd ls`) oder über eine JSON-API auf
  einem Unix-Socket, die auch andere Prozesse oder Container nutzen können

Ein einziges statisches Binary (~2 MB), keine Laufzeitabhängigkeiten.

## Installation

`install.sh` lädt das statische Binary für die passende Architektur
(`x86_64` oder `aarch64`) aus den GitHub-Releases, prüft die SHA256-Summe und
installiert es nach `/usr/local/bin/procd`. Es braucht nur `sh` und `curl`
oder `wget`, läuft also auch mit busybox auf Alpine.

```bash
curl -fsSL https://raw.githubusercontent.com/bli-blu-bla/process-deamon/main/install.sh | sh

# bestimmte Version, anderes Verzeichnis
curl -fsSL https://raw.githubusercontent.com/bli-blu-bla/process-deamon/main/install.sh \
    | sh -s -- --version v0.2.0 --dir ~/.local/bin
```

| Option | Env | Default |
|---|---|---|
| `-v`, `--version <tag>` | `PROCD_VERSION` | `latest` |
| `-d`, `--dir <pfad>` | `PROCD_INSTALL_DIR` | `/usr/local/bin` |
| `-r`, `--repo <owner/repo>` | `PROCD_REPO` | `bli-blu-bla/process-deamon` |
| | `PROCD_BASE_URL` | `https://github.com/<repo>/releases` (für Mirror) |

Ist das Zielverzeichnis nicht beschreibbar, wird `sudo` verwendet. Im
Dockerfile ersetzt das die Build-Stage:

```dockerfile
FROM alpine:3
RUN wget -qO- https://raw.githubusercontent.com/bli-blu-bla/process-deamon/main/install.sh | sh -s -- --version v0.2.0
```

Releases entstehen automatisch beim Pushen eines Tags `v*` (siehe
`.github/workflows/build.yml`).

## Docker

### Einbinden ins Image

procd wird in einer eigenen Build-Stage gebaut und als Binary ins
Anwendungs-Image kopiert:

```dockerfile
FROM rust:1-alpine AS procd
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM alpine:3
COPY --from=procd /src/target/release/procd /usr/local/bin/procd
COPY process.toml /etc/procd/process.toml
ENV PROCD_CONFIG=/etc/procd/process.toml

HEALTHCHECK --interval=30s --timeout=3s CMD ["procd", "ls"]
ENTRYPOINT ["procd"]
```

Das musl-Binary ist statisch gelinkt und läuft damit auch in Images, die nicht
auf Alpine basieren (Debian, distroless, `scratch` mit `/bin/sh`).

### Starten

```bash
docker run -d --init --name app my-image
```

```yaml
# compose.yaml
services:
  app:
    image: my-image
    init: true
    stop_grace_period: 30s
```

### Bedienen

```bash
docker logs -f app                     # Ausgabe aller Prozesse
docker exec app procd ls               # Übersicht
docker exec app procd status worker
docker exec app procd stop worker
docker exec app procd start -a
```

```
KEY       STATE    PID  UPTIME  MEM      CPU   LAST EXIT
web (ro)  running  7    3h12m   48.2 MiB 1.3%  -
worker    crashed  -    -       -        -     code 1
```

### Hinweise für den Container-Betrieb

- **Stop-Timeout:** `docker stop` wartet standardmäßig 10s und schickt dann
  SIGKILL an den ganzen Container. procd braucht im schlimmsten Fall
  `shutdown_timeout` (Default 10s) plus die Restlaufzeit gerade laufender
  Hooks (`failed_hook_timeout`). Deshalb `stop_grace_period` bzw.
  `docker stop -t` großzügiger setzen als `shutdown_timeout` oder
  `shutdown_timeout` verkleinern.
- **PID 1 / `--init`:** procd räumt seine eigenen Kindprozesse ab, aber keine
  fremden verwaisten Prozesse, die der Kernel an PID 1 weiterreicht (z.B.
  Enkel eines abgestürzten Prozesses). Mit `--init` (compose: `init: true`)
  übernimmt das ein minimaler Init-Prozess (tini). Signale reicht er an procd weiter.
- **Healthcheck:** `procd ls` liefert Exit-Code ≠ 0, wenn procd nicht antwortet.
- **Socket teilen:** Liegt `core.sock` in einem Volume, können andere
  Container die API nutzen. `socket_perm` und die Gruppe der Socket-Datei
  müssen zum zugreifenden Benutzer passen.
- **Read-only-Modus:** Mit `procd --readonly` kann die API Prozesse nur noch
  anzeigen, aber weder starten noch stoppen. Sinnvoll, wenn der Socket nach
  außen geteilt wird.

## CLI

Ohne Subcommand läuft procd als Supervisor (identisch zu `procd run`). Die
übrigen Subcommands sind ein Client, der über den Socket mit einer laufenden
Instanz spricht.

```bash
procd                               # Supervisor mit ./process.toml
procd -c /etc/procd/process.toml    # andere Config
procd run --config process.toml -r  # Supervisor, API nur lesend

procd ls                            # Tabelle aller Prozesse (alias: list)
procd ls --json                     # dasselbe als JSON
procd status worker                 # Details eines Prozesses
procd start worker                  # einzelne Prozesse starten …
procd stop worker other             # … oder stoppen
procd start -a                      # alle nicht laufenden starten
procd stop --all                    # alle laufenden stoppen (readonly-Prozesse ausgenommen)
procd check -c process.toml         # Config prüfen und beenden, z.B. im Image-Build
```

### Supervisor-Optionen

| Kurz | Lang | Env | Default | Beschreibung |
|---|---|---|---|---|
| `-c` | `--config <PFAD>` | `PROCD_CONFIG` | `process.toml` | Pfad zur Konfigurationsdatei |
| `-r` | `--readonly` | | aus | Lehnt `process.start`/`process.stop` über den Socket ab, unabhängig von der Config. Anzeigen bleibt erlaubt. |
| `-h` | `--help` | | | Hilfe (`--help` ausführlich) |
| `-V` | `--version` | | | Version |

### Client-Optionen

| Kurz | Lang | Env | Beschreibung |
|---|---|---|---|
| `-s` | `--socket <PFAD>` | `PROCD_SOCKET` | Socket der laufenden Instanz. Default: `core.sock` aus der Config (`-c`), sonst `/run/procd.sock` |
| `-c` | `--config <PFAD>` | `PROCD_CONFIG` | Config, aus der `core.sock` gelesen wird (nur dieses Feld; fehlt die Datei, gilt der Default) |
| `-a` | `--all` | | `start`/`stop`: alle Prozesse statt einzelner Keys |
| `-j` | `--json` | | `list`/`status`: JSON statt Tabelle |

`start`/`stop` liefern Exit-Code `1`, wenn mindestens ein Prozess fehlschlägt.
Ein Prozess, der schon im Zielzustand ist, gilt nicht als Fehler, sondern wird
als `skip` gemeldet. Farben gibt es nur auf einem Terminal; mit `NO_COLOR=1`
sind sie aus.

## Konfiguration: `process.toml`

Unbekannte Felder führen beim Start zu einem Fehler (Schutz vor Tippfehlern).
Dauern werden als Strings angegeben: `"500ms"`, `"10s"`, `"1m30s"`, `"1h"`.
Relative Pfade werden relativ zum Verzeichnis der `process.toml` aufgelöst,
nicht relativ zum aktuellen Arbeitsverzeichnis. Ein kommentiertes Beispiel
liegt in [`process.toml`](process.toml).

### `[core]`

| Feld | Typ | Default | Beschreibung |
|---|---|---|---|
| `sock` | string | `/run/procd.sock` | Pfad des Unix-Sockets der API. Eine verwaiste Socket-Datei eines abgestürzten Laufs wird beim Start entfernt. Lauscht dort noch eine aktive Instanz, bricht der Start ab, bevor Prozesse gestartet werden. |
| `socket_perm` | Oktalliteral | `0o660` | Dateiberechtigungen des Sockets |
| `shutdown_timeout` | Dauer | `10s` | Wartezeit nach SIGTERM an einen Prozess, bevor SIGKILL folgt. Gilt für `stop` und für das Herunterfahren von procd. |
| `metrics_interval` | Dauer | `2s` | Sampling-Intervall für RAM/CPU |
| `envfile` | string | – | `.env`-Datei (`KEY=VALUE` pro Zeile, `#`-Kommentare, optionale Anführungszeichen), deren Variablen **alle** Prozesse und Hooks erhalten. Fehlt die Datei oder ist eine Zeile ungültig, bricht der Start ab. |
| `logfile` | string | – | Gemeinsames Crash-Log: eine Zeile pro Absturz über alle Prozesse, mit Zeitstempel, Key, Exit-Code, Signal und `cmd` |
| `failed_hook` | string | – | Kommando, das bei jedem Absturz läuft (siehe [Crash-Hook](#crash-hook)) |
| `failed_hook_timeout` | Dauer | `30s` | Maximale Laufzeit eines Hooks; danach wird seine Prozessgruppe per SIGKILL beendet |

### `[[process]]`

Ein Block pro Prozess, beliebig viele.

| Feld | Typ | Default | Beschreibung |
|---|---|---|---|
| `key` | string | *erforderlich* | Eindeutiger Name, für CLI, API und Log-Prefix |
| `cmd` | string | *erforderlich* | Wird als `sh -c "<cmd>"` in einer eigenen Prozessgruppe ausgeführt. Signale gehen an die ganze Gruppe. Mit `exec` (`cmd = "exec my-server --flag"`) ersetzt das Programm die Shell; dann beziehen sich PID und Metriken auf das Programm selbst. |
| `workdir` | string | cwd von procd | Arbeitsverzeichnis |
| `env` | string-Array | – | Zusätzliche Variablen (`"KEY=VALUE"`). Sie überschreiben die geerbte Umgebung und `core.envfile`. |
| `readonly` | bool | `false` | Verhindert **nur das Stoppen** über CLI/API. Start bleibt erlaubt; beim Herunterfahren von procd wird der Prozess trotzdem gestoppt. |
| `autostart` | bool | `true` | Beim Hochfahren starten |
| `stdout` | bool | `true` | stdout/stderr mit Prefix auf die Konsole (→ `docker logs`) spiegeln |
| `logfile` | string | – | stdout/stderr zusätzlich roh (ohne Prefix) in diese Datei schreiben. Unabhängig von `stdout`. |
| `restart` | bool | `false` | Neustart bei **Absturz** (Exit-Code ≠ 0 oder Signal). Nicht bei Exit-Code 0 und nicht nach einem Stop. |
| `restart_delay` | Dauer | `1s` | Wartezeit vor jedem Neustart |
| `restart_window` | Dauer | `60s` | Zeitfenster für das Burst-Limit |
| `restart_max_retries` | int | `5` | Maximale Neustarts innerhalb von `restart_window`; danach Zustand `failed` und kein weiterer Versuch. `0` = unbegrenzt. Ein manueller Start setzt den Zähler zurück. |
| `failed_hook` | string | erbt `core.failed_hook` | Eigener Hook für diesen Prozess; `""` schaltet ihn ab |
| `schedule` | string | – | Zeitplan als Cron-Ausdruck (siehe [Zeitplan](#zeitplan)) |
| `schedule_action` | string | `"start"` | Was zum Zeitpunkt passiert: `"start"` oder `"restart"`. Nur zusammen mit `schedule` erlaubt. |

### Zustände

| Zustand | Bedeutung |
|---|---|
| `stopped` | nicht gestartet oder per Stop beendet |
| `running` | läuft |
| `stopping` | SIGTERM gesendet, wartet auf Exit |
| `exited` | selbst beendet mit Exit-Code 0 |
| `crashed` | selbst beendet mit Exit-Code ≠ 0 oder durch Signal; ggf. ist ein Neustart geplant |
| `failed` | Neustart-Limit erreicht, kein weiterer automatischer Versuch |

(`starting` existiert im Protokoll, ist aber praktisch nie sichtbar.)

### Konsolen-Ausgabe

```
[2026-08-17 12:00:12][core] procd 0.1.0 starting (config=/etc/procd/process.toml, readonly=false)
[2026-08-17 12:00:12][core] listening on /run/procd.sock (readonly=false)
[2026-08-17 12:00:13][web] server started on :8080
[2026-08-17 12:00:13][worker] waiting for jobs
[2026-08-17 12:03:47][core] worker crashed, restarting in 2s (attempt 1/5 within 1m0s)
```

## Zeitplan

Mit `schedule` startet procd einen Prozess zu festen Zeiten bzw. startet ihn
neu. `schedule_action` legt fest, was passiert:

- **`start`** (Default): Der Prozess wird gestartet, falls er nicht läuft.
  Läuft er noch vom letzten Mal, wird der Termin übersprungen (keine
  Überlappung). Das ist ein klassischer Cron-Job, meist mit `autostart = false`.
- **`restart`**: Ein laufender Prozess wird gestoppt (SIGTERM, nach
  `shutdown_timeout` SIGKILL) und danach neu gestartet. Läuft er nicht, wird
  er einfach gestartet. Gedacht z.B. für einen nächtlichen Neustart eines
  Dienstes, der Speicher verliert.

```toml
[[process]]
key = "cleanup"
cmd = "exec /usr/local/bin/cleanup --older-than 7d"
autostart = false
schedule = "30 2 * * *"         # täglich 02:30

[[process]]
key = "web"
cmd = "exec /usr/local/bin/web-server"
schedule = "0 4 * * sun"        # sonntags 04:00 neu starten
schedule_action = "restart"
```

### Syntax

Fünf Felder wie bei klassischem cron: `Minute Stunde Tag Monat Wochentag`.

| Feld | Werte | Namen |
|---|---|---|
| Minute | 0-59 | |
| Stunde | 0-23 | |
| Tag | 1-31 | |
| Monat | 1-12 | `jan`-`dec` |
| Wochentag | 0-7 (0 und 7 = Sonntag) | `sun`-`sat` |

In jedem Feld sind `*`, einzelne Werte (`5`), Bereiche (`1-5`, `mon-fri`),
Schritte (`*/15`, `10-50/10`) und Listen (`1,15,30`) möglich. Sind Tag *und*
Wochentag eingeschränkt, reicht es, wenn einer von beiden passt (wie bei
cron). Außerdem gibt es:

| Ausdruck | Bedeutung |
|---|---|
| `@yearly`, `@annually` | `0 0 1 1 *` |
| `@monthly` | `0 0 1 * *` |
| `@weekly` | `0 0 * * 0` |
| `@daily`, `@midnight` | `0 0 * * *` |
| `@hourly` | `0 * * * *` |
| `@every <Dauer>` | festes Intervall ab dem Start von procd, z.B. `@every 30m` (mind. `1s`) |

### Verhalten

- Die Zeiten gelten in der lokalen Zeitzone des Containers (`TZ` bzw.
  `/etc/localtime`, ohne beides UTC). Bei der Umstellung auf Sommerzeit
  fallen nicht existierende Zeiten aus; bei der Umstellung auf Winterzeit
  wird eine doppelte Zeit nur einmal ausgeführt.
- Termine, die verpasst wurden, während ein Neustart lief oder die Uhr
  sprang, werden nicht nachgeholt.
- Ein Zeitplan läuft unabhängig von `autostart`, `restart` und manuellem
  Stop: Auch ein per CLI gestoppter Prozess wird zum nächsten Termin wieder
  gestartet. Er läuft auch für `readonly`-Prozesse, weil er aus der Config
  kommt und nicht über die API.
- Ein geplanter Start gilt wie ein manueller Start: Ein Prozess in `failed`
  bekommt eine neue Chance, der Neustart-Zähler wird zurückgesetzt.
- `procd status <key>` zeigt den Zeitplan und den nächsten Termin. In der API
  stehen dafür `schedule`, `schedule_action` und `next_run`.

## Crash-Hook

Stürzt ein Prozess ab (Exit-Code ≠ 0 oder Signal, ohne dass ein Stop
angefordert wurde), führt procd `failed_hook` aus. Exit-Code 0 und ein
regulärer Stop lösen ihn nicht aus.

- **Ein Aufruf pro Absturz,** *nach* der Neustart-Entscheidung:
  `PROCD_EVENT=crashed` (Neustart geplant oder `restart` aus) bzw.
  `PROCD_EVENT=failed` (Neustart-Limit erreicht).
- Ausführung über `sh -c` im `workdir` des Prozesses, in eigener
  Prozessgruppe, mit dessen Umgebung (inkl. `envfile` und `env`) plus den
  Variablen unten. Tokens für Benachrichtigungen können also aus dem
  `envfile` kommen.
- Läuft asynchron und blockiert weder den Neustart noch andere Prozesse.
  Die Ausgabe erscheint mit `[hook]`-Prefix unter dem Key des Prozesses.
- Beim Herunterfahren von procd starten keine neuen Hooks mehr. Laufende
  dürfen bis `failed_hook_timeout` zu Ende laufen.

### Umgebungsvariablen

Alle Variablen sind immer gesetzt (ggf. leer), damit Scripts sie auch unter
`set -u` lesen können.

| Variable | Beispiel | Beschreibung |
|---|---|---|
| `PROCD_EVENT` | `crashed` | `crashed` oder `failed` |
| `PROCD_KEY` | `worker` | Key des Prozesses |
| `PROCD_CMD` | `exec /usr/local/bin/worker` | dessen `cmd` |
| `PROCD_WORKDIR` | `/app` | dessen `workdir` (leer = cwd von procd) |
| `PROCD_LOGFILE` | `/var/log/procd/worker.log` | dessen `logfile` (leer wenn keins) |
| `PROCD_STATE` | `crashed` | `crashed` oder `failed` |
| `PROCD_PID` | `4711` | PID des abgestürzten Laufs |
| `PROCD_EXIT_CODE` | `3` | Exit-Code; `-1` bei Signal |
| `PROCD_EXIT_SIGNAL` | `killed` | Signalname (`killed`, `terminated`, `segmentation fault`, …), leer bei normalem Exit |
| `PROCD_STARTED_AT` | `2026-08-17T12:00:12+02:00` | Startzeit des Laufs (RFC 3339) |
| `PROCD_EXITED_AT` | `2026-08-17T12:03:47+02:00` | Zeitpunkt des Absturzes (RFC 3339) |
| `PROCD_UPTIME_SECONDS` | `215` | Laufzeit bis zum Absturz |
| `PROCD_RESTART_ENABLED` | `true` | ob `restart` aktiv ist |
| `PROCD_RESTART_SCHEDULED` | `true` | ob ein Neustart geplant wurde |
| `PROCD_RESTART_DELAY_SECONDS` | `2` | Wartezeit bis dazu (`0` wenn keiner geplant) |
| `PROCD_RESTART_COUNT` | `3` | Abstürze im aktuellen `restart_window` inkl. diesem |
| `PROCD_RESTART_MAX_RETRIES` | `5` | `restart_max_retries` (`0` = unbegrenzt) |

### Beispiel

```sh
#!/bin/sh
# /usr/local/bin/notify-crash.sh
set -eu

if [ "$PROCD_EVENT" = "failed" ]; then
    title="$PROCD_KEY: automatic restarts given up"
else
    title="$PROCD_KEY crashed"
fi

curl -fsS -X POST "$NOTIFY_WEBHOOK" \
    --data-urlencode "title=$title" \
    --data-urlencode "body=exit=$PROCD_EXIT_CODE signal=$PROCD_EXIT_SIGNAL uptime=${PROCD_UPTIME_SECONDS}s attempt=$PROCD_RESTART_COUNT/$PROCD_RESTART_MAX_RETRIES"
```

## API

Die CLI ist ein Client dieser API. Andere Programme können den Socket direkt
ansprechen. Das Protokoll ist kein HTTP, sondern an JSON-RPC 2.0 angelehnt:
**eine JSON-Zeile pro Request** (mit `\n` abgeschlossen), eine JSON-Zeile pro
Antwort. Über eine Verbindung können mehrere Requests nacheinander laufen. Nach
60s ohne Request wird sie geschlossen.

```json
→ {"id": 1, "method": "process.start", "params": {"key": "worker"}}
← {"id": 1, "result": {"process": {"key": "worker", "state": "running", "pid": 4711, ...}}}
← {"id": 1, "error": {"code": -32004, "message": "process \"worker\" is already running"}}
```

```bash
printf '{"id":1,"method":"process.list"}\n' | nc -U /run/procd.sock -w 1
```

### Methoden

| Methode | Params | Ergebnis | Von `--readonly` blockiert |
|---|---|---|---|
| `process.list` | – | `{"processes": [ProcessInfo, ...]}` | nein |
| `process.get` | `{"key": string}` | `{"process": ProcessInfo}` | nein |
| `process.start` | `{"key": string}` | `{"process": ProcessInfo}` | ja |
| `process.stop` | `{"key": string}` | `{"process": ProcessInfo}` | ja (und per-Prozess `readonly`) |

### `ProcessInfo`

| Feld | Beschreibung |
|---|---|
| `key`, `cmd` | aus der Config |
| `state` | siehe [Zustände](#zustände) |
| `pid`, `started_at`, `uptime_seconds` | nur während der Prozess läuft |
| `rss_bytes`, `cpu_percent` | aus `/proc` gesampelt, nur während der Prozess läuft; `cpu_percent` ist nicht pro Kern normalisiert (wie bei `top`) |
| `readonly`, `autostart`, `stdout`, `logfile` | aus der Config |
| `exit_code`, `exit_signal` | gesetzt, sobald der Prozess mindestens einmal beendet wurde |
| `schedule`, `schedule_action` | aus der Config, nur bei Prozessen mit Zeitplan |
| `next_run` | nächster geplanter Termin (RFC 3339) |

Felder ohne Wert werden weggelassen.

### Fehlercodes

| Code | Bedeutung |
|---|---|
| `-32700` | Zeile ist kein gültiges JSON |
| `-32600` | `method` fehlt |
| `-32601` | unbekannte Methode |
| `-32602` | `params.key` fehlt oder ist leer |
| `-32603` | interner Fehler (z.B. Start fehlgeschlagen) |
| `-32001` | unbekannter `key` |
| `-32002` | procd läuft mit `--readonly`, verändernde Methoden sind verboten |
| `-32003` | Prozess hat `readonly = true` und kann nicht gestoppt werden |
| `-32004` | bereits im Zielzustand (Start auf laufendem, Stop auf gestopptem Prozess) |

## Build

```bash
cargo build --release                                   # → target/release/procd
cargo build --release --target x86_64-unknown-linux-musl  # statisch, für Container
cargo test
```

## Einschränkungen

- Nur Linux: Metriken kommen aus `/proc`, Signale gehen an Prozessgruppen.
- Neustart nur bei Absturz, nicht nach Exit-Code 0.
- `shutdown_timeout` gilt global, nicht pro Prozess.
- Prozesse starten parallel, ohne Reihenfolge oder Abhängigkeiten.
- Zeitpläne mit Minuten-Auflösung; Sekunden-Felder (6 Felder) werden nicht unterstützt.

## Umstieg von process-ctl

procd ist die Rust-Neufassung von `process-ctl`. Bestehende
`process.toml`-Dateien funktionieren unverändert. Protokoll, Methoden,
Felder und Fehlercodes der API sind gleich geblieben. Geändert hat sich:

- **Aufruf:** `-config`/`-readonly` heißen jetzt `-c`/`--config` und
  `-r`/`--readonly`. Der alte Aufruf bricht mit einer Fehlermeldung ab.
- **Hook-Variablen:** `PROCESS_CTL_*` heißen jetzt `PROCD_*`.
- **Default-Socket:** `/run/procd.sock` statt `/run/observer.sock`. Das
  betrifft nur Configs ohne `core.sock`.
- **Absturzerkennung:** Ein Absturz wird jetzt auch erkannt, wenn ein
  Kindprozess die Ausgabe-Pipe offen hält. process-ctl blieb in diesem Fall
  dauerhaft in `running`.
- **Hook-Umgebung:** Der Hook erbt immer die vollständige Umgebung.
  process-ctl übergab ohne `envfile`/`env` nur die Hook-Variablen, also auch
  kein `PATH`.
- Ein per-Prozess-`logfile` wird relativ zur `process.toml` aufgelöst, wie
  alle anderen Pfade.
