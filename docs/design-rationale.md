# Design rationale

## Dirent btree over filesystem traversal

The filesystem already maintains a sorted index of names keyed by parent directory; a tree walk rediscovers it one syscall at a time and pays for every path it materializes. Snapshots decouple stored keys from reachable paths without bound, because keys shared between snapshots are stored once and walked once per snapshot. Build cost tracks stored keys, and adding snapshots must not change it.

## Kernel ioctl over device and debug interfaces

Keys come from the btree-query ioctl issued against the mount point. Nothing reads the block device: a mounted filesystem gives a torn view, and an encrypted one would demand a key the kernel already holds. The debug filesystem exposes the same keys, but as text, with no interface promise and a parse of everything it prints. A path that opens the device is out of scope rather than merely unused.

## Snapshot visibility

Bypassing readdir bypasses the kernel's snapshot filtering, so this tool reimplements it, and that logic carries the tool's correctness. An entry is visible in a subvolume when its snapshot is an ancestor of that subvolume's; the nearest ancestor supersedes the rest; a whiteout at a visible snapshot removes the entry; and a subvolume dirent belongs to the subvolume that created it, so snapshots of its parent do not show it. Resolved paths are keyed by subvolume context and never by inode alone: snapshot subvolumes reuse inode numbers, and an inode-keyed map drops whole subtrees without any sign that it did.

## Directory map holds structure, not paths

Resolved directories form a tree of (parent node, interned name); a directory's path is materialized once per directory run at emission and never stored. Entries scale with directories times snapshot contexts, and a stored path multiplies that by its length. Two parents for one (context, inode) can only come from a rename landing between two ioctl batches; the second is dropped by a set that lives only while that context is being walked, never by a set over the whole run, and each subvolume is entered once per run. Names are bytes end to end: the interner, the prune rules and the emitted path carry the bytes the filesystem holds, and lossy UTF-8 appears only in diagnostics, because a path rendered any other way names nothing on disk.

## Undecodable keys abort the run

A key that cannot be decoded stops the run with an error instead of being skipped. The output's only claim is completeness; nothing downstream can tell an absent name from one never offered, and a layout change upstream presents exactly as names quietly going missing.

## Path list stays the verification interface

The newline-delimited path list is the only output that can be diffed against a tree walk, and that diff is the only check on the visibility rules above. The index writer consumes the same stream the list is printed from, so a clean differential on the list vouches for the index. Any change to what gets emitted goes through the list first.

## Own index writer

The tool writes the search database itself rather than handing a list to the upstream builder. The upstream builder reads its input twice and only from a seekable file, so the daily job would have to materialize the whole list on disk, several times the size of the index it produces. Writing the index directly keeps the run at one pass over the keys and leaves the dictionary handoff between runs, which the upstream indexer also keeps, inside the indexer rather than in a wrapper. Format compatibility is proven by comparing the index against the upstream builder's, never by inspection.

## Version gating against the running module

On-disk structure layouts carry no compatibility promise. The tool must refuse to run against a filesystem version it was not built for: a moved field misparses into plausible names rather than an error, and an index of plausible names is worse than no index. Until a gate exists, the hard-coded layouts and the header version they were read from are listed where users see them.

## Database group follows the search binary

The database is readable only through the search binary's setgid group, and distributions name that group differently. The group is taken from the installed search binary unless told otherwise, so one unit file serves every distribution and a build that finds no such binary fails instead of writing a database nobody can read.

## Pruning follows the system indexer's configuration

Prune rules are read from the system indexer's configuration file, in its grammar, so one file governs both indexers and a second dialect never has to track it. Only the path and directory-name rules apply: filesystem-type and bind-mount rules select mounts, and this tool indexes exactly one. Command-line prunes extend the file's rules and never replace them. Snapshot subvolumes are left out by the subvolume's own snapshot flag, never by a name pattern: snapshots are routinely named as siblings of the tree they copy. A pruned entry is dropped along with everything under it; the system indexer keeps the entry itself, and that difference is accepted rather than mirrored, so a pruned path never surfaces as a hit.

## One filesystem per index

Scope is this filesystem alone. Others keep their existing indexer and the search tool merges the resulting databases. Widening this into a general indexer would mean reimplementing the traversal it exists to avoid.
