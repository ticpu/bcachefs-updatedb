//! Build a filename list from a mounted bcachefs by streaming the dirents btree
//! through BCH_IOCTL_QUERY_BTREE_KEYS, instead of walking it with readdir.

use clap::{Parser, Subcommand};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::unix::io::AsRawFd;
use std::time::Instant;

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

struct Subvol {
    snapshot: u32,
    root_inode: u64,
    state: u32,
}

fn read_subvols(fd: i32) -> io::Result<HashMap<u32, Subvol>> {
    let mut out = HashMap::new();
    for_each_key(fd, BTREE_SUBVOLUMES, 0, |k| {
        if key_type(k) == KEY_TYPE_SUBVOLUME {
            let val = &k[BKEY_HDR..];
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

struct DirChild {
    snapshot: u32,
    hash: u64,
    name: Vec<u8>,
    target: u64,
    child_subvol: u32,
    parent_subvol: u32,
    is_subvol: bool,
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

/// Match `dir/name` against the prune list without building the joined path,
/// which would allocate once per emitted entry.
fn is_pruned(prunes: &[String], dir: &str, name: &[u8]) -> bool {
    prunes
        .iter()
        .any(|p| {
            let b = p.as_bytes();
            b.len() == dir.len() + 1 + name.len()
                && b[..dir.len()] == *dir.as_bytes()
                && b[dir.len()] == b'/'
                && b[dir.len() + 1..] == *name
        })
}

fn open_fs(path: &str) -> io::Result<File> {
    File::open(path)
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
        mount: String,
        /// Path the emitted names are rooted at (default: the mount point).
        #[arg(long, value_name = "PATH")]
        prefix: Option<String>,
        /// Exact path to exclude, repeatable.
        #[arg(long, value_name = "PATH")]
        prune: Vec<String>,
        /// Print the resolved directory paths instead of the file list.
        #[arg(long)]
        dump_dirs: bool,
    },
    /// Count dirent keys and name bytes by type.
    Stats {
        /// Mount point of the bcachefs filesystem.
        mount: String,
    },
    /// Print every dirent key, all snapshots included.
    Dump {
        /// Mount point of the bcachefs filesystem.
        mount: String,
    },
    /// List subvolumes with their snapshot, root inode and state.
    Subvols {
        /// Mount point of the bcachefs filesystem.
        mount: String,
    },
}

impl Cmd {
    fn mount(&self) -> &str {
        match self {
            Cmd::Paths { mount, .. }
            | Cmd::Stats { mount }
            | Cmd::Dump { mount }
            | Cmd::Subvols { mount } => mount,
        }
    }
}

fn cmd_paths(fd: i32, prefix: &str, prunes: &[String], dump_dirs: bool) -> io::Result<()> {
    let subvols = read_subvols(fd)?;
    let snaps = read_snapshots(fd)?;
    eprintln!("{} subvolumes, {} snapshots", subvols.len(), snaps.len());

    let root = subvols
        .get(&1)
        .ok_or_else(|| io::Error::other("no root subvolume"))?;

    let scan = Instant::now();
    let mut dir_children: HashMap<u64, Vec<DirChild>> = HashMap::new();
    for_each_run(fd, BTREE_DIRENTS, |inode, run| {
        let interesting = run
            .iter()
            .any(|r| {
                r.ktype == KEY_TYPE_DIRENT
                    && parse_dirent(&r.bytes)
                        .is_some_and(|d| d.d_type == DT_DIR || d.d_type == DT_SUBVOL)
            });
        if !interesting {
            return Ok(());
        }
        let slot = dir_children
            .entry(inode)
            .or_default();
        for r in run {
            let hash = key_offset(&r.bytes);
            match r.ktype {
                KEY_TYPE_DIRENT => {
                    let d = parse_dirent(&r.bytes).ok_or_else(|| unparsable(&r.bytes))?;
                    if d.d_type == DT_DIR || d.d_type == DT_SUBVOL {
                        slot.push(DirChild {
                            snapshot: r.snapshot,
                            hash,
                            name: d
                                .name
                                .to_vec(),
                            target: d.target,
                            child_subvol: d.child_subvol,
                            parent_subvol: d.parent_subvol,
                            is_subvol: d.d_type == DT_SUBVOL,
                        });
                    }
                }
                // A whiteout hides the entry in this snapshot and below.
                _ => slot.push(DirChild {
                    snapshot: r.snapshot,
                    hash,
                    name: Vec::new(),
                    target: 0,
                    child_subvol: 0,
                    parent_subvol: 0,
                    is_subvol: false,
                }),
            }
        }
        Ok(())
    })?;
    eprintln!(
        "pass 1: {} directories with children in {:.2}s",
        dir_children.len(),
        scan.elapsed()
            .as_secs_f64()
    );

    // Walk the live namespace, recording the path of every reachable directory.
    // Snapshot subvolumes reuse inode numbers, so one inode can hold several
    // live paths and each must be keyed by the subvolume context it came from.
    let mut resolved: HashMap<u64, Vec<(u32, u32, String)>> = HashMap::new();
    let mut seen: HashSet<(u32, u64)> = HashSet::new();
    let mut vis_cache: HashMap<u32, HashMap<u32, u32>> = HashMap::new();
    let mut queue = vec![(1u32, root.snapshot, root.root_inode, prefix.to_string())];
    resolved
        .entry(root.root_inode)
        .or_default()
        .push((1, root.snapshot, prefix.to_string()));
    seen.insert((root.snapshot, root.root_inode));

    while let Some((subvol, ctx, inode, path)) = queue.pop() {
        // Every reachable context needs an ancestry set, including one whose
        // directory holds only files: the emit pass looks it up per entry.
        let vis = vis_cache
            .entry(ctx)
            .or_insert_with(|| ancestry(ctx, &snaps));
        let Some(children) = dir_children.get(&inode) else {
            continue;
        };

        let mut by_hash: HashMap<u64, Vec<&DirChild>> = HashMap::new();
        for c in children {
            by_hash
                .entry(c.hash)
                .or_default()
                .push(c);
        }
        for (_, run) in by_hash {
            let Some(c) = choose_visible(&run, vis, |c| c.snapshot) else {
                continue;
            };
            if c.name
                .is_empty()
            {
                continue; // whiteout won
            }
            let mut child_path = path.clone();
            child_path.push('/');
            child_path.push_str(&String::from_utf8_lossy(&c.name));
            if prunes.contains(&child_path) {
                continue;
            }
            let (nsubvol, nctx, ninode) = if c.is_subvol {
                // A subvolume dirent belongs to one parent subvolume. Snapshots
                // of that parent share the key but must not show the child.
                if c.parent_subvol != subvol {
                    continue;
                }
                match subvols.get(&c.child_subvol) {
                    Some(sv) if sv.state == SUBVOLUME_STATE_LIVE => {
                        (c.child_subvol, sv.snapshot, sv.root_inode)
                    }
                    _ => continue,
                }
            } else {
                (subvol, ctx, c.target)
            };
            if seen.insert((nctx, ninode)) {
                resolved
                    .entry(ninode)
                    .or_default()
                    .push((nsubvol, nctx, child_path.clone()));
                queue.push((nsubvol, nctx, ninode, child_path));
            }
        }
    }
    eprintln!("resolved {} directory paths", resolved.len());

    if dump_dirs {
        let stdout = io::stdout();
        let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
        for (inode, ctxs) in &resolved {
            for (subvol, ctx, path) in ctxs {
                writeln!(out, "{inode}\t{subvol}\t{ctx}\t{path}")?;
            }
        }
        return out.flush();
    }

    // Emit every entry whose parent directory landed in the live namespace.
    let emit = Instant::now();
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(4 << 20, stdout.lock());
    out.write_all(prefix.as_bytes())?;
    out.write_all(b"\n")?;

    let mut emitted = 0u64;
    for_each_run(fd, BTREE_DIRENTS, |inode, run| {
        let Some(contexts) = resolved.get(&inode) else {
            return Ok(());
        };
        for (subvol, ctx, path) in contexts {
            let Some(vis) = vis_cache.get(ctx) else {
                continue;
            };
            let Some(r) = choose_visible(run, vis, |r| r.snapshot) else {
                continue;
            };
            if r.ktype != KEY_TYPE_DIRENT {
                continue;
            }
            let d = parse_dirent(&r.bytes).ok_or_else(|| unparsable(&r.bytes))?;
            if is_pruned(prunes, path, d.name) {
                continue;
            }
            if d.d_type == DT_SUBVOL
                && (d.parent_subvol != *subvol
                    || !subvols
                        .get(&d.child_subvol)
                        .is_some_and(|sv| sv.state == SUBVOLUME_STATE_LIVE))
            {
                continue;
            }
            out.write_all(path.as_bytes())?;
            out.write_all(b"/")?;
            out.write_all(d.name)?;
            out.write_all(b"\n")?;
            emitted += 1;
        }
        Ok(())
    })?;
    out.flush()?;
    eprintln!(
        "emitted {emitted} paths in {:.2}s",
        emit.elapsed()
            .as_secs_f64()
    );
    Ok(())
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let fs = open_fs(
        cli.cmd
            .mount(),
    )?;
    let fd = fs.as_raw_fd();

    match cli.cmd {
        Cmd::Subvols { .. } => {
            let subvols = read_subvols(fd)?;
            let mut ids: Vec<_> = subvols
                .keys()
                .copied()
                .collect();
            ids.sort_unstable();
            for id in ids {
                let s = &subvols[&id];
                println!(
                    "subvol {id} snapshot {} root_inode {} state {:#010x}{}",
                    s.snapshot,
                    s.root_inode,
                    s.state,
                    if s.state == SUBVOLUME_STATE_LIVE {
                        " live"
                    } else {
                        " NOT-LIVE"
                    }
                );
            }
            eprintln!("{} subvolumes", subvols.len());
        }
        Cmd::Stats { .. } => {
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
        Cmd::Dump { .. } => {
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
            dump_dirs,
        } => {
            let prefix = prefix.unwrap_or(mount);
            cmd_paths(fd, &prefix, &prune, dump_dirs)?;
        }
    }
    Ok(())
}
