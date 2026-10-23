# deploy/

Deployment assets for the headless `ale-server` backend on Debian-family
arm64 systems (Debian 12, Raspberry Pi OS Bookworm, Armbian). The full guide is
[docs/DEPLOY.md](../docs/DEPLOY.md); this file is the quick map.

## Layout

| Path | Installed to (system install) | Purpose |
| --- | --- | --- |
| `systemd/ale-server.service` | `/etc/systemd/system/ale-server.service` | hardened system unit: HTTP API + device channel, user `ale` |
| `systemd/ale-server-user.service` | `/usr/lib/systemd/user/ale-server.service` | user unit for capture/automation inside a graphical session |
| `config/ale-server.toml` | `/etc/ale/ale-server.toml` | commented configuration template matching the serde schema |
| `tmpfiles/ale-server.conf` | `/usr/lib/tmpfiles.d/ale-server.conf` | guarantees `/var/lib/ale` and `/etc/ale` ownership/modes |
| `../scripts/install.sh` | shipped in the release tarball | installer/uninstaller (`--system`, `--user`) |

Binaries are installed below `/usr/lib/ale/` and `/usr/bin/ale-server` is a
symlink; managed state lives in `/var/lib/ale/`.

## Which unit do I need?

* **System unit** (`ale-server.service`, user `ale`) - always safe to enable.
  Serves the HTTP API, the encrypted device channel, pairing and egress. It has
  no access to a graphical session, so capture/automation answer
  `CAPABILITY_UNAVAILABLE`.
* **User unit** (`ale-server.service` under `systemctl --user`) - use it on the
  machine whose screen/keyboard the assistant should control. It inherits the
  desktop session environment and can reach `/dev/uinput` (ydotool/dotool).
  See [docs/DEPLOY.md](../docs/DEPLOY.md#system-vs-user-service) for the
  Wayland/X11 backend setup.

Do not run both units against the same ports on one host.

## Packaging

```bash
# Native arm64 build (on the device)
./scripts/build-debian-arm.sh --with-modeld

# Release tarball: dist/ale-server-<version>-linux-aarch64.tar.gz
./scripts/package-tarball.sh

# Install from an unpacked tarball
sudo ./install.sh --system
./install.sh --user
```

Keep the configuration template in sync with the serde schema in
`ale-device/src/config.rs` and `ale-server/` whenever keys are added.
