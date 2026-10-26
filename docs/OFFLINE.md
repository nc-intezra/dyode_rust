# Offline installs

A diode box usually has no internet.  Everything it needs comes in one file,
`dyode-<version>-offline.tar.gz`, attached to every GitHub release.  Carry it
across the air gap, unpack it, and install from it:

```bash
tar xzf dyode-2.0.1-offline.tar.gz && cd dyode-2.0.1
sha256sum -c ../dyode-2.0.1-offline.tar.gz.sha256   # optional, before unpacking
sudo ./install.sh --offline --side in              # input box
sudo ./install.sh --offline --side out             # output box
sudo ./install.sh --offline --side out --with ssh,nfs,smb   # plus file-sharing servers
```

Inside an unpacked bundle, `--offline` is the default.

## What is in the bundle

| Path | Contents |
|---|---|
| `bin/amd64/dyode`, `bin/arm64/dyode` | the program, statically linked (musl, mimalloc): one file per CPU, no library dependencies, whatever the system's glibc |
| `packaging/debs/<os>-<release>/<arch>/` | the OS packages DYODE may need, with their dependencies, for each supported system |
| `source/dyode-<version>-src.tar.gz` | the source with `Cargo.lock` and every dependency vendored: rebuild without a network |
| `install.sh`, `config.example.yaml`, `packaging/`, `docs/`, `tools/` | as in the repository |
| `BUNDLE.txt` | version, commit, build date, contents |
| `SHA256SUMS` | every other file |

DYODE itself is one binary, so the OS packages are only:

* `nftables` -- the output box's diode-port firewall;
* `iproute2` -- `ip neigh`, for the input box's static ARP entry;
* with `--with`: `ssh` (openssh-server, sftp-server), `nfs`
  (nfs-kernel-server), `smb` (samba, smbclient, cifs-utils), for serving the
  received files.

Each is installed only when the box lacks it.

## Supported systems

| System | CPU | Bundle folder |
|---|---|---|
| Ubuntu 24.04 LTS | amd64, arm64 | `ubuntu-24.04/` |
| Ubuntu 26.04 LTS | amd64, arm64 | `ubuntu-26.04/` |
| Debian 12 / Raspberry Pi OS bookworm (64-bit) | amd64, arm64 | `debian-12/` |
| Debian 13 / Raspberry Pi OS trixie (64-bit) | amd64, arm64 | `debian-13/` |

64-bit Raspberry Pi OS takes these packages unchanged from Debian's arm64
archive and reports itself as Debian, so the `debian-*` folders serve it.
32-bit Raspberry Pi OS (armhf) is not covered.

On any other system the binary still runs (it is static); install the
packages above by hand and use `--skip-os-packages`.

## How the installer stays safe

* **Checked before anything changes.**  The whole bundle is verified against
  `SHA256SUMS` first, and any binary or package *not listed* there is
  refused (it would otherwise be installed unchecked).  A copy damaged on its
  way across stops the install instead of leaving it half done.
* **No network, by construction.**  The `.deb` files are installed through a
  private apt repository made at install time from the bundled files alone.
  apt resolves dependencies itself, installs only what is missing, and never
  downgrades a package the host already has newer.
* **The right binary.**  `bin/<cpu>/dyode` is chosen from
  `dpkg --print-architecture`, and run once (`dyode version`) before
  anything is installed.

## How the bundle is built

`.github/workflows/release.yml`, on every tag (and by hand, and on pull
requests that touch the installer or tools):

1. **source** -- `cargo generate-lockfile` (if needed) and `cargo vendor`, into
   `dyode-<version>-src.tar.gz`.
2. **static** -- the binaries are built *from that vendored source with
   `--offline`*, on native amd64 and arm64 runners, which proves the source
   tarball rebuilds without a network.
3. **debs** -- `tools/build_debs.sh`, one clean container per system, with
   apt `--download-only`.
4. **bundle** -- `tools/build_bundle.sh` assembles and checksums it.
5. **verify** -- for every system: a clean container with `--network none`
   unpacks the bundle and installs both sides from it (with
   `--with ssh,nfs,smb`), runs `dyode check`, and confirms that a damaged
   copy is refused.
6. **publish** (tags only) -- the bundle, the source tarball and the bare
   binaries are attached to the release, with `SHA256SUMS`.

By hand, on a machine with internet, Docker and musl-tools:

```bash
tools/build_static.sh                     # this CPU's static binary
TARGETS="debian-13/arm64" tools/build_debs.sh
tools/build_bundle.sh --bin arm64=target/aarch64-unknown-linux-musl/release/dyode
```

## Rebuilding from source, offline

```bash
tar xzf source/dyode-2.0.1-src.tar.gz && cd dyode-2.0.1-src
cargo build --release --offline --locked          # or: OFFLINE=1 tools/build_static.sh
```

Needs a Rust toolchain (1.85 or newer) on the build machine; nothing is
downloaded.

## Differences from DYODE (Python) 1.5

* The bundle is a **release asset**, not committed to the repository: it
  holds binaries, which do not belong in git history.  Clone-and-carry still
  works for a source build; for an install, carry the release tarball.
* No wheels, no virtualenv, no udpcast, no Python: the binary replaces them.
* `--with ssh,nfs,smb` installs the servers; the share configuration that the
  Python installer also wrote (`/etc/exports`, `smb.conf`) is not ported yet.
  Configure the shares of the `out` folders yourself, and never share the
  `*.incoming` staging folders.
