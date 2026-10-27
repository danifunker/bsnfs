# bsnfs

Share a folder on your computer with NFS clients, old and new, using one
command. No kernel NFS server, no `/etc/exports`, no root. Why this exists:
[GOAL.md](GOAL.md).

- NFS v2 and v3, over UDP and TCP
- Built-in portmapper, mount service and lock manager
- A web page for managing shares
- macOS and Linux, natively or in Docker

## Getting started

### 1. Build it

You need [Rust](https://rustup.rs) installed.

```sh
cargo build --release
```

The program is now at `target/release/bsnfs`.

### 2. Share a folder

```sh
./target/release/bsnfs ~/Shared
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

To use the page from another machine:
`bsnfs --admin 0.0.0.0:8080 --admin-password something ~/Shared`.

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

## Running in Docker

```sh
docker compose up -d
```

Before that, edit `docker-compose.yml`:

- replace `./shared` with the folder you want to share
- change `BSNFS_ADMIN_PASSWORD`
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

**Seeing what's happening**
Run with `-v` to log every request, or `-vv` to also log errors. The web page
shows recent requests too.

## All options

```
bsnfs [OPTIONS] [[NAME=]DIR]...

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
      --admin-password PW  password for the web page (or BSNFS_ADMIN_PASSWORD)
  -c, --exports-file FILE  load shares from FILE; web page changes are saved there
  -v / -vv / -q            log every request / also errors / warnings only
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
- Windows hosts: not built or tested yet.

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
