//! Build a filename list from a mounted bcachefs by streaming the dirents btree
//! through BCH_IOCTL_QUERY_BTREE_KEYS, instead of walking it with readdir.

use bcachefs_updatedb::plocate_db::{
    self, DatabaseBuilder, Header, TRIGRAM_SIZE, Trigram, ZSTD_LEVEL,
};
use bcachefs_updatedb::updatedb_conf;
use clap::{Parser, Subcommand};
use hashbrown::HashTable;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Instant;
use zstd::dict::EncoderDictionary;

const BTREE_DIRENTS: u32 = 2;
const BTREE_SUBVOLUMES: u32 = 8;
const BTREE_SNAPSHOTS: u32 = 9;

const KEY_TYPE_DIRENT: u8 = 10;
const KEY_TYPE_SUBVOLUME: u8 = 21;
const KEY_TYPE_SNAPSHOT: u8 = 22;

const FLAG_ALL_SNAPSHOTS: u32 = 1 << 2;

/// sizeof(struct bkey): packed header, value follows.
const BKEY_HDR: usize = 40;
/// offsetof(struct bch_dirent, d_name) and .d_cf_name_block.{d_name_len,d_names}.
/// The casefold block's __le16s are 2-aligned, so d_pad occupies two bytes.
const DIRENT_NAME_OFF: usize = 9;
const DIRENT_CF_NAME_LEN_OFF: usize = 11;
const DIRENT_CF_NAMES_OFF: usize = 15;

/// BCACHEFS_STATFS_MAGIC, i.e. BCACHEFS_SUPER_MAGIC of linux/magic.h.
const BCACHEFS_STATFS_MAGIC: u64 = 0xca45_1a4e;

const DT_DIR: u8 = 4;
const DT_SUBVOL: u8 = 16;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Bpos {
    snapshot: u32,
    offset: u64,
    inode: u64,
}

const POS_MIN: Bpos = Bpos {
    snapshot: 0,
    offset: 0,
    inode: 0,
};
const POS_MAX: Bpos = Bpos {
    snapshot: u32::MAX,
    offset: u64::MAX,
    inode: u64::MAX,
};

#[repr(C)]
struct QueryBtreeKeys {
    btree: u32,
    level: u32,
    flags: u32,
    done: u32,
    start: Bpos,
    end: Bpos,
    buf: u64,
    buf_size: u32,
    used: u32,
}

fn ioctl_nr() -> libc::c_ulong {
    let size = std::mem::size_of::<QueryBtreeKeys>() as u64;
    (((3u64) << 30) | (size << 16) | (0xbcu64 << 8) | 34) as libc::c_ulong
}

/// Stream every key of `btree`, calling `f` with one `struct bkey_i` at a time.
fn for_each_key<F>(fd: i32, btree: u32, flags: u32, mut f: F) -> io::Result<u64>
where
    F: FnMut(&[u8]) -> io::Result<()>,
{
    let mut buf = vec![0u8; 4 << 20];
    let mut arg = QueryBtreeKeys {
        btree,
        level: 0,
        flags,
        done: 0,
        start: POS_MIN,
        end: POS_MAX,
        buf: buf.as_mut_ptr() as u64,
        buf_size: buf.len() as u32,
        used: 0,
    };

    let mut count = 0u64;
    loop {
        arg.used = 0;
        let ret = unsafe { libc::ioctl(fd, ioctl_nr(), &mut arg as *mut QueryBtreeKeys) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        let used = arg.used as usize;
        let mut off = 0usize;
        while off + BKEY_HDR <= used {
            let u64s = buf[off] as usize;
            if u64s < BKEY_HDR / 8 || off + u64s * 8 > used {
                return Err(io::Error::other(format!(
                    "malformed key at offset {off}: u64s={u64s} used={used}"
                )));
            }
            f(&buf[off..off + u64s * 8])?;
            count += 1;
            off += u64s * 8;
        }

        if arg.done != 0 {
            return Ok(count);
        }
        if used == 0 {
            return Err(io::Error::other("kernel returned no keys but not done"));
        }
    }
}

fn key_type(k: &[u8]) -> u8 {
    k[2]
}

fn key_snapshot(k: &[u8]) -> u32 {
    u32::from_le_bytes(
        k[20..24]
            .try_into()
            .unwrap(),
    )
}

fn key_offset(k: &[u8]) -> u64 {
    u64::from_le_bytes(
        k[24..32]
            .try_into()
            .unwrap(),
    )
}

fn key_inode(k: &[u8]) -> u64 {
    u64::from_le_bytes(
        k[32..40]
            .try_into()
            .unwrap(),
    )
}

/// A dirent we cannot decode means the struct layout has moved under us; that
/// silently drops names from the index, so it has to stop the run.
fn unparsable(k: &[u8]) -> io::Error {
    io::Error::other(format!(
        "unparsable dirent: u64s={} type_byte={:#04x}",
        k[0],
        k[BKEY_HDR + 8]
    ))
}

struct Dirent<'a> {
    target: u64,
    child_subvol: u32,
    parent_subvol: u32,
    d_type: u8,
    name: &'a [u8],
}

