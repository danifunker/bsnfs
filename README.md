# bsnfs

Share a folder on your computer with NFS clients, old and new, using one
command. No kernel NFS server, no `/etc/exports`, no root. Why this exists:
[GOAL.md](GOAL.md).

- NFS v2 and v3, over UDP and TCP
- Built-in portmapper, mount service and lock manager
- A web page for managing shares
- Clients are locked inside the folders you share
- Runs on macOS, Linux and Windows, by hand or as a service, or in Docker

## Getting started

### 1. Install it

Download the file for your system from the
[Releases](https://github.com/danifunker/bsnfs/releases) page:

| System | Download | Then |
| --- | --- | --- |
| Windows 10/11 (x64 or 32-bit x86), Windows 11 on ARM | `bsnfs-setup-windows-x64-….exe` (or `x86`, `arm64`) | Run it. It can set bsnfs up as a service and opens the admin page. |
| Debian, Ubuntu | `bsnfs-linux-x64-….deb` (or `arm64`, `riscv64`) | `sudo apt install ./bsnfs-linux-*.deb` |
| Fedora, RHEL, openSUSE | `bsnfs-linux-x64-….rpm` | `sudo dnf install ./bsnfs-linux-*.rpm` |
| Arch | `bsnfs-linux-x64-….pkg.tar.zst` | `sudo pacman -U bsnfs-linux-*.pkg.tar.zst` |
| Any Linux | `bsnfs-linux-x64-….AppImage` or `.tar.gz` | `chmod +x` the AppImage and run it, or unpack the tarball. |
| macOS 10.13+ (Intel), 11+ (Apple silicon) | `bsnfs-macos-universal-….tar.gz` | Unpack, then `sudo mv bsnfs /usr/local/bin/`. |

Every Linux build is a single static program, so it runs on any distribution.
There's a new release for every change; the version is its date and time
(e.g. `2026.10.27.015312`).

Or build it yourself with [Rust](https://rustup.rs): `cargo build --release`
puts the program at `target/release/bsnfs`.

### 2. Share a folder

```sh
bsnfs ~/Shared
```

That shares `~/Shared` under the name `/Shared`. You'll see something like:

```
bsnfs 0.1.0 serving NFS v2/v3 on 0.0.0.0:2049 (udp+tcp)
  portmapper on port 111
  /Shared -> /Users/you/Shared
  e.g.: mount -t nfs -o vers=3 THIS_HOST:/Shared /mnt
```

Leave it running. Press **Ctrl-C** to stop sharing.

More examples:

```sh
bsnfs irix=~/IrixStuff              # share as /irix instead of /IrixStuff
bsnfs irix=~/IrixStuff pics=~/Pictures   # several folders at once
bsnfs -r ~/Shared                   # read-only
```

On macOS you may be asked to allow incoming network connections; say yes.

### 3. Connect from another machine

Replace `server` with your computer's name or IP address.

| Client | Command (run as root) |
| --- | --- |
| IRIX, SunOS, old BSDs (NFSv2) | `mount -o vers=2 server:/Shared /mnt` |
| IRIX 6.x, Linux (NFSv3) | `mount -t nfs -o vers=3 server:/Shared /mnt` |
| macOS | `mount -t nfs -o vers=3 server:/Shared /private/tmp/mnt` |
| Windows (NFS client feature) | `mount -o anon \\server\Shared Z:` |

You can also mount a subfolder (`server:/Shared/projects`). If you share only
one folder, `server:/` works too.

To check from the client that the server is visible: `showmount -e server`.

### 4. Manage shares in the browser

While bsnfs is running, open <http://127.0.0.1:8080/>. From there you can:

- add a folder (with a folder picker) or stop sharing one
- switch a share between read-only and read-write
- copy the mount command for each share
- see which machines have what mounted, and recent activity

Shares you add here disappear when bsnfs stops, unless you start it with a
file to save them in:

```sh
bsnfs -c ~/.bsnfs-exports
```

To use the page from another machine, give it a password (it won't start
without one): `bsnfs --admin 0.0.0.0:8080 --admin-password something ~/Shared`.

### 5. Run it as a service

To have bsnfs start at boot and keep running without a terminal:

```sh
sudo bsnfs service install ~/Shared        # macOS and Linux
bsnfs service install C:\Shared            # Windows, as Administrator
```

This registers bsnfs with the system's own service manager (systemd, launchd,
or Windows Services), writes a config file, and starts it. On macOS and Linux
it runs as you (the user who ran `sudo`), so your permissions apply; pass
`--user NAME` to pick another account.

Settings live in a plain-text config file, one option per line:

| System | Config file | Restart after editing |
| --- | --- | --- |
| Linux | `/etc/bsnfs/bsnfs.conf` | `sudo systemctl restart bsnfs` |
| macOS | `/Library/Application Support/bsnfs/bsnfs.conf` | `sudo launchctl kickstart -k system/io.github.danifunker.bsnfs` |
| Windows | `C:\ProgramData\bsnfs\bsnfs.conf` | `Restart-Service bsnfs` |

```
# Folders to share
share irix=/srv/irix
share pics=/home/you/Pictures

# Admin page, reachable from the network
admin 0.0.0.0:8080
admin-password change-me

read-only
```

Any option from [All options](#all-options) works, without the dashes. The
same file works by hand: `bsnfs --config myconfig.conf`.

To remove the service: `sudo bsnfs service uninstall` (the config is kept).

Notes:

- **Linux packages** (deb, rpm, Arch) install the service for a dedicated
  `bsnfs` account but don't start it. Add `share` lines to
  `/etc/bsnfs/bsnfs.conf` and run `sudo systemctl enable --now bsnfs`, making
  sure the `bsnfs` account can read the folders. Or run it as yourself
  instead with `sudo bsnfs service install --user you /path/to/folder`.
- **macOS** keeps services out of Desktop, Documents, Downloads, iCloud
  Drive and external disks. To share a folder there, add the `bsnfs` program
  under System Settings > Privacy & Security > Full Disk Access.
- **Windows**: the service runs as LocalSystem, which can read every file,
  unless you install it with `--user NAME --password PW`. Its log is
  `C:\ProgramData\bsnfs\bsnfs.log`.

## Who can do what

Two layers, on purpose:

- **On the client, everything is allowed.** Every file appears to be owned by
  whichever user is looking at it, and every access check passes. So any
  client user can `cd` anywhere in the share, list, read, and create files.
- **On your computer, the normal rules apply.** bsnfs reads and writes files
  as the user who started it. New files belong to that user and get its
  default permissions (usually `rw-r--r--`). Anything that user isn't allowed
  to touch fails on the client with "Permission denied".

So run bsnfs as yourself, not as root, unless you really want clients to be
able to change everything.

On the client, `chmod` and `chown` appear to succeed but change nothing.
Changing a file's size or timestamps does work (`touch`, `tar`, `rsync -t`).
A file that's executable on your computer shows as executable on the client.

## What clients can't do

bsnfs is meant to keep old, unauthenticated NFS clients inside the folders
you share. Clients can't get out of a shared folder, however they try:

- **No path tricks.** Every name a client sends must be a single plain
  name; `..` stops at the top of the share.
- **No symlink escapes.** Clients can see and create symlinks, but bsnfs
  never follows one on your computer, so a link to `/etc` inside a share
  can't be used to reach `/etc`. On macOS and Linux this holds even if a
  client races its own requests to swap a folder for a symlink mid-operation:
  bsnfs walks each path one step at a time and refuses any link along the way.
  (Windows: best effort, see Limitations.)
- **No handles to anything else.** A file handle can only name something
  already inside the share; forged or old handles are refused.
- **No reaching the admin page from websites.** It only answers to its own
  address, `localhost`, IP addresses and this machine's name, so a web page
  can't trick your browser into using it. Off this machine it requires a
  password.

You can tighten things further:

- `--share-root /srv/nfs` lets the admin page share (and browse) only folders
  inside `/srv/nfs`.
- `--no-symlinks` stops clients creating symlinks. bsnfs never follows them,
  but other programs on your computer (backup tools, file browsers, a future
  FTP or HTTP service) might.
- `--emulate-symlinks` stores client symlinks as small files instead (see
  below), which no program on your computer will follow.
- `--read-only`, and `--bind` with a firewall, to limit who can reach it.

What it doesn't protect against: anyone who can reach the server can mount
it (NFS v2/v3 have no real authentication), and they can do anything the
account bsnfs runs as can do *inside* the shared folders.

### Symlinks on Windows (and FAT/exFAT disks)

Old Unix software expects symlinks, but Windows (and FAT or exFAT disks)
can't hold real ones. There, bsnfs stores a client's symlink `lib` as an
ordinary file named `symlink-lib`, containing a marker line and the link's
target. Clients see a normal symlink (`ls -l`, `readlink`, `tar` all work);
on your computer it's just a small text file nothing will follow. A file
only counts as a link if it starts with the marker, so your own files named
`symlink-...` are left alone.

This is always on for Windows hosts; elsewhere use `--emulate-symlinks`.

## Running in Docker

```sh
docker compose up -d
```

Before that, edit `docker-compose.yml`:

- replace `./shared` with the folder you want to share
- change `BSNFS_ADMIN_PASSWORD` (required: the container won't start without
  one, because its admin page listens on the network)
- set `user: "1000:1000"` to your own user and group IDs (`id -u`, `id -g`),
  so files clients create belong to you

Then open `http://<docker-host>:8080/`. To share more folders, add them as
volumes in `docker-compose.yml` and share them from the web page; the list is
saved in the container's `/config` volume.

## Troubleshooting

**"cannot listen on 0.0.0.0:2049: Address already in use"**
Another NFS server is running (on macOS: the built-in one; `sudo nfsd stop`).
Or pick another port: `bsnfs -p 12049 ~/Shared`. Clients find the port on
their own through the portmapper.

**Linux: a warning about the portmapper at startup**
Ports below 1024 (the portmapper's 111) need root on Linux, so bsnfs falls
back to registering with the system's portmapper, which usually needs root
too. Any one of these fixes it:

- run it once with `sudo`, or allow the port without root:
  `sudo setcap cap_net_bind_service=+ep target/release/bsnfs`
- or skip the portmapper: `bsnfs --portmap off ~/Shared`, and mount with
  `-o vers=3,port=2049,mountport=2049,nolock`

**The host already runs rpcbind on 111**
That's fine: bsnfs registers with it instead of running its own. In Docker,
remove the two `111` lines from `docker-compose.yml` and mount with
`-o vers=3,port=2049,mountport=2049,nolock`.

**Mount hangs or times out**
Check the client can reach the server (`showmount -e server`), and that no
firewall blocks TCP/UDP ports 2049 and 111.

**"Permission denied" when writing from the client**
Either the share is read-only, or the user running bsnfs can't write that
file on your computer.

**"Stale file handle"**
bsnfs was restarted, or the file was renamed or deleted directly on your
computer while a client was using it. `cd` out and back in, or reopen the
file. No remount needed.

**The admin page says "unexpected Host name"**
You opened it by a name it doesn't know (to stop websites abusing it, it
answers only to IP addresses, `localhost`, `*.local` and this machine's own
name). Use the IP address, or allow the name: `--admin-host nas.example.lan`.

**Seeing what's happening**
Run with `-v` to log every request, or `-vv` to also log errors. The web page
shows recent requests too.

## All options

```
bsnfs [OPTIONS] [[NAME=]DIR]...
bsnfs service install [--user NAME] [--config FILE] [[NAME=]DIR]...
bsnfs service uninstall

  -C, --config FILE        read options from FILE first (one per line, no dashes)
  -p, --port PORT          NFS port; MOUNT and the lock manager share it (default 2049)
      --mount-port PORT    run MOUNT on its own port instead
      --nlm-port PORT      run the lock manager on its own port instead
      --portmap MODE       auto | own | system | off (default auto)
      --portmap-port PORT  (default 111)
  -b, --bind ADDR          address to listen on (default 0.0.0.0, all)
      --versions LIST      NFS versions to serve (default 2,3)
      --no-udp, --no-tcp   drop a transport
      --no-nlm             don't answer lock requests (clients need -o nolock)
  -r, --read-only          share everything read-only
      --admin ADDR:PORT    web page address (default 127.0.0.1:8080)
      --no-admin           no web page
      --admin-password PW  password for the web page (or BSNFS_ADMIN_PASSWORD);
                           required when the page listens beyond this machine
      --admin-host NAME    also answer the web page under this host name
      --share-root DIR     the web page may only share folders inside DIR (repeatable)
      --no-symlinks        refuse client requests to create symlinks
      --emulate-symlinks   store client symlinks as `symlink-NAME` files
                           (always on for Windows hosts)
  -c, --exports-file FILE  load shares from FILE; web page changes are saved there
      --log-file FILE      log to FILE instead of the terminal
  -v / -vv / -q            log every request / also errors / warnings only
  -V, --version
  -h, --help
```

The exports file is plain text, one share per line: `NAME<TAB>PATH`, with
`<TAB>ro` on the end for read-only.

## Limitations

- No authentication: any machine that can reach the server can mount it. Use
  `--bind`, a firewall, or `-r` to limit that.
- File locks are always granted but not enforced between clients.
- NFSv2 can't handle files over 4 GiB (a protocol limit).
- No NFSv4 yet. (NFSv1 was never released, so v2 is the oldest there is.)
- Windows hosts: built and tested by CI, but not yet tried on a real
  machine. Containment there is best effort: each step of a path is checked
  for symlinks and junctions, but a client racing its own requests could in
  principle slip one in between check and use. Client symlinks are emulated.

## How it works

```
src/
  vfs.rs        the shared folders: ID↔path tables, made-up attributes, file I/O
  xdr.rs        XDR encoding
  rpc.rs        ONC RPC: parsing, dispatch, retransmission cache, statistics
  server.rs     UDP and TCP listeners
  portmap.rs    portmapper / rpcbind, and registering with the system's
  nfs/          NFS v2 + v3, MOUNT, lock manager
  admin.rs      web page server + JSON API (page: admin.html)
  app.rs        wires it together
```

- **File IDs are handed out as files are seen**, as in iris. Each share looks
  like its own filesystem with its root at ID 2. IDs 0 and 1 are never used:
  IRIX's file monitor crashes the kernel on inode 1.
- **Attributes are made up**, the same on every host: folders `0755` with a
  link count of 2 plus their subfolders (old `find` relies on that), files
  `0644` (`0755` if executable on the host), owner = the asking user. Free
  space is the host disk's real free space.
- **`.` and `..` are answered by the server** and `..` stops at the share's
  top, so `cd ..` works and can't leave the share. Symlinks are shown as
  symlinks and never followed on the server.
- **Old clients get replies they can read**: directory listings never exceed
  the size the client asked for.
- **Restarts can't mix up files**: handles from a previous run come back as
  "stale" rather than pointing at a different file; the share's root stays
  valid.
- **Retransmitted requests** (common over UDP) get their original reply, so a
  repeated create or delete doesn't fail spuriously.

## Roadmap

- NFSv4
- Real file locking
- Per-share client allow-lists
- More services on the same shares: FTP, HTTP/WebDAV
- A tested Windows build
- Optionally keep file handles valid across restarts

## Development

```sh
cargo test                   # unit tests + end-to-end tests over real sockets
cargo clippy --all-targets
```

### Making releases

Everything is built by the scripts in `scripts/dist/`, which GitHub Actions
runs. `.github/workflows/`:

- `release.yml`: **every push to `main` is a release.** It runs the tests,
  builds every platform, and publishes the result as release
  `YYYY.MM.DD.HHMMSS` (the commit's UTC time, tagged `v…`), marked as the
  latest, with the commits since the previous release in its notes.
- `build.yml`: the builds themselves; run it by hand (Actions > Build > Run
  workflow) to try a branch without releasing anything.
- `ci.yml`: lint and test on Linux, macOS and Windows, for pull requests and
  other branches.

What gets built: Windows x86, x64 and ARM64 (zip and installer; Windows 10+,
ARM64 for Windows 11); macOS as one universal binary (10.13+ Intel, 11+ Apple
silicon); Linux x64, ARM64 and RISC-V 64 as static binaries in tarballs, deb,
rpm and Arch packages, plus AppImages for x64 and ARM64. riscv64 is
cross-built on x86-64 and checked under QEMU.

For a notarized macOS binary, add these repository secrets (otherwise it's
signed ad hoc, and macOS asks users to approve it on first run):

| Secret | What it is |
| --- | --- |
| `MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PWD` | base64 of a .p12 with your **Developer ID Application** certificate and key, and its password |
| `APP_STORE_CONNECT_KEY_ID`, `APP_STORE_CONNECT_ISSUER_ID`, `APP_STORE_CONNECT_PRIVATE_KEY` | an App Store Connect API key (base64 of the .p8), for notarization |

(or `MACOS_NOTARIZE_APPLE_ID`, `MACOS_NOTARIZE_PASSWORD`, `MACOS_TEAM_ID`
instead of the API key). Then set the repository variable
`REQUIRE_NOTARIZATION=true` so a release can't ship without it.

To build locally: `scripts/dist/package-macos.sh`, `package-linux.sh` (on
Linux) or `package-windows.sh` (Git Bash on Windows); files land in `dist/`.

## License

MIT; see [LICENSE](LICENSE). Parts of the protocol code and the release
tooling come from [IRIS](https://github.com/techomancer/iris) and remain under
its BSD 3-Clause license; see [LICENSE-iris.txt](LICENSE-iris.txt).
