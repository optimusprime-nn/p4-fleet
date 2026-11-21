# P4 Fleet

One place to find, watch and update all your ESP32-P4 devices: the ADS-B,
ONO and Frigate displays, the NTS time servers and whatever comes next.

```text
 ┌──────────── P4 device ────────────┐          ┌──────── p4fleet (Docker) ────────┐
 │ app (ONO display, NTS server...)  │  mDNS    │ discovery  ── _p4fleet._tcp       │
 │   └─ fleet_agent component ───────┼─────────▶│ poller     ── GET /fleet/info     │
 │        GET  /fleet/info           │          │ rollouts   ── POST /fleet/ota     │
 │        POST /fleet/ota  (token)   │◀─────────┤ SQLite, web UI, /metrics, webhook │
 │        POST /fleet/reboot|revert… │          └───────────────────────────────────┘
 └───────────────────────────────────┘
```

## What it does

- **Discovery**: devices announce `_p4fleet._tcp` over mDNS and appear by
  themselves. You can also add a device by IP address.
- **Health**: every 30 s it records uptime, Wi-Fi RSSI, free and minimum
  internal heap and free PSRAM (24-hour sparklines in the UI). It also stores
  the reset reason and the last panic, plus whatever status the app reports
  (ONO poll state, NTS offset...).
- **Events**: offline, back online, crash (panic or watchdog, with the crash
  text), unexpected reboot, firmware changed, updates and rollbacks. Optionally
  each one is POSTed to a webhook.
- **Firmware updates**: upload an app `.bin`. The server reads its project,
  version and ELF SHA-256 from the image and offers it to the devices running
  that project.
  - **Single device**: pick a version in the device panel.
  - **Rollout**: one device at a time, waiting for each to boot the new image
    *and confirm it* before moving on. It stops at the first failure.
  - **Rollback detection**: if the device comes back on the old image after the
    reboot, its bootloader rolled the update back. The update is marked
    `rolled_back` and the rollout stops.
- **Actions**: identify (blink the backlight), reboot, revert to the previous
  slot.
- **Prometheus**: `/metrics` exports online state, uptime, RSSI and heap per
  device. Every numeric or boolean value an app puts in `info.app` is exported
  too, as `p4fleet_app_value{key=...}`.

## Run the server

```bash
cp .env.example .env        # set FLEET_DEVICE_TOKEN and FLEET_ADMIN_TOKEN
docker compose up -d --build
```

Open `http://<docker-host>:8790/`. The compose file uses `network_mode: host`
so mDNS reaches the devices. On a bridge network set `FLEET_MDNS=0`, publish
port 8790 and add devices by IP.

Without Docker: `cd server && cargo run --release`. The same environment
variables apply, and data goes to `./data`.

### Trying it without hardware

`server/examples/fake_device.rs` pretends to be a P4 running the agent. It
announces itself over mDNS, accepts firmware, "reboots", and can simulate a
bootloader rollback:

```bash
cd server
cargo run --example fake_device -- mkimage /tmp/nts-1.1.0.bin nts_server 1.1.0
cargo run --example fake_device -- serve 18081 a1b2c3000001 nts_server 1.0.0          # updates fine
cargo run --example fake_device -- serve 18082 a1b2c3000002 nts_server 1.0.0 rollback # new image "crashes"
FLEET_DEVICE_TOKEN=test FLEET_POLL_S=5 cargo run
```

Upload the image in the UI and press **Roll out**. The first device updates;
the second comes back on 1.0.0, so its update is marked `rolled_back` and the
rollout halts.

On Windows without Visual Studio, the `x86_64-pc-windows-gnu` toolchain works,
with WinLibs MinGW (MSVCRT) `mingw64\bin` on `PATH` for the C parts (SQLite,
TLS).