fn parse_dirent(k: &[u8]) -> Option<Dirent<'_>> {
    let val = &k[BKEY_HDR..];
    if val.len() < DIRENT_NAME_OFF + 1 {
        return None;
    }

    let type_byte = val[8];
    let casefold = type_byte & 0x80 != 0;

    let name = if casefold {
        let lo = DIRENT_CF_NAME_LEN_OFF;
        let len = u16::from_le_bytes(
            val.get(lo..lo + 2)?
                .try_into()
                .unwrap(),
        ) as usize;
        val.get(DIRENT_CF_NAMES_OFF..DIRENT_CF_NAMES_OFF + len)?
    } else {
        // The name is NUL-padded out to the last u64 of the value.
        let last = u64::from_le_bytes(
            val[val.len() - 8..]
                .try_into()
                .unwrap(),
        );
        let pad = if last == 0 {
            8
        } else {
            (last.leading_zeros() / 8) as usize
        };
        val.get(
            DIRENT_NAME_OFF
                ..val
                    .len()
                    .checked_sub(pad)?,
        )?
    };

    Some(Dirent {
        target: u64::from_le_bytes(
            val[0..8]
                .try_into()
                .unwrap(),
        ),
        child_subvol: u32::from_le_bytes(
            val[0..4]
                .try_into()
                .unwrap(),
        ),
        parent_subvol: u32::from_le_bytes(
            val[4..8]
                .try_into()
                .unwrap(),
        ),
        d_type: type_byte & 0x1f,
        name,
    })
}

/// SUBVOLUME_STATE_live; an unlinked or deleted subvolume keeps its dirent
/// until cleanup runs, but readdir no longer shows it.
const SUBVOLUME_STATE_LIVE: u32 = 0x4ad5_447e;

const BCH_SUBVOLUME_RO: u32 = 1 << 0;
const BCH_SUBVOLUME_SNAP: u32 = 1 << 1;

struct Subvol {
    snapshot: u32,
    root_inode: u64,
    state: u32,
    ro: bool,
    snap: bool,
}

impl Subvol {
    fn enterable(&self, skip_snapshots: bool) -> bool {
        self.state == SUBVOLUME_STATE_LIVE && !(skip_snapshots && self.snap)
    }
}

fn read_subvols(fd: i32) -> io::Result<HashMap<u32, Subvol>> {
    let mut out = HashMap::new();
    for_each_key(fd, BTREE_SUBVOLUMES, 0, |k| {
        if key_type(k) == KEY_TYPE_SUBVOLUME {
            let val = &k[BKEY_HDR..];
            let flags = u32::from_le_bytes(
                val[0..4]
                    .try_into()
                    .unwrap(),
            );
            out.insert(
                key_offset(k) as u32,
                Subvol {
                    snapshot: u32::from_le_bytes(
                        val[4..8]
                            .try_into()
                            .unwrap(),
                    ),
                    root_inode: u64::from_le_bytes(
                        val[8..16]
                            .try_into()
                            .unwrap(),
                    ),
                    state: u32::from_le_bytes(
                        val[40..44]
                            .try_into()
                            .unwrap(),
                    ),
                    ro: flags & BCH_SUBVOLUME_RO != 0,
                    snap: flags & BCH_SUBVOLUME_SNAP != 0,
                },
            );
        }
        Ok(())
    })?;
    Ok(out)
}

struct SnapNode {
    parent: u32,
}

fn read_snapshots(fd: i32) -> io::Result<HashMap<u32, SnapNode>> {
    let mut out = HashMap::new();
    for_each_key(fd, BTREE_SNAPSHOTS, 0, |k| {
        if key_type(k) == KEY_TYPE_SNAPSHOT {
            let val = &k[BKEY_HDR..];
            out.insert(
                key_offset(k) as u32,
                SnapNode {
                    parent: u32::from_le_bytes(
                        val[4..8]
                            .try_into()
                            .unwrap(),
                    ),
                },
            );
        }
        Ok(())
    })?;
    Ok(out)
}

/// Snapshot ids visible from `snap`, mapped to their distance from it. A key is
/// visible in a subvolume iff its snapshot is in this set; the smallest distance
/// wins when several versions of one entry are.
fn ancestry(snap: u32, snaps: &HashMap<u32, SnapNode>) -> HashMap<u32, u32> {
    let mut out = HashMap::new();
    let mut cur = snap;
    let mut dist = 0u32;
    loop {
        if out
            .insert(cur, dist)
            .is_some()
        {
            break; // cycle guard: malformed snapshot tree
        }
        match snaps.get(&cur) {
            Some(n) if n.parent != 0 => {
                cur = n.parent;
                dist += 1;
            }
            _ => break,
        }
    }
    out
}

/// Name id of a version that hides whatever directory sits at its position: a
/// whiteout, or a dirent pointing at something that is not a directory.
const NAME_BLOCKED: u32 = u32::MAX;
/// Parent of a root node.
const NODE_NONE: u32 = u32::MAX;

struct DirChild {
    hash: u64,
    target: u64,
    snapshot: u32,
    name: u32,
    child_subvol: u32,
    parent_subvol: u32,
    is_subvol: bool,
}

/// Distinct name byte strings, each stored once in one arena.
struct Names {
    arena: Vec<u8>,
    spans: Vec<(u32, u32)>,
    table: HashTable<u32>,
    hasher: RandomState,
}

fn span<'a>(arena: &'a [u8], spans: &[(u32, u32)], id: u32) -> &'a [u8] {
    let (off, len) = spans[id as usize];
    &arena[off as usize..off as usize + len as usize]
}

impl Names {
    fn new() -> Self {
        Names {
            arena: Vec::new(),
            spans: Vec::new(),
            table: HashTable::new(),
            hasher: RandomState::new(),
        }
    }

    fn get(&self, id: u32) -> &[u8] {
        span(&self.arena, &self.spans, id)
    }

    fn intern(&mut self, name: &[u8]) -> u32 {
        let Names {
            arena,
            spans,
            table,
            hasher,
        } = self;
        let h = hasher.hash_one(name);
        if let Some(&id) = table.find(h, |&id| span(arena, spans, id) == name) {
            return id;
        }
        let off = u32::try_from(arena.len()).expect("name arena grew past 4 GiB");
        let id = u32::try_from(spans.len()).expect("more than 4 G distinct names");
        arena.extend_from_slice(name);
        spans.push((off, name.len() as u32));
        table.insert_unique(h, id, |&other| hasher.hash_one(span(arena, spans, other)));
        id
    }
}

