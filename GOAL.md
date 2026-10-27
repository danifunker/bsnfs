# Goal

Make any folder on a modern computer show up as a normal network drive on
old (and new) Unix machines, with no setup on either side beyond one command.

Classic workstations (SGI IRIX, SunOS, old BSDs and Linuxes) speak NFS v2/v3.
Modern hosts make serving that hard: kernel NFS servers need root,
`/etc/exports`, rpcbind, and matching user IDs, and some platforms have none
at all. bsnfs replaces all of that with a single program:

- **Point it at a folder; it's shared.** No accounts, no ID mapping.
- **Clients can always get in.** Ownership and permissions shown to clients
  are a shim; every request is granted.
- **The host stays in charge.** All file access happens as the user running
  bsnfs, so the host's own permissions decide what really happens.
- **Old clients just work.** It follows the quirks they depend on (`..`
  handled by the server, no inode 1, small directory replies).

It started as the NFS server inside the iris SGI emulator. Later it will host
other simple services (FTP, HTTP) over the same shared folders.
