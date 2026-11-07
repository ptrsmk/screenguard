# ScreenGuard server in a Proxmox LXC

Two scripts, both published with every release:

- **`pve-screenguard`** runs on the Proxmox host. It creates the container,
  takes a snapshot before each update and drives the updater inside the container.
- **`screenguard-update`** runs inside the container (any Debian with systemd works).
  It installs each release side by side in `/opt/screenguard/releases/<tag>`, with
  its own Python venv built from the hash-pinned `webui/requirements.lock`, and
  switches `/opt/screenguard/current` atomically.

The server and web UI run natively under systemd as an unprivileged `screenguard`
user. Idle, the server uses about 5 MB of RAM and the web UI about 60 MB.

## Install

On the Proxmox host:

```bash
curl -fsSLO https://github.com/<owner>/<repo>/releases/latest/download/pve-screenguard
install -m 0755 pve-screenguard /usr/local/sbin/
pve-screenguard create --ip 192.168.1.50/24 --gw 192.168.1.1   # or leave out --ip for DHCP
```

This creates an unprivileged Debian 13 container (1 vCPU, 512 MB RAM, 256 MB swap,
4 GB disk, start on boot), turns on automatic Debian security updates and installs
the latest release. Then open `http://<ip>:5000` and create the administrator account.

Agents should connect with a fixed address in `/etc/screenguard/agent.toml`:
`server_url = "ws://<ip-or-name>:8080"`. mDNS is off in the generated `server.toml`.

## Update

```bash
pve-screenguard update <vmid>            # latest release
pve-screenguard update <vmid> v0.10.18   # a specific one
```

1. Pushes that release's `screenguard-update` (checksum-verified) into the container.
2. Snapshots the container (`sgpre_<UTC time>`, newest 3 kept). On storage that can't
   snapshot, it warns and continues.
3. Inside the container: downloads and verifies the release, backs up the SQLite
   database to `/var/backups/screenguard`, switches, restarts, and waits until
   `/api/v1/version` reports the new version and the web UI answers.
4. If that check fails, it restores the database backup and the previous release
   automatically, and deletes the failed release.

## When something goes wrong

| | |
|---|---|
| `pve-screenguard status <vmid>` | Releases installed, services, version, snapshots |
| `pve-screenguard rollback <vmid> [tag]` | Back to the previous (or given) release. Code only, database untouched |
| `pve-screenguard restore <vmid> [snapshot]` | Whole container back to a pre-update snapshot. Loses data since then |
| `journalctl -u screenguard-server -u screenguard-webui` | Logs, inside the container |

## Files in the container

| Path | |
|---|---|
| `/etc/screenguard/update.conf` | Updater settings: repo, ports, how many releases/backups to keep |
| `/etc/screenguard/server.toml` | Server config (created once, never overwritten) |
| `/etc/screenguard/server.env`, `webui.env` | Generated secrets, mode 0600 |
| `/var/lib/screenguard/server.db` | Database |
| `/var/backups/screenguard/` | Pre-update database backups (newest 10) |
| `/etc/systemd/system/screenguard-*.service` | Rewritten by every update |

Both scripts default to the repository that published them, and use
`SCREENGUARD_REPO=owner/repo` if it is set.