/// A resolved directory: where it hangs and what it is called. Nodes are appended
/// as they are discovered, so a parent id is always below its children's.
struct DirNode {
    parent: u32,
    name: u32,
}

/// Write the path of `node` into `out`, components joined by `/` and carrying the
/// bytes the filesystem holds. `stack` is scratch.
fn path_of(nodes: &[DirNode], names: &Names, node: u32, out: &mut Vec<u8>, stack: &mut Vec<u32>) {
    out.clear();
    stack.clear();
    let mut cur = node;
    while cur != NODE_NONE {
        stack.push(cur);
        cur = nodes[cur as usize].parent;
    }
    let mut first = true;
    while let Some(n) = stack.pop() {
        if !first {
            out.push(b'/');
        }
        first = false;
        out.extend_from_slice(names.get(nodes[n as usize].name));
    }
}

/// Pick, for each entry position, the version nearest the subvolume's snapshot.
fn choose_visible<'a, T, F>(run: &'a [T], vis: &HashMap<u32, u32>, snap_of: F) -> Option<&'a T>
where
    F: Fn(&T) -> u32,
{
    run.iter()
        .filter_map(|e| {
            vis.get(&snap_of(e))
                .map(|d| (*d, e))
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, e)| e)
}

struct RunKey {
    snapshot: u32,
    ktype: u8,
    bytes: Vec<u8>,
}

/// Group keys by position. Versions of one entry differ only in snapshot, so a
/// run holds every snapshot's take on the same name, whiteouts included.
fn for_each_run<F>(fd: i32, btree: u32, mut f: F) -> io::Result<u64>
where
    F: FnMut(u64, &[RunKey]) -> io::Result<()>,
{
    let mut run: Vec<RunKey> = Vec::new();
    let mut cur = (u64::MAX, u64::MAX);
    let mut runs = 0u64;

    let flush = |run: &mut Vec<RunKey>, inode: u64, f: &mut F| -> io::Result<()> {
        if !run.is_empty() {
            f(inode, run)?;
            run.clear();
        }
        Ok(())
    };

    for_each_key(fd, btree, FLAG_ALL_SNAPSHOTS, |k| {
        let pos = (key_inode(k), key_offset(k));
        if pos != cur {
            flush(&mut run, cur.0, &mut f)?;
            runs += 1;
            cur = pos;
        }
        run.push(RunKey {
            snapshot: key_snapshot(k),
            ktype: key_type(k),
            bytes: k.to_vec(),
        });
        Ok(())
    })?;
    flush(&mut run, cur.0, &mut f)?;
    Ok(runs)
}

const DEFAULT_UPDATEDB_CONF: &str = "/etc/updatedb.conf";

/// Exact paths to exclude, directory names to exclude wherever they occur, and
/// whether snapshot subvolumes are left out of the walk.
struct Filter {
    paths: Vec<Vec<u8>>,
    names: HashSet<Vec<u8>>,
    skip_snapshots: bool,
}

impl Filter {
    /// Match `dir/name` against the prune list without building the joined path,
    /// which would allocate once per emitted entry.
    fn is_pruned(&self, dir: &[u8], name: &[u8]) -> bool {
        self.paths
            .iter()
            .any(|b| {
                b.len() == dir.len() + 1 + name.len()
                    && b[..dir.len()] == *dir
                    && b[dir.len()] == b'/'
                    && b[dir.len() + 1..] == *name
            })
    }

    fn has_path(&self, path: &[u8]) -> bool {
        self.paths
            .iter()
            .any(|p| p == path)
    }

    fn is_pruned_name(&self, name: &[u8]) -> bool {
        self.names
            .contains(name)
    }
}

fn byte_args(args: Vec<OsString>) -> Vec<Vec<u8>> {
    args.into_iter()
        .map(OsString::into_vec)
        .collect()
}

fn load_filter(
    conf: Option<&Path>,
    no_conf: bool,
    mut paths: Vec<Vec<u8>>,
    mut names: Vec<Vec<u8>>,
    skip_snapshots: bool,
) -> io::Result<Filter> {
    if !no_conf {
        let path = conf.unwrap_or(Path::new(DEFAULT_UPDATEDB_CONF));
        if let Some(c) = updatedb_conf::load(path, conf.is_some())? {
            eprintln!(
                "{}: {} prune paths, {} prune names",
                path.display(),
                c.prunepaths
                    .len(),
                c.prunenames
                    .len()
            );
            paths.extend(
                c.prunepaths
                    .into_iter()
                    .map(String::into_bytes),
            );
            names.extend(
                c.prunenames
                    .into_iter()
                    .map(String::into_bytes),
            );
        }
    }
    Ok(Filter {
        paths,
        names: names
            .into_iter()
            .collect(),
        skip_snapshots,
    })
}

fn open_fs(path: &OsStr) -> io::Result<File> {
    File::open(path)
}

