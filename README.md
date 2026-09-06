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

Two passes over the dirents btree, all snapshots included. Pass 1 keeps directory and
subvolume entries only and resolves the path of every reachable directory. Pass 2
streams everything and emits `parent_path/name` for each entry whose parent resolved.

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

Measured with this binary, `/usr/bin/time -v`, output to `/dev/null` for `paths`, no
configuration file (`--no-conf`). Both machines run a Ryzen 9 7950X3D. "Live tree"
prunes the snapshots directory; "all" indexes every snapshot.

**p4**: 64 GB RAM, kernel 7.1.9, bcachefs DKMS 1.39.2, 13.6 TB filesystem (8.8 TB used) on
four HDDs and two NVMe, 208 subvolumes, 22 428 snapshots, 15.2 M dirent keys (34.2 M
keys with whiteouts).

| run | paths | wall | peak RSS | output |
|---|---|---|---|---|
| `stats` (one pass over every key, cold) | 15.2 M keys | 5.7 s | 5 MB | 2.7 M keys/s |
| `paths`, live tree | 11.8 M | 8.1 s | 0.76 GB | |
| `paths`, all snapshots | 214.7 M | 45.8 s | 7.0 GB | |
| `build`, live tree, no dictionary | 11.8 M | 18.2 s | 0.90 GB | 319 MB db |
| `build`, live tree, dictionary from previous db | 11.8 M | 29.5 s | 0.90 GB | 303 MB db |
| `build`, all snapshots | 214.7 M | 172 s | 7.0 GB | 2.95 GB db |

The readdir-based `updatedb` job that used to index the snapshots portion of this
filesystem took 1 h 47 min and 6.98 GB, and produced a 4.07 GB database. Path counts are
not comparable: `updatedb` applied PRUNENAMES, these runs pruned one path. The claim is
the wall time.

**castgti86**: 32 GB RAM, kernel 7.2.2, DKMS 1.39.4, 734 GB filesystem (308 GB used) on
LVM-on-LUKS NVMe plus one HDD, 521 subvolumes, 39 364 snapshots, 7.6 M dirent keys
(12.3 M with whiteouts).

| run | paths | wall | peak RSS | output |
|---|---|---|---|---|
| `stats` | 7.6 M keys | 1.8 s | 5 MB | 4.3 M keys/s |
| `paths`, live tree | 13.3 M | 4.1 s | 0.58 GB | |
| `paths`, all snapshots | 56.2 M | 13.9 s | 1.65 GB | |
| `build`, live tree, no dictionary | 13.3 M | 18.2 s | 0.73 GB | 304 MB db |
| `build`, live tree, dictionary from previous db | 13.3 M | 15.5 s | 0.73 GB | 281 MB db |
| `build`, all snapshots | 56.2 M | 41.7 s | 1.77 GB | 723 MB db |

Scanning is independent of the snapshot count: keys shared between snapshots are stored
once and read once. What grows with snapshots is the number of paths emitted and the
resolved-directory map, which is where the memory goes.

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
                        [--prune PATH]... [--prune-name NAME]... [--group G]
                        [--require-visibility BOOL] [--block-size N]
bcachefs-updatedb build --from-list FILE --output DB [--group G] ...
bcachefs-updatedb paths <mount> [--prefix P] [--conf FILE | --no-conf] [--prune PATH]...
                        [--prune-name NAME]... [--dump-dirs]
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

## Limitations

- Saves the scan, not the database: plocate stores literal paths, so N snapshots still
  emit N copies of every name.
- bcachefs only. Other filesystems keep `updatedb` and plocate merges the databases.
- Always the tree of subvolume 1, whatever subvolume is mounted at the given path.
- Not a stable ABI. Layouts of `bkey`, `bch_dirent`, `bch_subvolume` and
  `bch_snapshot`, the btree ids and key types are hard-coded from the bcachefs
  v1.39.4 headers. Only dirent decoding is checked at run time and aborts the run on
  failure; a layout shift elsewhere ends in "no root subvolume" or garbage. There is no
  version gate yet; the statfs check only rejects non-bcachefs mounts.
- Skipping non-live subvolumes is implemented but has never executed: every
  subvolume on the machines it was validated on is live.
- Peak memory on a full-snapshot run is several GB of anonymous working set (the
  resolved directory map plus posting lists), not reclaimable cache.

## License

GPL-2.0-only. The struct layouts are taken from GPL-2.0 kernel headers.