| Variable | Default | |
|---|---|---|
| `FLEET_DEVICE_TOKEN` | – | Shared secret for OTA, reboot, revert and identify. Must match `CONFIG_FLEET_AGENT_TOKEN` on the devices. Unset = monitor-only |
| `FLEET_ADMIN_TOKEN` | – | Protects the UI, the API and `/metrics` (Bearer token; the browser asks once) |
| `FLEET_LISTEN` | `0.0.0.0:8790` | |
| `FLEET_DATA_DIR` | `./data` (`/data` in Docker) | SQLite database and firmware files |
| `FLEET_POLL_S` | `30` | Poll interval |
| `FLEET_RETENTION_DAYS` | `14` | Health samples (events are kept 4× longer) |
| `FLEET_OTA_TIMEOUT_S` | `600` | Time a device gets to reboot into new firmware and confirm it |
| `FLEET_MDNS` | `1` | `0` turns discovery off |
| `FLEET_WEBHOOK_URL` | – | Every event is POSTed as `{"event": {...}, "device_name": "..."}` |

## Add the agent to a P4 app

The component is in [agent/fleet_agent](agent/fleet_agent). It knows nothing
about the app; the app passes callbacks.

1. Depend on it in `main/idf_component.yml`:

   ```yaml
   fleet_agent:
     git: https://github.com/optimusprime-nn/p4-fleet.git
     path: agent/fleet_agent
     version: main            # dependencies.lock pins the commit
   ```

2. Turn off mDNS's own Wi-Fi hooks in `sdkconfig.defaults`. The agent registers
   the esp_wifi_remote STA interface itself:

   ```text
   CONFIG_MDNS_PREDEF_NETIF_STA=n
   CONFIG_MDNS_PREDEF_NETIF_AP=n
   CONFIG_MDNS_PREDEF_NETIF_ETH=n
   ```

3. Put the token in `sdkconfig.defaults.local` (git-ignored):
   `CONFIG_FLEET_AGENT_TOKEN="..."`.

4. Give the HTTP server room for 5 more URI handlers
   (`cfg.max_uri_handlers = 16`). Expose an "install from request body" entry
   point in `ota.c` that skips the web upload window. Then start the agent
   after the HTTP server:

   ```c
   const fleet_agent_config_t cfg = {
       .label        = "NTS server",
       .register_uri = ota_register_uri,
       .ota_receive  = ota_receive,     /* verify + install + respond + reboot */
       .revert       = ota_revert,
       .identify     = identify,        /* optional */
       .rssi         = wifi_rssi,       /* optional */
       .fill_status  = fill_status,     /* prev_version, can_revert, last_crash... */
       .fill_app     = fill_app,        /* e.g. offset_us, stratum, gps_lock */
   };
   fleet_agent_start(&cfg);
   ```

The ONO display ([esp32p4-ono-display](https://github.com/optimusprime-nn/esp32p4-ono-display)) is the
reference integration: see `main/fleet.c`.

`status` keys the UI understands: `can_revert`, `prev_version` (enables the
Revert button) and `last_crash` (included in crash events).

## Device API

| | Auth | |
|---|---|---|
| `GET /fleet/info` | none | `id`, `project`, `version`, `elf_sha256`, `partition`, `pending_verify`, `uptime_s`, `reset_reason`, `ip`, `rssi`, `heap{}`, `caps{}`, `status{}`, `app{}` |
| `POST /fleet/ota` | Bearer | raw app image as the body |
| `POST /fleet/reboot` | Bearer | |
| `POST /fleet/revert` | Bearer | boots the other OTA slot |
| `POST /fleet/identify` | Bearer | |

With no token on the device, the write endpoints answer 403 and `caps` says so.
The server then shows the device as monitor-only.

## Security notes

- Device traffic is plain HTTP on your LAN, like the existing upload pages. The
  token stops casual misuse, not someone sniffing the network. Keep the devices
  on a trusted VLAN.
- Firmware is not signed by the fleet server. The device still checks the
  chip, project name and SHA-256 before it switches, and rolls back an image
  that doesn't come up.

## License

MIT, see [LICENSE](LICENSE).