/// The btree ioctl answers ENOTTY on every other filesystem, which names
/// neither the path nor the reason.
fn check_bcachefs(fd: i32, path: &Path) -> io::Result<()> {
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(fd, st.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let f_type = unsafe {
        st.assume_init()
            .f_type
    } as u64;
    if f_type != BCACHEFS_STATFS_MAGIC {
        return Err(io::Error::other(format!(
            "{} is not a bcachefs mount: statfs f_type {f_type:#x}",
            path.display()
        )));
    }
    Ok(())
}

#[derive(Parser)]
#[command(name = "bcachefs-updatedb", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print every path reachable in the live namespace.
    Paths {
        /// Mount point of the bcachefs filesystem.
        mount: OsString,
        /// Path the emitted names are rooted at (default: the mount point).
        #[arg(long, value_name = "PATH")]
        prefix: Option<OsString>,
        /// Exact path to exclude, on top of PRUNEPATHS, repeatable.
        #[arg(long, value_name = "PATH")]
        prune: Vec<OsString>,
        /// Directory name to exclude anywhere, on top of PRUNENAMES, repeatable.
        #[arg(long, value_name = "NAME")]
        prune_name: Vec<OsString>,
        /// Read PRUNEPATHS and PRUNENAMES from FILE (default: /etc/updatedb.conf).
        #[arg(long, value_name = "FILE")]
        conf: Option<PathBuf>,
        /// Read no configuration file.
        #[arg(long, conflicts_with = "conf")]
        no_conf: bool,
        /// Do not enter subvolumes that are snapshots of another subvolume.
        #[arg(long)]
        skip_snapshots: bool,
        /// Print the resolved directory paths instead of the file list.
        #[arg(long)]
        dump_dirs: bool,
    },
    /// Write a plocate database from the live namespace.
    #[command(group = clap::ArgGroup::new("source").required(true).multiple(false).args(["mount", "from_list"]))]
    Build {
        /// Mount point of the bcachefs filesystem.
        mount: Option<OsString>,
        /// Index the newline-delimited paths of FILE instead of scanning a mount.
        #[arg(long, value_name = "FILE", conflicts_with_all = ["prefix", "prune", "prune_name", "conf", "no_conf", "skip_snapshots"])]
        from_list: Option<OsString>,
        /// Database to write, replaced atomically.
        #[arg(long, value_name = "DB")]
        output: PathBuf,
        /// Path the indexed names are rooted at (default: the mount point).
        #[arg(long, value_name = "PATH")]
        prefix: Option<OsString>,
        /// Exact path to exclude, on top of PRUNEPATHS, repeatable.
        #[arg(long, value_name = "PATH")]
        prune: Vec<OsString>,
        /// Directory name to exclude anywhere, on top of PRUNENAMES, repeatable.
        #[arg(long, value_name = "NAME")]
        prune_name: Vec<OsString>,
        /// Read PRUNEPATHS and PRUNENAMES from FILE (default: /etc/updatedb.conf).
        #[arg(long, value_name = "FILE")]
        conf: Option<PathBuf>,
        /// Read no configuration file.
        #[arg(long, conflicts_with = "conf")]
        no_conf: bool,
        /// Do not enter subvolumes that are snapshots of another subvolume.
        #[arg(long)]
        skip_snapshots: bool,
        /// Group owning the database (default: the group of the setgid plocate on PATH).
        #[arg(long, value_name = "NAME")]
        group: Option<String>,
        /// Have plocate check visibility before reporting a file.
        #[arg(long, value_name = "BOOL", default_value_t = true, action = clap::ArgAction::Set, value_parser = parse_bool)]
        require_visibility: bool,
        /// Number of filenames per compressed block.
        #[arg(long, value_name = "N", default_value_t = 32)]
        block_size: usize,
    },
    /// Print the header of a plocate database, and optionally its posting lists.
    Dbinfo {
        /// Database to read.
        db: PathBuf,
        /// Print one line per non-empty hash table slot instead of the header.
        #[arg(long)]
        posting_lists: bool,
    },
    /// Count dirent keys and name bytes by type.
    Stats {
        /// Mount point of the bcachefs filesystem.
        mount: OsString,
    },
    /// Print every dirent key, all snapshots included.
    Dump {
        /// Mount point of the bcachefs filesystem.
        mount: OsString,
    },
    /// List subvolumes with their snapshot, root inode and state.
    Subvols {
        /// Mount point of the bcachefs filesystem.
        mount: OsString,
    },
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "0" | "no" | "false" => Ok(false),
        "1" | "yes" | "true" => Ok(true),
        _ => Err(format!("expected one of 0, 1, no, yes, false, true: {s}")),
    }
}

fn open_mount(mount: &OsStr) -> io::Result<File> {
    let fs = open_fs(mount)?;
    check_bcachefs(fs.as_raw_fd(), Path::new(mount))?;
    Ok(fs)
}

/// The live namespace: every directory inode with the paths it is reachable
/// under, and the ancestry set of each subvolume context.
struct Namespace {
    subvols: HashMap<u32, Subvol>,
    ctx_subvol: HashMap<u32, u32>,
    names: Names,
    nodes: Vec<DirNode>,
    resolved: HashMap<u64, Vec<(u32, u32)>>,
    vis_cache: HashMap<u32, HashMap<u32, u32>>,
}

/// Contexts are subvolume snapshot ids, so the map always holds one.
fn subvol_of(ctx_subvol: &HashMap<u32, u32>, ctx: u32) -> u32 {
    ctx_subvol[&ctx]
}

/// The dedupe set of a context being walked, dropped once its last queued item pops.
#[derive(Default)]
struct CtxWalk {
    pending: usize,
    seen: HashSet<u64>,
}

