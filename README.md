# bcachefs-updatedb

`updatedb` for bcachefs. Instead of walking the tree with readdir and stat, it streams
the dirents btree out of the kernel and writes a [plocate](https://plocate.sesse.net/)
database from it.

```
bcachefs-updatedb build /mnt/fs --output /var/lib/plocate/bcachefs-mnt-fs.db
plocate -d /var/lib/plocate/bcachefs-mnt-fs.db pattern
```

Root is required: the btree query ioctl checks `CAP_SYS_ADMIN`. The filesystem stays
mounted and live, and an encrypted filesystem needs no passphrase, because the kernel
serves the keys it already holds.

## How it works

Keys come from `BCH_IOCTL_QUERY_BTREE_KEYS` on the mount point, not from debugfs and
not from the block device. The ioctl is stateless: the cursor lives in the argument, the
kernel fills a buffer with densely packed unpacked `bkey_i` and the tool steps through it.

Two passes over the dirents btree, all snapshots included. Pass 1 keeps every version of
a name that is a directory or subvolume in some snapshot and resolves the path of every
reachable directory. Pass 2 streams everything and emits `parent_path/name` for each
entry whose parent resolved.

Snapshot visibility is reimplemented in userspace, since bypassing readdir bypasses the
kernel's filtering: an entry is visible in a subvolume when its snapshot is an ancestor
of the subvolume's, the nearest ancestor wins, a whiteout hides the entry, a subvolume
dirent shows only in the subvolume that created it, and only live subvolumes are
entered. Resolved paths are keyed by (snapshot, inode): snapshot subvolumes reuse inode
numbers.

`build` writes the plocate database format directly (same block layout, trigram index,
TurboPFor posting lists and zstd dictionary handoff between runs as `updatedb`), so the
daily job needs neither `plocate-build` nor a temporary path list. `paths` prints the
same stream as text for `plocate-build -p` or for diffing against `find`.

## Performance

Measured with this binary, `/usr/bin/time`, output to `/dev/null` for `paths`, no
configuration file (`--no-conf`). Peak RSS is the process's own; a cgroup reports more
(see Limitations). "Live tree" prunes the snapshots directory; "all" indexes every
snapshot; "snapshot tree nodes" counts every key in the snapshots btree, interior nodes
included.

**p4**: Ryzen 9 7950X3D, 61 GB RAM, kernel 7.1.9, bcachefs DKMS 1.39.2, 13.6 TB
filesystem (8.8 TB used) on four HDDs and two NVMe, 209 subvolumes, 22 992 snapshot tree
nodes, 17.1 M dirent keys (36.3 M keys with whiteouts).

| run | paths | wall | peak RSS | output |
|---|---|---|---|---|
| `stats` (one pass over every key) | 17.1 M keys | 5.9 s | 5 MB | 2.9 M keys/s |
| `paths`, live tree | 13.6 M | 7.7 s | 0.37 GB | |
| `paths`, all snapshots | 252.1 M | 33.4 s | 0.96 GB | |
| `build`, live tree, no dictionary | 13.6 M | 18.6 s | 0.57 GB | |
| `build`, live tree, dictionary from previous db | 13.6 M | 19.8 s | 0.57 GB | 333 MB db |
| `build`, all snapshots, no dictionary | 252.1 M | 186 s | 1.57 GB | 3.43 GB db |

The readdir-based `updatedb` job that used to index the snapshots portion of this
filesystem took 1 h 47 min and 6.98 GB, and produced a 4.07 GB database. Path counts are
not comparable: `updatedb` applied PRUNENAMES, these runs pruned one path. The claim is
the wall time.

**castgti86**: Ryzen 9 7950X3D, 32 GB RAM, kernel 7.2.4, DKMS 1.39.5, 735 GB filesystem
(430 GB used) on LVM-on-LUKS NVMe plus one HDD, 708 subvolumes, 43 028 snapshot tree
nodes, 9.2 M dirent keys (14.0 M with whiteouts).

| run | paths | wall | peak RSS | output |
|---|---|---|---|---|
| `stats` | 9.2 M keys | 2.9 s | 5 MB | 3.2 M keys/s |
| `paths`, live tree | 16.9 M | 4.0 s | 0.20 GB | |
| `paths`, all snapshots | 60.0 M | 12.7 s | 0.35 GB | |
| `build`, live tree, no dictionary | 16.9 M | 17.6 s | 0.45 GB | |
| `build`, live tree, dictionary from previous db | 16.9 M | 21.4 s | 0.45 GB | 457 MB db |
| `build`, all snapshots, no dictionary | 60.0 M | 49.9 s | 0.61 GB | 913 MB db |

**Backup server**: 20-core Neoverse-N1, 39 GB RAM, kernel 7.1.3 (Debian 13), DKMS 1.39.5,
130 TB filesystem (34 TB used) on seven HDDs and four NVMe, 5216 subvolumes of which
most are daily backup snapshots, 276 517 snapshot tree nodes, 138.6 M dirent keys
(149.7 M with whiteouts).

| run | paths | wall | peak RSS | output |
|---|---|---|---|---|
| `stats`, cold btree cache | 138.6 M keys | 195 s | 5 MB | 0.71 M keys/s, disk-bound |
| `paths`, all snapshots | 586.7 M | 166 s | 1.69 GB | |
| `build`, all snapshots, no dictionary | 586.7 M | 965 s | 3.46 GB | 7.90 GB db |
| `build`, all snapshots, dictionary, from the systemd unit (idle IO, cold cache) | 586.7 M | 26.5 min | 1.9 GB | 6.87 GB db |

The readdir-based `updatedb` on this machine indexes the same tree (its database holds
18.44 M blocks to this one's 18.34 M, a day of churn apart) in 11 to 12 h, reading
574 GB, and peaks at 2.3 GB plus 4.3 GB of swap under a 2 GB MemoryHigh. The unit's
cgroup reported 18.7 G peak for the last row, 17 G of it btree cache.

Scanning is independent of the snapshot count: keys shared between snapshots are stored
once and read once. What grows with snapshots is the number of paths emitted, the
directory map (one node per directory per snapshot it is visible in) and the posting
lists, which is where the memory goes.

## Validation

Differential against `find` on a live tree of 10.1 M paths: 13 differing lines, all
browser cache churn between the two runs, in both directions. A 214 K-path subtree
compared byte-identical.

The database writer is checked against `plocate-build` fed the same path list
(`tests/compare-db.sh`): on both machines above, every posting list is byte-identical
and every query, including `-r .` over the whole database, returns the same set. Only
the filename blocks differ, through the zstd dictionary: `plocate-build` trains one from
the full input, this tool takes it from the previous database like `updatedb`.

## Usage

```
bcachefs-updatedb build <mount> --output DB [--prefix P] [--conf FILE | --no-conf]
                        [--prune PATH]... [--prune-name NAME]... [--skip-snapshots]
                        [--group G] [--require-visibility BOOL] [--block-size N]
bcachefs-updatedb build --from-list FILE --output DB [--group G] ...
bcachefs-updatedb paths <mount> [--prefix P] [--conf FILE | --no-conf] [--prune PATH]...
                        [--prune-name NAME]... [--skip-snapshots] [--dump-dirs]
bcachefs-updatedb stats <mount>
bcachefs-updatedb dump <mount>
bcachefs-updatedb subvols <mount>
bcachefs-updatedb dbinfo <db> [--posting-lists]
```

- `--prefix` is the path users see the filesystem at; default is the mount argument.
  plocate checks visibility against that path, so it must match the real mount.
- Pruning comes from `/etc/updatedb.conf`, the same file `updatedb` reads and in its
  grammar: `PRUNEPATHS` are exact paths in prefixed form, `PRUNENAMES` are directory
  names matched anywhere. `PRUNEFS` and `PRUNE_BIND_MOUNTS` are parsed and ignored,
  since one filesystem is indexed. `--conf` reads another file, `--no-conf` none;
  `--prune` and `--prune-name` add to whatever the file says. Unlike `updatedb`, the
  pruned entry itself is not emitted, and `--prune` also accepts a file path.
- `--skip-snapshots` leaves out every subvolume carrying the snapshot flag, wherever it
  sits and whatever its name. `subvols` shows the flag as `snap`.
- `--group` names the group that owns the database. Without it, the group of the
  setgid `plocate` binary on PATH is used and printed; if there is none, the build
  fails rather than write a database only root can read.
- A previous database at `--output` supplies the zstd dictionary for this run, as with
  `updatedb`; the first run has none.
- `--from-list` indexes a stored path list instead of scanning, with the same dictionary
  handoff; it is what the differential test uses, and it needs no root.
- Emission order follows the btree, not the path; it can differ between two runs, so
  `plocate` result order is not stable across rebuilds. Content is.
- `stats`, `dump`, `subvols` and `paths --dump-dirs` are debugging views of the btrees
  and of the resolved directory map. `dbinfo` prints a database's header and, with
  `--posting-lists`, every posting list in hex.

## Install

Arch: `cd packaging && makepkg -si`. Debian: `make deb` (cross-compiles amd64 and arm64
in a container), then install the `.deb`.

Both ship `bcachefs-updatedb@.service` and `.timer`, instanced on the escaped mount
path, and `/etc/profile.d/bcachefs-updatedb.sh`. After installing:

1. In `/etc/updatedb.conf`, add `bcachefs` to `PRUNEFS`, or `updatedb` indexes the same
   files again (the mount directory itself still appears in both databases), and add
   the snapshots directory to `PRUNEPATHS` unless you want every snapshot indexed.
2. `systemctl enable --now bcachefs-updatedb@$(systemd-escape --path /mnt/fs).timer`
   per filesystem. The database lands in `/var/lib/plocate/bcachefs-<instance>.db`; a
   dash in the mount path becomes `\x2d` in the instance, and `/` is instance `-`.
3. Log in again. The profile.d script appends every existing
   `/var/lib/plocate/bcachefs-*.db` to `LOCATE_PATH`, which is the only way plocate
   searches more than its default database. Non-login contexts pass `-d` explicitly.
   A database removed after login breaks plocate until the next login.

The unit carries the same hardening as `plocate-updatedb.service` plus `CAP_SYS_ADMIN`.
Do not add MemoryHigh or MemoryMax to it: the btree nodes the scan reads are charged to
its cgroup but cannot be reclaimed from there, so a limit swaps or kills the tool while
the cache stays (see Limitations).

## Limitations

- Saves the scan, not the database: plocate stores literal paths, so N snapshots still
  emit N copies of every name.
- bcachefs only. Other filesystems keep `updatedb` and plocate merges the databases.
- Always the tree of subvolume 1, whatever subvolume is mounted at the given path.
- Not a stable ABI. Layouts of `bkey`, `bch_dirent`, `bch_subvolume` and
  `bch_snapshot`, the btree ids and key types are hard-coded from the bcachefs
  v1.39.5 headers. Only dirent decoding is checked at run time and aborts the run on
  failure; a layout shift elsewhere ends in "no root subvolume" or garbage. There is no
  version gate yet; the statfs check only rejects non-bcachefs mounts.
- Skipping non-live subvolumes is implemented but has never executed: every
  subvolume on the machines it was validated on is live.
- `/etc/updatedb.conf` is read as UTF-8; plocate's `updatedb` reads it as bytes. A
  prune path with non-UTF-8 bytes goes through `--prune` instead.
- Memory is the directory map (about 8 bytes per directory per snapshot it is visible
  in, plus the interned names) and the posting lists (their encoded size plus an eighth),
  all anonymous. A cgroup sees far more: the kernel charges every btree node the scan
  faults in to the unit's cgroup, and the btree cache shrinker is not memcg-aware, so
  that charge is never reclaimed under MemoryHigh or MemoryMax. Compare the process's
  VmHWM with the cgroup's slab_unreclaimable before reading "memory peak".

## License

GPL-2.0-only. The struct layouts are taken from GPL-2.0 kernel headers.
