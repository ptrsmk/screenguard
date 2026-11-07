# Running the ScreenGuard server in a Proxmox LXC

This guide sets up the ScreenGuard server and web UI in a small unprivileged
Debian container on Proxmox VE, keeps it updated with one command, and covers
what to do when something goes wrong. It was written while doing a real
deployment (Proxmox VE 8.4, Debian 13 container); the lessons from that run are
in [Things learned the hard way](#things-learned-the-hard-way).

## Contents

1. [How it fits together](#how-it-fits-together)
2. [Before you start](#before-you-start)
3. [Pick an IP address](#pick-an-ip-address)
4. [Create the container](#create-the-container)
5. [First steps after install](#first-steps-after-install)
6. [DNS name](#dns-name)
7. [Reaching it from outside your network](#reaching-it-from-outside-your-network)
8. [Connecting agents](#connecting-agents)
9. [Updating](#updating)
10. [When something goes wrong](#when-something-goes-wrong)
11. [Backups and monitoring](#backups-and-monitoring)
12. [Files in the container](#files-in-the-container)
13. [Things learned the hard way](#things-learned-the-hard-way)
14. [Maintaining a fork](#maintaining-a-fork)
15. [Removing it](#removing-it)

## How it fits together

Two scripts, both published with every release:

- **`pve-screenguard`** runs on the Proxmox host. It creates the container,
  takes a snapshot before each update and runs the updater inside the container.
- **`screenguard-update`** runs inside the container (any Debian with systemd works).
  It installs each release side by side in `/opt/screenguard/releases/<tag>`, each
  with its own Python environment built from the hash-pinned `webui/requirements.lock`,
  and switches `/opt/screenguard/current` in one atomic step.

Inside the container, two systemd services run as an unprivileged `screenguard` user:

| Service | Port | What it is |
|---|---|---|
| `screenguard-server` | 8080 | Rust server: REST API under `/api/v1/` (web UI and phone app), WebSocket `/ws` (agents), SQLite database |
| `screenguard-webui` | 5000 | Flask web UI under gunicorn; talks to the server on `localhost:8080` |

No Docker. Measured on a real install: the server uses about 3 MB of RAM and the
web UI about 60 MB; the whole container uses about 125 MB of its 512 MB and under 1 GB
of its 4 GB disk.

Each script uses the GitHub repository it was published from: a fork's release
installs that fork's builds. Set `SCREENGUARD_REPO=owner/repo` to use a different one.

## Before you start

- **Proxmox VE 8.4 or newer**, and a root shell on the host (web UI → node → Shell, or SSH).
- **Internet access** from the host and the container (GitHub and the Debian mirrors).
- **Storage that can snapshot** for the container's disk: `lvmthin` (Proxmox's default
  `local-lvm`), ZFS or Ceph. Plain directory and NFS storage can't snapshot raw disks;
  updates still work there, but without the whole-container safety net.
- About 600 MB of host RAM to spare, 4 GB of disk, and one free IP address.

Check what you have:

```bash
pveversion
pvesm status                 # storage names, types and free space
free -m                      # memory headroom on the host
grep -A3 '^auto vmbr' /etc/network/interfaces   # bridge, subnet and gateway
```

## Pick an IP address

The agents and the phone app need an address that never changes, so give the
container a static IP (or a DHCP reservation on your router).

Match the subnet and gateway your other containers use. To list what your
containers are configured with:

```bash
for f in /etc/pve/lxc/*.conf; do
  printf '%s %-20s %s\n' "$(basename "$f" .conf)" "$(grep -m1 ^hostname "$f" | cut -d' ' -f2)" \
    "$(grep -m1 ^net0 "$f" | grep -oE 'ip=[^,]+|gw=[^,]+' | tr '\n' ' ')"
done
grep -h '^net' /etc/pve/qemu-server/*.conf     # VMs, which usually use DHCP
```

Proxmox only knows its own containers. Before you choose an address, check that
nothing else on the network (a printer, a phone, a TV) is using it:

```bash
ip=192.168.1.50
ping -c1 -W1 $ip && echo "IN USE" || ip neigh show $ip    # no "lladdr" line means nobody answered
```

Also make sure the address is outside your router's DHCP pool.

## Create the container

On the Proxmox host, download the host script, check it against its published
checksum, and install it:

```bash
repo=adambie/screenguard        # or your fork, e.g. yourname/screenguard
u=https://github.com/$repo/releases/latest/download
curl -fsSLO $u/pve-screenguard
[ "$(curl -fsSL $u/pve-screenguard.sha256 | cut -d' ' -f1)" = "$(sha256sum pve-screenguard | cut -d' ' -f1)" ] && echo OK
install -m 0755 pve-screenguard /usr/local/sbin/ && rm pve-screenguard
```

The `.sha256` files hold only the hash, without a filename, so `sha256sum -c` can't
read them. Compare the hashes as shown above.

Then create the container:

```bash
pve-screenguard create --vmid 123 --hostname screenguard \
    --ip 192.168.1.50/24 --gw 192.168.1.1
```

| Option | Default |
|---|---|
| `--vmid N` | next free ID |
| `--hostname NAME` | `screenguard` |
| `--storage NAME` | `local-lvm` |
| `--template-storage NAME` | `local` (where the Debian template is downloaded) |
| `--bridge NAME` | `vmbr0` |
| `--ip CIDR` / `--gw ADDR` | DHCP |
| `--tag TAG` | latest release |

Use the prefix length your network actually has (`/24`, `/22`, …), not whatever is
typical.

It takes two or three minutes. It:

1. downloads the newest `debian-13-standard` template, if it isn't already there;
2. creates an unprivileged container with 1 vCPU, 512 MB RAM, 256 MB swap and a
   4 GB disk that starts on boot, tagged `screenguard`;
3. waits for the network, installs all Debian updates, and turns on automatic
   security updates (`unattended-upgrades`);
4. copies in the matching `screenguard-update` (checksum-checked) and runs
   `screenguard-update install`. That creates the `screenguard` user, the config,
   and fresh secrets, then installs and starts the release.

It ends by printing the web UI address. If you tag containers with their IP or
other labels, add yours now, for example `pct set 123 --tags "50.123;screenguard"`.

## First steps after install

1. **Create the administrator account straight away** at `http://<ip>:5000`.
   Until an administrator exists, anyone who can reach the web UI or `/api/v1/auth/setup`
   can create one. Do this before making it reachable from outside your network.
2. Use a strong password. The server doesn't limit login attempts.
3. Make children's accounts non-admin.
4. Check that it's healthy:

   ```bash
   pve-screenguard status 123
   curl -s http://<ip>:8080/api/v1/version
   ```

## DNS name

Give the container a name, so agents and bookmarks don't depend on the IP. Use
whatever domain your network already uses. Look at the `search` line in
`/etc/resolv.conf` on the host: `home.arpa` is the standard name for home networks,
and `.lan` or `.home` are common too.

- **AdGuard Home:** Filters → DNS rewrites → add `screenguard.home.arpa` → `<ip>`.
- **Pi-hole:** Local DNS → DNS Records.
- **OPNsense Unbound:** Services → Unbound DNS → Overrides → Host.

Check it from another machine with `nslookup screenguard.home.arpa`.

## Reaching it from outside your network

The Android app talks straight to the server's REST API on port 8080. The web UI
is on port 5000. Agents connect to `/ws` and only need to reach the server on your
local network. A reverse proxy can serve all of it on one HTTPS hostname:

| Path | Goes to |
|---|---|
| `/ws` | **blocked** (403): keeps agent connections local-only |
| `/api/v1/*` | `<ip>:8080` |
| everything else | `<ip>:5000` |

Example Caddyfile:

```caddyfile
screenguard.example.com {
    handle /ws* {
        respond 403
    }
    handle /api/v1/* {
        reverse_proxy 192.168.1.50:8080
    }
    handle {
        reverse_proxy 192.168.1.50:5000
    }
}
```

`handle` blocks are tried as a set, most specific first, so only one of them handles
each request. On OPNsense's Caddy plugin, set up the same thing as one domain with
three handlers.

Only publish it after the administrator account exists. If you can, add brute-force
protection on the proxy (CrowdSec, fail2ban), because the server doesn't limit
login attempts.

## Connecting agents

Install agents with the `install.sh` from the same repository's release, so their
self-updates come from that repository too (`update_repo` in `agent.toml`). Point
them at the server's fixed name, with mDNS discovery off:

```toml
# /etc/screenguard/agent.toml
server_url = "ws://screenguard.home.arpa:8080"
```

mDNS is turned off in the server's generated `server.toml`, so agents only find the
server through this address.

If an agent was installed from a different repository's `install.sh`, the web UI's
"Update" button updates it from that repository, not from yours.

## Updating

```bash
pve-screenguard update 123             # latest release
pve-screenguard update 123 v0.10.18    # a specific one
```

What happens:

1. It downloads that release's `screenguard-update`, checks its checksum, and copies
   it into the container, so the updater always matches the release it installs.
2. It snapshots the container (`sgpre_<UTC time>`, newest 3 kept). On storage that
   can't snapshot it prints a warning and carries on.
3. Inside the container, it downloads and verifies the release and builds its Python
   environment, while the old release keeps running. Then it backs up the SQLite
   database to `/var/backups/screenguard`, switches to the new release, and restarts.
4. It waits until `/api/v1/version` reports the new version and the web UI answers.
   If that doesn't happen within 60 seconds, it restores the database backup and the
   previous release, deletes the failed release, and exits with an error.

Downtime is a few seconds. Logins survive updates and restarts, because the
server's secret for signing logins (the JWT secret) is generated once at install and
kept in `/etc/screenguard/server.env`.

A successful update looks like this:

```
[✓] Snapshot sgpre_20261107T230108 taken
[✓] Downloading yourname/screenguard v0.10.18-d2 (x86_64)
[✓] Checksums OK
[✓] Building the web UI environment
[✓] Database backed up to /var/backups/screenguard/server-20261107T230115Z-pre-v0.10.18-d2.db
[✓] Starting v0.10.18-d2
[✓] ScreenGuard v0.10.18-d2 is running (was v0.10.18-d1)
```

Running `update` on the version that is already installed is harmless: it reports
"Already running" and checks health.

Debian security updates install automatically. Other Debian updates and Proxmox
itself are up to you.

## When something goes wrong

| Command (on the host) | Does |
|---|---|
| `pve-screenguard status 123` | Installed releases, service state, reported version, snapshots |
| `pve-screenguard rollback 123 [tag]` | Back to the previous (or given) release. Code only; the database is left as it is |
| `pve-screenguard restore 123 [snapshot]` | The whole container back to a pre-update snapshot. **Loses all data since then.** Asks you to type the ID to confirm |
| `pct exec 123 -- journalctl -u screenguard-server -u screenguard-webui -n 100` | Recent logs |
| `pct enter 123` | Shell inside the container |

Problems you might hit:

- **Update failed and rolled back.** Read the log lines above the error. Usually
  either a download failed (GitHub or network trouble; just run it again) or the new
  version didn't start (look at `journalctl`). Nothing needs cleaning up.
- **`screenguard-update: No such file or directory` when you run it with `pct exec`.**
  `pct exec` uses a short PATH without `/usr/local/sbin`. Use the full path
  (`pct exec 123 -- /usr/local/sbin/screenguard-update status`), `pct enter`, or
  `pve-screenguard`, which sets the PATH itself.
- **Everyone is logged out after every restart.** `SCREENGUARD_SERVER_JWT_SECRET` is missing
  from `/etc/screenguard/server.env`. The server then makes a new temporary secret on
  each start. The installer writes one; check that the file is there.
- **The web UI loads but says it can't reach the server.** Check
  `systemctl status screenguard-server` and that `SERVER_URL` in
  `/etc/screenguard/webui.env` is `http://127.0.0.1:8080`.
- **Agents don't connect.** Check `server_url` in `agent.toml`, and that the agent
  machine can resolve the name and reach port 8080. A reverse proxy blocking `/ws` is
  expected; agents must use the LAN address.
- **"Could not snapshot container".** The container's storage can't snapshot. Updates
  still work, with database backups as the fallback; move the disk to snapshot-capable
  storage if you want the whole-container safety net.
- **Two services fight over one IP.** If something drops off the network at random,
  check for two containers configured with the same IP (or the same MAC address after
  cloning) using the loop in [Pick an IP address](#pick-an-ip-address).

## Backups and monitoring

- **Backups:** include the container in your normal Proxmox backup job (vzdump or
  Proxmox Backup Server); snapshot mode is fine. The database is
  `/var/lib/screenguard/server.db` (SQLite in WAL mode, so `-wal` and `-shm` files
  next to it are normal). `pct exec 123 -- /usr/local/sbin/screenguard-update backup`
  makes a consistent copy in `/var/backups/screenguard` at any time.
- **Monitoring:** point an uptime monitor at `http://<ip>:8080/api/v1/version` (it
  returns `{"version":"…"}`) and `http://<ip>:5000/login` (200). The server also runs
  with a systemd watchdog, which restarts it if it hangs.

## Files in the container

| Path | |
|---|---|
| `/etc/screenguard/update.conf` | Updater settings (shell syntax): `REPO`, `KEEP_RELEASES=3`, `KEEP_BACKUPS=10`, `SERVER_PORT=8080`, `UI_PORT=5000`, `HEALTH_TIMEOUT=60` |
| `/etc/screenguard/server.toml` | Server config; written once, never overwritten |
| `/etc/screenguard/server.env` | JWT secret, mode 0600 |
| `/etc/screenguard/webui.env` | Web UI session secret, server URL and port, mode 0600 |
| `/var/lib/screenguard/server.db` | Database |
| `/var/backups/screenguard/` | Pre-update database backups (newest 10) |
| `/opt/screenguard/releases/<tag>/` | Installed releases (current, previous and up to 3 in total) |
| `/opt/screenguard/current` | Symlink to the running release |
| `/etc/systemd/system/screenguard-*.service` | Rewritten by every update; don't edit them, change `update.conf` instead |
| `/usr/local/sbin/screenguard-update` | The updater; replaced by `pve-screenguard update` |

Secrets are created inside the container and never leave it. Keep them out of any
repository.

## Things learned the hard way

From the first real deployment:

- **`pct exec` doesn't use the host's PATH or a clean environment.** Commands get
  `PATH=/sbin:/bin:/usr/sbin:/usr/bin` (no `/usr/local/sbin`). Your SSH session's
  `LANG` also comes along, and perl and apt complain about it in a fresh container
  without that locale. `pve-screenguard` now runs every command in the container with
  an explicit `env -i PATH=… LANG=C.UTF-8`. Do the same if you script `pct exec`
  yourself.
- **A test container is not Proxmox.** The updater was tested end to end in a
  Debian 13 container with systemd before the first real run, and the PATH problem
  above still only showed up on Proxmox. Try new host-side changes on the real host
  with a harmless command (`status`, or `update` to the version already installed)
  before relying on them.
- **Proxmox's configs don't show the whole network.** An address no container used
  turned out to be taken by another device on the network. Ping and check
  `ip neigh` before choosing an IP.
- **Look for duplicate IPs while you're there.** Cloned containers can end up
  with the same IP and even the same MAC as the original, and both run "fine" until
  traffic goes to the wrong one.
- **Match the network's DNS domain** (`home.arpa` there) rather than inventing a new
  one. The server name ends up in every agent's config, so choose it before
  installing agents.
- **Use the network's real prefix length.** The deployment network was a `/22` with its
  gateway at `.48.1`. With the more usual `/24`, the gateway would have been outside the
  container's subnet and it would have had no route out at all.
- **Do the admin setup before anything else.** The setup endpoint is open until the
  first administrator exists.
- **Snapshots on `local-lvm` (lvmthin) work** and take only seconds, so
  `pve-screenguard update` snapshots every time.
- **Debian 13 templates work on Proxmox VE 8.4.** `pveam` downloads the newest
  `debian-13-standard` (about 124 MB) even when an older one is cached.

## Maintaining a fork

If you run your own fork, so you aren't dependent on the original project:

- Keep `main` an exact mirror of upstream; this fork's `sync-upstream.yml` action
  updates it daily and opens an issue when upstream tags a release.
- Make a `feat/*` branch from upstream's `main` for each change you want to offer upstream.
- Keep a `deploy` branch (the fork's default branch, so the scheduled action runs) with
  upstream's latest release plus your patches. For each upstream release: rebase
  `deploy` onto the new tag, then tag `vX.Y.Z-dN` (`-d1`, `-d2`, … for your own fixes).
- The release workflow publishes to the fork's own GitHub releases and its own
  `ghcr.io/<owner>` images, and writes the fork's repository into `install.sh` and both
  scripts. Releases must be full releases, not prereleases, because
  `releases/latest` ignores prereleases.
- Never push upstream's tags to the fork: every `v*` tag starts a release build.
- GitHub turns scheduled actions off after 60 days without activity in the repository.
  If the sync issue stops appearing, re-enable the workflow.

## Removing it

```bash
pct stop 123 && pct destroy 123 --purge
rm /usr/local/sbin/pve-screenguard
```

Remove its DNS entry and reverse-proxy route too, and uninstall or repoint the agents.