fn resolve_namespace(fd: i32, prefix: &[u8], filter: &Filter) -> io::Result<Namespace> {
    let subvols = read_subvols(fd)?;
    let snaps = read_snapshots(fd)?;
    eprintln!(
        "{} subvolumes, {} snapshot tree nodes",
        subvols.len(),
        snaps.len()
    );

    let root = subvols
        .get(&1)
        .ok_or_else(|| io::Error::other("no root subvolume"))?;

    let scan = Instant::now();
    let mut names = Names::new();
    let mut dir_children: HashMap<u64, Vec<DirChild>> = HashMap::new();
    let mut versions: Vec<DirChild> = Vec::new();
    for_each_run(fd, BTREE_DIRENTS, |inode, run| {
        versions.clear();
        let mut any_dir = false;
        for r in run {
            // Every version of the position is kept: a nearer non-directory one
            // hides the directory an older snapshot has here.
            let mut c = DirChild {
                hash: key_offset(&r.bytes),
                snapshot: r.snapshot,
                name: NAME_BLOCKED,
                target: 0,
                child_subvol: 0,
                parent_subvol: 0,
                is_subvol: false,
            };
            if r.ktype == KEY_TYPE_DIRENT {
                let d = parse_dirent(&r.bytes).ok_or_else(|| unparsable(&r.bytes))?;
                if d.d_type == DT_DIR || d.d_type == DT_SUBVOL {
                    any_dir = true;
                    c.name = names.intern(d.name);
                    c.target = d.target;
                    c.child_subvol = d.child_subvol;
                    c.parent_subvol = d.parent_subvol;
                    c.is_subvol = d.d_type == DT_SUBVOL;
                }
            }
            versions.push(c);
        }
        if any_dir {
            dir_children
                .entry(inode)
                .or_default()
                .append(&mut versions);
        }
        Ok(())
    })?;
    eprintln!(
        "pass 1: {} directories with children in {:.2}s",
        dir_children.len(),
        scan.elapsed()
            .as_secs_f64()
    );

    // Snapshot subvolumes reuse inode numbers, so one inode can hold several
    // live nodes and each must be keyed by the subvolume context it came from.
    let ctx_subvol: HashMap<u32, u32> = subvols
        .iter()
        .map(|(id, sv)| (sv.snapshot, *id))
        .collect();
    let mut resolved: HashMap<u64, Vec<(u32, u32)>> = HashMap::new();
    let mut walk: HashMap<u32, CtxWalk> = HashMap::new();
    let mut vis_cache: HashMap<u32, HashMap<u32, u32>> = HashMap::new();
    let mut nodes = vec![DirNode {
        parent: NODE_NONE,
        name: names.intern(prefix),
    }];
    let mut queue = vec![(root.snapshot, root.root_inode, 0u32)];
    let mut dups = 0u64;
    let mut path: Vec<u8> = Vec::with_capacity(4096);
    let mut stack: Vec<u32> = Vec::new();
    resolved
        .entry(root.root_inode)
        .or_default()
        .push((root.snapshot, 0));
    let w = walk
        .entry(root.snapshot)
        .or_default();
    w.pending = 1;
    w.seen
        .insert(root.root_inode);

    while let Some((ctx, inode, node)) = queue.pop() {
        // Every reachable context needs an ancestry set, including one whose
        // directory holds only files: the emit pass looks it up per entry.
        let vis = vis_cache
            .entry(ctx)
            .or_insert_with(|| ancestry(ctx, &snaps));
        if let Some(children) = dir_children.get(&inode) {
            let subvol = subvol_of(&ctx_subvol, ctx);
            path_of(&nodes, &names, node, &mut path, &mut stack);
            let parent_len = path.len();
            for run in children.chunk_by(|a, b| a.hash == b.hash) {
                let Some(c) = choose_visible(run, vis, |c| c.snapshot) else {
                    continue;
                };
                if c.name == NAME_BLOCKED {
                    continue;
                }
                let name = names.get(c.name);
                if filter.is_pruned_name(name) {
                    continue;
                }
                path.truncate(parent_len);
                path.push(b'/');
                path.extend_from_slice(name);
                if filter.has_path(&path) {
                    continue;
                }
                let (nctx, ninode) = if c.is_subvol {
                    // A subvolume dirent belongs to one parent subvolume. Snapshots
                    // of that parent share the key but must not show the child.
                    if c.parent_subvol != subvol {
                        continue;
                    }
                    match subvols.get(&c.child_subvol) {
                        Some(sv) if sv.enterable(filter.skip_snapshots) => {
                            (sv.snapshot, sv.root_inode)
                        }
                        _ => continue,
                    }
                } else {
                    (ctx, c.target)
                };
                let w = walk
                    .entry(nctx)
                    .or_default();
                if !w
                    .seen
                    .insert(ninode)
                {
                    dups += 1;
                    continue;
                }
                w.pending += 1;
                let child = u32::try_from(nodes.len()).expect("more than 4 G resolved directories");
                nodes.push(DirNode {
                    parent: node,
                    name: c.name,
                });
                resolved
                    .entry(ninode)
                    .or_default()
                    .push((nctx, child));
                queue.push((nctx, ninode, child));
            }
        }
        let w = walk
            .get_mut(&ctx)
            .expect("a queued item holds its context entry");
        w.pending -= 1;
        if w.pending == 0 {
            walk.remove(&ctx);
        }
    }
    for v in resolved.values_mut() {
        v.shrink_to_fit();
    }
    eprintln!(
        "resolved {} directories in {} contexts, {dups} duplicate parents skipped",
        nodes.len(),
        vis_cache.len()
    );

    Ok(Namespace {
        subvols,
        ctx_subvol,
        names,
        nodes,
        resolved,
        vis_cache,
    })
}

/// Hand every entry whose parent directory landed in the live namespace to
/// `sink`, prefix line first, without a trailing newline.
fn emit_paths(
    fd: i32,
    ns: &Namespace,
    prefix: &[u8],
    filter: &Filter,
    sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<u64> {
    let emit = Instant::now();
    sink(prefix)?;

    let mut line: Vec<u8> = Vec::with_capacity(4096);
    let mut paths_buf: Vec<u8> = Vec::new();
    let mut ctx_paths: Vec<(u32, u32, usize, usize)> = Vec::new();
    let mut path_buf: Vec<u8> = Vec::new();
    let mut stack: Vec<u32> = Vec::new();
    let mut cur_inode: Option<u64> = None;
    let mut emitted = 0u64;
    for_each_run(fd, BTREE_DIRENTS, |inode, run| {
        if cur_inode != Some(inode) {
            cur_inode = Some(inode);
            ctx_paths.clear();
            paths_buf.clear();
            if let Some(contexts) = ns
                .resolved
                .get(&inode)
            {
                for &(ctx, node) in contexts {
                    path_of(&ns.nodes, &ns.names, node, &mut path_buf, &mut stack);
                    let start = paths_buf.len();
                    paths_buf.extend_from_slice(&path_buf);
                    ctx_paths.push((subvol_of(&ns.ctx_subvol, ctx), ctx, start, path_buf.len()));
                }
            }
        }
        for &(subvol, ctx, start, len) in &ctx_paths {
            let path = &paths_buf[start..start + len];
            let Some(vis) = ns
                .vis_cache
                .get(&ctx)
            else {
                continue;
            };
            let Some(r) = choose_visible(run, vis, |r| r.snapshot) else {
                continue;
            };
            if r.ktype != KEY_TYPE_DIRENT {
                continue;
            }
            let d = parse_dirent(&r.bytes).ok_or_else(|| unparsable(&r.bytes))?;
            if filter.is_pruned(path, d.name) {
                continue;
            }
            if (d.d_type == DT_DIR || d.d_type == DT_SUBVOL) && filter.is_pruned_name(d.name) {
                continue;
            }
            if d.d_type == DT_SUBVOL
                && (d.parent_subvol != subvol
                    || !ns
                        .subvols
                        .get(&d.child_subvol)
                        .is_some_and(|sv| sv.enterable(filter.skip_snapshots)))
            {
                continue;
            }
            line.clear();
            line.extend_from_slice(path);
            line.push(b'/');
            line.extend_from_slice(d.name);
            sink(&line)?;
            emitted += 1;
        }
        Ok(())
    })?;
    eprintln!(
        "emitted {emitted} paths in {:.2}s",
        emit.elapsed()
            .as_secs_f64()
    );
    Ok(emitted)
}

fn cmd_paths(fd: i32, prefix: &[u8], filter: &Filter, dump_dirs: bool) -> io::Result<()> {
    let ns = resolve_namespace(fd, prefix, filter)?;
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(4 << 20, stdout.lock());

    if dump_dirs {
        let mut path = Vec::new();
        let mut stack = Vec::new();
        for (inode, ctxs) in &ns.resolved {
            for &(ctx, node) in ctxs {
                path_of(&ns.nodes, &ns.names, node, &mut path, &mut stack);
                let subvol = subvol_of(&ns.ctx_subvol, ctx);
                write!(out, "{inode}\t{subvol}\t{ctx}\t")?;
                out.write_all(&path)?;
                out.write_all(b"\n")?;
            }
        }
        return out.flush();
    }

    emit_paths(fd, &ns, prefix, filter, &mut |line| {
        out.write_all(line)?;
        out.write_all(b"\n")
    })?;
    out.flush()
}

/// Hand every line of `list` to `sink`, newline stripped, in file order. The
/// list already carries the prefix line, so none is added.
fn feed_list(list: &Path, sink: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<u64> {
    let read = Instant::now();
    let file = File::open(list)?;
    let mut count = 0u64;
    for line in BufReader::with_capacity(4 << 20, file).split(b'\n') {
        sink(&line?)?;
        count += 1;
    }
    if count == 0 {
        return Err(io::Error::other(format!("{} is empty", list.display())));
    }
    eprintln!(
        "read {count} paths from {} in {:.2}s",
        list.display(),
        read.elapsed()
            .as_secs_f64()
    );
    Ok(count)
}

enum Source<'a> {
    Scan {
        fd: i32,
        prefix: &'a [u8],
        filter: &'a Filter,
    },
    List(&'a Path),
}

fn cmd_build(
    source: Source<'_>,
    output: &Path,
    group: Option<&str>,
    check_visibility: bool,
    block_size: usize,
) -> io::Result<()> {
    let gid = plocate_db::resolve_group(group)?;
    let dictionary = plocate_db::read_next_dictionary(output)?;
    if dictionary.is_empty() {
        eprintln!("no dictionary to reuse; this run compresses filenames without one");
    } else {
        eprintln!(
            "reusing the {}-byte next dictionary of {}",
            dictionary.len(),
            output.display()
        );
    }
    let cdict = if dictionary.is_empty() {
        None
    } else {
        Some(EncoderDictionary::copy(&dictionary, ZSTD_LEVEL))
    };

    let mut db = DatabaseBuilder::new(
        output,
        Some(gid),
        block_size,
        &dictionary,
        cdict.as_ref(),
        check_visibility,
    )?;
    match source {
        Source::Scan { fd, prefix, filter } => {
            let ns = resolve_namespace(fd, prefix, filter)?;
            emit_paths(fd, &ns, prefix, filter, &mut |line| db.add_file(line))?
        }
        Source::List(list) => feed_list(list, &mut |line| db.add_file(line))?,
    };
    let stats = db.finish()?;

    let mb = |bytes: u64| bytes as f64 / 1048576.0;
    eprintln!(
        "{} files, {} different trigrams, {} entries, longest {}",
        stats.num_files, stats.num_trigrams, stats.num_entries, stats.longest_posting_list
    );
    eprintln!("Block size:     {block_size:7} files");
    eprintln!("Dictionary:     {:7.1} MB", mb(stats.bytes_for_dictionary));
    eprintln!("Hash table:     {:7.1} MB", mb(stats.bytes_for_hashtable));
    eprintln!(
        "Posting lists:  {:7.1} MB",
        mb(stats.bytes_for_posting_lists)
    );
    eprintln!(
        "Filename index: {:7.1} MB",
        mb(stats.bytes_for_filename_index)
    );
    eprintln!("Filenames:      {:7.1} MB", mb(stats.bytes_for_filenames));
    eprintln!(
        "Total:          {:7.1} MB",
        mb(stats.bytes_for_dictionary
            + stats.bytes_for_hashtable
            + stats.bytes_for_posting_lists
            + stats.bytes_for_filename_index
            + stats.bytes_for_filenames)
    );
    Ok(())
}

fn cmd_dbinfo(db: &Path, posting_lists: bool) -> io::Result<()> {
    let hdr = Header::read(db)?;
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());

    if !posting_lists {
        writeln!(out, "magic {}", String::from_utf8_lossy(&hdr.magic[1..]))?;
        writeln!(out, "version {}", hdr.version)?;
        writeln!(out, "hashtable_size {}", hdr.hashtable_size)?;
        writeln!(out, "extra_ht_slots {}", hdr.extra_ht_slots)?;
        writeln!(out, "num_docids {}", hdr.num_docids)?;
        writeln!(
            out,
            "hash_table_offset_bytes {}",
            hdr.hash_table_offset_bytes
        )?;
        writeln!(
            out,
            "filename_index_offset_bytes {}",
            hdr.filename_index_offset_bytes
        )?;
        writeln!(out, "max_version {}", hdr.max_version)?;
        writeln!(
            out,
            "zstd_dictionary_length_bytes {}",
            hdr.zstd_dictionary_length_bytes
        )?;
        writeln!(
            out,
            "zstd_dictionary_offset_bytes {}",
            hdr.zstd_dictionary_offset_bytes
        )?;
        writeln!(
            out,
            "directory_data_length_bytes {}",
            hdr.directory_data_length_bytes
        )?;
        writeln!(
            out,
            "directory_data_offset_bytes {}",
            hdr.directory_data_offset_bytes
        )?;
        writeln!(
            out,
            "next_zstd_dictionary_length_bytes {}",
            hdr.next_zstd_dictionary_length_bytes
        )?;
        writeln!(
            out,
            "next_zstd_dictionary_offset_bytes {}",
            hdr.next_zstd_dictionary_offset_bytes
        )?;
        writeln!(
            out,
            "conf_block_length_bytes {}",
            hdr.conf_block_length_bytes
        )?;
        writeln!(
            out,
            "conf_block_offset_bytes {}",
            hdr.conf_block_offset_bytes
        )?;
        writeln!(out, "check_visibility {}", hdr.check_visibility)?;
        return out.flush();
    }

    let file = File::open(db)?;
    let slots = (hdr.hashtable_size as usize)
        .checked_add(hdr.extra_ht_slots as usize)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| io::Error::other("hash table size overflows"))?;
    let mut table = vec![0u8; slots * TRIGRAM_SIZE];
    file.read_exact_at(&mut table, hdr.hash_table_offset_bytes)?;

    let mut entries: Vec<(u32, u32, u64, u64)> = Vec::new();
    for i in 0..slots - 1 {
        let slot = Trigram::from_bytes(&table[i * TRIGRAM_SIZE..]);
        if slot.num_docids == 0 {
            continue;
        }
        let next = Trigram::from_bytes(&table[(i + 1) * TRIGRAM_SIZE..]);
        entries.push((
            slot.trgm,
            slot.num_docids,
            slot.offset,
            next.offset - slot.offset,
        ));
    }
    entries.sort_unstable();

    let mut encoded = Vec::new();
    for (trgm, num_docids, offset, len) in entries {
        encoded.resize(len as usize, 0);
        file.read_exact_at(&mut encoded, offset)?;
        write!(out, "trgm={trgm:06x} n={num_docids} ")?;
        for byte in &encoded {
            write!(out, "{byte:02x}")?;
        }
        out.write_all(b"\n")?;
    }
    out.flush()
}

fn main() -> io::Result<()> {
    // SAFETY: single-threaded here, so no other thread can observe the disposition change.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Subvols { mount } => {
            let fs = open_mount(&mount)?;
            let fd = fs.as_raw_fd();
            let subvols = read_subvols(fd)?;
            let mut ids: Vec<_> = subvols
                .keys()
                .copied()
                .collect();
            ids.sort_unstable();
            for id in ids {
                let s = &subvols[&id];
                println!(
                    "subvol {id} snapshot {} root_inode {} state {:#010x}{}{}{}",
                    s.snapshot,
                    s.root_inode,
                    s.state,
                    if s.state == SUBVOLUME_STATE_LIVE {
                        " live"
                    } else {
                        " NOT-LIVE"
                    },
                    if s.ro { " ro" } else { "" },
                    if s.snap { " snap" } else { "" }
                );
            }
            eprintln!("{} subvolumes", subvols.len());
        }
        Cmd::Stats { mount } => {
            let fs = open_mount(&mount)?;
            let fd = fs.as_raw_fd();
            let start = Instant::now();
            let mut by_type = [0u64; 32];
            let mut dirents = 0u64;
            let mut name_bytes = 0u64;
            let mut casefold = 0u64;
            let mut unparsed = 0u64;
            let total = for_each_key(fd, BTREE_DIRENTS, FLAG_ALL_SNAPSHOTS, |k| {
                if key_type(k) == KEY_TYPE_DIRENT {
                    dirents += 1;
                    if k[BKEY_HDR + 8] & 0x80 != 0 {
                        casefold += 1;
                    }
                    match parse_dirent(k) {
                        Some(d) => {
                            by_type[(d.d_type & 31) as usize] += 1;
                            name_bytes += d
                                .name
                                .len() as u64;
                        }
                        None => {
                            if unparsed < 4 {
                                let val = &k[BKEY_HDR..];
                                eprintln!(
                                    "unparsed: u64s={} val_len={} type_byte={:#04x} raw={:02x?}",
                                    k[0],
                                    val.len(),
                                    val[8],
                                    &val[..val
                                        .len()
                                        .min(24)]
                                );
                            }
                            unparsed += 1;
                        }
                    }
                }
                Ok(())
            })?;
            eprintln!("casefold {casefold} unparsed {unparsed}");
            let el = start.elapsed();
            eprintln!(
                "keys {total} dirents {dirents} in {:.2}s = {:.0} dirents/s",
                el.as_secs_f64(),
                dirents as f64 / el.as_secs_f64()
            );
            eprintln!("name bytes {name_bytes}");
            for (t, n) in by_type
                .iter()
                .enumerate()
            {
                if *n > 0 {
                    let label = match t as u8 {
                        DT_DIR => "dir",
                        DT_SUBVOL => "subvol",
                        8 => "reg",
                        10 => "lnk",
                        _ => "other",
                    };
                    eprintln!("  d_type {t:2} {label:7} {n}");
                }
            }
        }
        Cmd::Dump { mount } => {
            let fs = open_mount(&mount)?;
            let fd = fs.as_raw_fd();
            let stdout = io::stdout();
            let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
            for_each_key(fd, BTREE_DIRENTS, FLAG_ALL_SNAPSHOTS, |k| {
                if key_type(k) != KEY_TYPE_DIRENT {
                    return Ok(());
                }
                let d = parse_dirent(k).ok_or_else(|| unparsable(k))?;
                write!(out, "{}:{}:", key_inode(k), key_snapshot(k))?;
                out.write_all(d.name)?;
                if d.d_type == DT_SUBVOL {
                    write!(out, " -> subvol {}", d.child_subvol)?;
                } else {
                    write!(out, " -> {} type {}", d.target, d.d_type)?;
                }
                out.write_all(b"\n")
            })?;
            out.flush()?;
        }
        Cmd::Paths {
            mount,
            prefix,
            prune,
            prune_name,
            conf,
            no_conf,
            skip_snapshots,
            dump_dirs,
        } => {
            let filter = load_filter(
                conf.as_deref(),
                no_conf,
                byte_args(prune),
                byte_args(prune_name),
                skip_snapshots,
            )?;
            let fs = open_mount(&mount)?;
            let prefix = prefix
                .unwrap_or(mount)
                .into_vec();
            cmd_paths(fs.as_raw_fd(), &prefix, &filter, dump_dirs)?;
        }
        Cmd::Build {
            mount,
            from_list,
            output,
            prefix,
            prune,
            prune_name,
            conf,
            no_conf,
            skip_snapshots,
            group,
            require_visibility,
            block_size,
        } => match (mount, from_list) {
            (Some(mount), None) => {
                let filter = load_filter(
                    conf.as_deref(),
                    no_conf,
                    byte_args(prune),
                    byte_args(prune_name),
                    skip_snapshots,
                )?;
                let fs = open_mount(&mount)?;
                let prefix = prefix
                    .unwrap_or(mount)
                    .into_vec();
                cmd_build(
                    Source::Scan {
                        fd: fs.as_raw_fd(),
                        prefix: &prefix,
                        filter: &filter,
                    },
                    &output,
                    group.as_deref(),
                    require_visibility,
                    block_size,
                )?;
            }
            (None, Some(list)) => {
                cmd_build(
                    Source::List(Path::new(&list)),
                    &output,
                    group.as_deref(),
                    require_visibility,
                    block_size,
                )?;
            }
            _ => {
                return Err(io::Error::other(
                    "exactly one of <MOUNT> and --from-list is required",
                ));
            }
        },
        Cmd::Dbinfo { db, posting_lists } => {
            cmd_dbinfo(&db, posting_lists)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(prefix: &[u8], comps: &[&[u8]]) -> Vec<u8> {
        let mut names = Names::new();
        let mut nodes = vec![DirNode {
            parent: NODE_NONE,
            name: names.intern(prefix),
        }];
        for c in comps {
            let name = names.intern(c);
            let parent = nodes.len() as u32 - 1;
            nodes.push(DirNode { parent, name });
        }
        let mut out = Vec::new();
        let mut stack = Vec::new();
        path_of(&nodes, &names, nodes.len() as u32 - 1, &mut out, &mut stack);
        out
    }

    #[test]
    fn interner_stores_each_name_once_and_keeps_raw_bytes() {
        let mut names = Names::new();
        let a = names.intern(b"etc");
        let b = names.intern(b"etc");
        let raw = names.intern(b"\xff\xfe/");
        assert_eq!(a, b);
        assert_ne!(a, raw);
        assert_eq!(names.intern(b"\xff\xfe/"), raw);
        assert_eq!(names.get(a), b"etc");
        assert_eq!(names.get(raw), b"\xff\xfe/");
        assert_eq!(
            names
                .spans
                .len(),
            2
        );
    }

    #[test]
    fn path_of_joins_components_as_raw_bytes() {
        assert_eq!(path(b"/mnt", &[&b"x"[..]]).as_slice(), b"/mnt/x".as_slice());
        assert_eq!(
            path(b"/mnt/", &[&b"x"[..]]).as_slice(),
            b"/mnt//x".as_slice()
        );
        assert_eq!(path(b"", &[&b"x"[..]]).as_slice(), b"/x".as_slice());
        assert_eq!(
            path(b"/mnt", &[&b"a"[..], &b"b"[..]]).as_slice(),
            b"/mnt/a/b".as_slice()
        );
        assert_eq!(
            path(b"/mnt", &[&b"\xff"[..], &b"c"[..]]).as_slice(),
            b"/mnt/\xff/c".as_slice()
        );
    }
}
