//! Layered Wine registry files.
//!
//! A Wine prefix's registry (`system.reg`, `user.reg`, `userdef.reg`) can be kept as layers, such
//! as a base, an app's own settings, and what changed while the app ran. `merge` puts the layers
//! together into the file Wine loads; `diff` writes what Wine's file changes relative to the
//! layers.
//!
//! Files are handled as bytes: Wine writes ASCII (other characters as escapes), but nothing here
//! needs the text to be valid UTF-8. Parsed keys and values borrow from the files' contents.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

const USAGE: &str = r#"Layered Wine registry files.

  regstack merge OUT LAYER...                    write LAYER1 + LAYER2 + ... as one registry file
  regstack diff [--keep] CURRENT OUT LAYER...    write what CURRENT changes relative to the layers;
                                                   --keep leaves out deletions (for a registry
                                                   written with another runtime's keys)

Layers are Wine registry files (as Wine writes them, or snippets with just key sections). On top of
Wine's format they may delete things, as in regedit's format:
  [-Software\\Key]       deletes the key and its subkeys
  "name"=-             deletes a value
Keys and value names are compared case-insensitively, as Wine does. Wine's key timestamps and
#time lines are ignored, so keys Wine only touched don't show up as changes, and diffs give keys
the timestamp 0, so the same changes always give the same file.
"#;

/// The separator between key names in Wine's files (an escaped backslash).
const SEP: &[u8] = b"\\\\";

type Bytes = Vec<u8>;

/// A lowercased key or value name, shared between a map's index and its entries.
type Name = Rc<[u8]>;

/// FNV-1a: the maps hold tens of thousands of short names from trusted files, where SipHash's
/// protection against crafted keys only costs time.
#[derive(Default)]
struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 { 0xcbf29ce484222325 } else { self.0 };
        for &b in bytes {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        self.0 = h;
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<Fnv>>;
type FastSet<K> = HashSet<K, BuildHasherDefault<Fnv>>;

/// A map that iterates in insertion order. Replacing a value keeps its place; a removed entry
/// that is inserted again goes to the end.
#[derive(Debug)]
struct OrderedMap<K, V> {
    entries: Vec<Option<(K, V)>>,
    index: FastMap<K, usize>,
}

impl<K: Hash + Eq + Clone + std::borrow::Borrow<[u8]>, V> OrderedMap<K, V> {
    fn new() -> Self {
        OrderedMap { entries: Vec::new(), index: FastMap::default() }
    }

    fn get(&self, key: &[u8]) -> Option<&V> {
        self.index.get(key).and_then(|&i| self.entries[i].as_ref()).map(|(_, v)| v)
    }

    fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        match self.index.get(key) {
            Some(&i) => self.entries[i].as_mut().map(|(_, v)| v),
            None => None,
        }
    }

    fn contains_key(&self, key: &[u8]) -> bool {
        self.index.contains_key(key)
    }

    fn insert(&mut self, key: K, value: V) {
        if let Some(&i) = self.index.get(key.borrow()) {
            self.entries[i].as_mut().unwrap().1 = value;
        } else {
            self.index.insert(key.clone(), self.entries.len());
            self.entries.push(Some((key, value)));
        }
    }

    fn remove(&mut self, key: &[u8]) {
        if let Some(i) = self.index.remove(key) {
            self.entries[i] = None;
        }
    }

    fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().flatten().map(|(k, v)| (k, v))
    }

    fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    fn into_iter(self) -> impl Iterator<Item = (K, V)> {
        self.entries.into_iter().flatten()
    }
}

/// A value line, with continuation lines joined.
#[derive(Debug, Clone, PartialEq)]
struct Value<'a> {
    line: Cow<'a, [u8]>,
    /// length of the name at the start of the line (`"name"` or `@`)
    name_len: usize,
}

impl Value<'_> {
    /// writes the line that deletes this value
    fn write_deletion(&self, out: &mut Bytes) {
        out.extend_from_slice(&self.line[..self.name_len]);
        out.extend_from_slice(b"=-");
    }
}

#[derive(Debug)]
struct Key<'a> {
    /// as written between the brackets (escaped)
    name: &'a [u8],
    /// Wine's modification time (seconds), kept from the lowest layer
    stamp: &'a [u8],
    /// `#class` / `#link` ... -> line; `#time` is dropped
    attrs: OrderedMap<&'a [u8], &'a [u8]>,
    /// lowercased value name -> line, or None where a layer deletes the value
    values: OrderedMap<Name, Option<Value<'a>>>,
}

#[derive(Debug)]
enum Op<'a> {
    Key(Key<'a>),
    /// `[-Key]`: the lowercased key name
    DelKey(Name),
}

#[derive(Debug)]
struct Error(String);

fn lower(s: &[u8]) -> Name {
    if s.is_ascii() {
        return s.to_ascii_lowercase().into();
    }
    match std::str::from_utf8(s) {
        Ok(s) => s.to_lowercase().into_bytes().into(),
        Err(_) => s.to_ascii_lowercase().into(),
    }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | 0x1c..=0x1f)
}

fn trim_start(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    &s[start..]
}

fn trim(s: &[u8]) -> &[u8] {
    let s = trim_start(s);
    let end = s.iter().rposition(|&c| !is_space(c)).map_or(0, |i| i + 1);
    &s[..end]
}

/// The key names leading to `name`: `A\\B\\C` -> `A`, `A\\B`.
fn parents(name: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut start = 0;
    std::iter::from_fn(move || {
        let p = name[start..].windows(SEP.len()).position(|w| w == SEP)?;
        let parent = &name[..start + p];
        start += p + SEP.len();
        Some(parent)
    })
}

fn text(line: &[u8]) -> String {
    String::from_utf8_lossy(line).into_owned()
}

/// Index just past the quoted name starting at `line[start] == '"'`.
fn name_end(line: &[u8], start: usize) -> Result<usize, Error> {
    let mut i = start + 1;
    while i < line.len() {
        match line[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err(Error(format!("unterminated name: {}", text(line))))
}

/// -> (preamble lines, operations in file order)
fn parse(data: &[u8]) -> Result<(Vec<&[u8]>, Vec<Op<'_>>), Error> {
    let mut lines = data.split(|&c| c == b'\n').peekable();
    let mut preamble = Vec::new();
    let mut ops: Vec<Op> = Vec::new();
    let mut key: Option<Key> = None;
    while let Some(line) = lines.next() {
        if line.starts_with(b"[") {
            if let Some(k) = key.take() {
                ops.push(Op::Key(k));
            }
            let end = line
                .iter()
                .rposition(|&c| c == b']')
                .filter(|&end| end > line.starts_with(b"[-") as usize)
                .ok_or_else(|| Error(format!("key without ']': {}", text(line))))?;
            if line.starts_with(b"[-") {
                ops.push(Op::DelKey(lower(&line[2..end])));
                continue;
            }
            let stamp = trim(&line[end + 1..]);
            key = Some(Key {
                name: &line[1..end],
                stamp: if stamp.is_empty() { b"0" } else { stamp },
                attrs: OrderedMap::new(),
                values: OrderedMap::new(),
            });
            continue;
        }
        let Some(k) = key.as_mut() else {
            if ops.is_empty() && !line.is_empty() {
                preamble.push(line);
            }
            continue;
        };
        let end = match line.first() {
            None => continue,
            Some(b'#') => {
                if !line.starts_with(b"#time=") {
                    let attr = line.split(|&c| c == b'=').next().unwrap();
                    k.attrs.insert(attr, line);
                }
                continue;
            }
            Some(b'"') => name_end(line, 0)?,
            Some(b'@') => 1,
            Some(_) => continue, // comments (;) and anything Wine wouldn't load
        };
        // hex values continue on the next line after a trailing backslash, indented or not
        // depending on who wrote the file; join them so equal values compare equal (Wine reads
        // long lines)
        let mut full = Cow::Borrowed(line);
        while full.ends_with(b"\\") {
            let Some(next) = lines.next() else { break };
            let joined = full.to_mut();
            joined.pop();
            joined.extend_from_slice(trim_start(next));
        }
        let value = if &full[end..] == b"=-" { None } else { Some(Value { line: full, name_len: end }) };
        k.values.insert(lower(&line[..end]), value);
    }
    if let Some(k) = key.take() {
        ops.push(Op::Key(k));
    }
    Ok((preamble, ops))
}

/// Lowercased key name -> Key, in first-seen order.
type Keys<'a> = OrderedMap<Name, Key<'a>>;

/// Merge layers -> (preamble, keys).
fn load(layers: &[Bytes]) -> Result<(Vec<&[u8]>, Keys<'_>), Error> {
    let mut preamble: Option<Vec<&[u8]>> = None;
    let mut keys: Keys = OrderedMap::new();
    for data in layers {
        let (pre, ops) = parse(data)?;
        if preamble.is_none() && !pre.is_empty() {
            preamble = Some(pre);
        }
        for op in ops {
            match op {
                Op::DelKey(name) => {
                    let gone: Vec<Name> = keys
                        .keys()
                        .filter(|k| {
                            k.starts_with(&name) && (k.len() == name.len() || k[name.len()..].starts_with(SEP))
                        })
                        .cloned()
                        .collect();
                    for k in gone {
                        keys.remove(&k);
                    }
                }
                Op::Key(mut key) => {
                    let lk = lower(key.name);
                    if let Some(old) = keys.get_mut(&lk) {
                        for (a, l) in key.attrs.into_iter() {
                            old.attrs.insert(a, l);
                        }
                        for (v, l) in key.values.into_iter() {
                            match l {
                                None => old.values.remove(&v),
                                Some(_) => old.values.insert(v, l),
                            }
                        }
                    } else {
                        let mut values = OrderedMap::new();
                        for (v, l) in key.values.into_iter() {
                            if l.is_some() {
                                values.insert(v, l);
                            }
                        }
                        key.values = values;
                        keys.insert(lk, key);
                    }
                }
            }
        }
    }
    let preamble = preamble.unwrap_or_else(|| vec![b"WINE REGISTRY Version 2"]);
    Ok((preamble, keys))
}

/// Output: the preamble, then sections separated by blank lines.
struct Out(Bytes);

impl Out {
    fn new(preamble: &[&[u8]], capacity: usize) -> Self {
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(&preamble.join(&b'\n'));
        out.push(b'\n');
        Out(out)
    }

    /// starts a section with `[name] stamp`; its lines follow with `line`
    fn key(&mut self, key: &Key, stamp: Option<&[u8]>) {
        self.0.extend_from_slice(b"\n[");
        self.0.extend_from_slice(key.name);
        self.0.extend_from_slice(b"] ");
        self.0.extend_from_slice(stamp.unwrap_or(key.stamp));
        self.0.push(b'\n');
    }

    fn line(&mut self, line: &[u8]) {
        self.0.extend_from_slice(line);
        self.0.push(b'\n');
    }

    fn deletion(&mut self, value: &Value) {
        value.write_deletion(&mut self.0);
        self.0.push(b'\n');
    }

    fn delete_key(&mut self, key: &Key) {
        self.0.extend_from_slice(b"\n[-");
        self.0.extend_from_slice(key.name);
        self.0.extend_from_slice(b"]\n");
    }
}

fn merge(layers: &[Bytes]) -> Result<Bytes, Error> {
    let (preamble, keys) = load(layers)?;
    let mut out = Out::new(&preamble, layers.iter().map(Vec::len).sum());
    for k in keys.values() {
        out.key(k, None);
        for l in k.attrs.values() {
            out.line(l);
        }
        for v in k.values.values().flatten() {
            out.line(&v.line);
        }
    }
    Ok(out.0)
}

fn diff(current: &[u8], layers: &[Bytes], keep: bool) -> Result<Bytes, Error> {
    const ZERO: Option<&[u8]> = Some(b"0");
    let (preamble, base) = load(layers)?;
    let (_, ops) = parse(current)?;
    let mut cur: Keys = OrderedMap::new();
    for op in ops {
        if let Op::Key(k) = op {
            cur.insert(lower(k.name), k);
        }
    }
    let mut out = Out::new(&preamble, 0);
    // Wine doesn't write keys that only hold subkeys, so a missing key with subkeys still exists
    let mut implied: FastSet<&[u8]> = FastSet::default();
    for lk in cur.keys() {
        implied.extend(parents(lk));
    }
    let mut deleted: FastSet<&[u8]> = FastSet::default();
    for (lk, b) in base.iter() {
        if keep || cur.contains_key(lk) {
            continue;
        }
        if implied.contains(&**lk) {
            // still there, but without values
            if !b.values.is_empty() {
                out.key(b, ZERO);
                for v in b.values.values().flatten() {
                    out.deletion(v);
                }
            }
            continue;
        }
        deleted.insert(lk);
        // only the topmost deleted key: deleting it deletes its subkeys
        if !parents(lk).any(|p| deleted.contains(p)) {
            out.delete_key(b);
        }
    }
    for (lk, c) in cur.iter() {
        let Some(b) = base.get(lk) else {
            out.key(c, ZERO);
            for l in c.attrs.values() {
                out.line(l);
            }
            for v in c.values.values().flatten() {
                out.line(&v.line);
            }
            continue;
        };
        let attrs: Vec<&[u8]> =
            c.attrs.iter().filter(|(a, l)| b.attrs.get(a) != Some(l)).map(|(_, l)| *l).collect();
        let values: Vec<&Value> = c
            .values
            .iter()
            .filter_map(|(v, l)| Some((v, l.as_ref()?)))
            .filter(|(v, l)| b.values.get(v).and_then(Option::as_ref).map(|b| &b.line) != Some(&l.line))
            .map(|(_, l)| l)
            .collect();
        let deletions: Vec<&Value> = if keep {
            Vec::new()
        } else {
            b.values.iter().filter_map(|(v, l)| l.as_ref().filter(|_| !c.values.contains_key(v))).collect()
        };
        if !attrs.is_empty() || !values.is_empty() || !deletions.is_empty() {
            out.key(c, ZERO);
            for l in attrs {
                out.line(l);
            }
            for v in values {
                out.line(&v.line);
            }
            for v in deletions {
                out.deletion(v);
            }
        }
    }
    Ok(out.0)
}

fn read(path: &Path) -> Result<Bytes, Error> {
    fs::read(path).map_err(|e| Error(format!("{}: {}", path.display(), e)))
}

fn read_all(paths: &[OsString]) -> Result<Vec<Bytes>, Error> {
    paths.iter().map(|p| read(Path::new(p))).collect()
}

/// Written whole or not at all: an interrupted write must not cost the save its registry.
fn write(path: &Path, data: &[u8]) -> Result<(), Error> {
    let mut tmp: OsString = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| -> io::Result<()> {
        fs::File::create(&tmp)?.write_all(data)?;
        fs::rename(&tmp, path)
    })();
    result.map_err(|e| Error(format!("{}: {}", path.display(), e)))
}

fn run(args: &[OsString]) -> Option<Result<(), Error>> {
    let cmd = args.first().and_then(|a| a.to_str()).unwrap_or("");
    let arg = |i: usize| Path::new(&args[i]);
    match cmd {
        "merge" if args.len() >= 3 => Some((|| {
            let data = merge(&read_all(&args[2..])?)?;
            write(arg(1), &data)
        })()),
        "diff" if args.len() >= 5 && args[1] == "--keep" => Some((|| {
            let data = diff(&read(arg(2))?, &read_all(&args[4..])?, true)?;
            write(arg(3), &data)
        })()),
        "diff" if args.len() >= 4 => Some((|| {
            let data = diff(&read(arg(1))?, &read_all(&args[3..])?, false)?;
            write(arg(2), &data)
        })()),
        _ => None,
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Some(Ok(())) => ExitCode::SUCCESS,
        Some(Err(Error(e))) => {
            eprintln!("regstack: {}", e);
            ExitCode::FAILURE
        }
        None => {
            eprint!("{}", USAGE);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "WINE REGISTRY Version 2\n;; All keys relative to \\\\Machine\n\n#arch=win64\n";

    fn layers(texts: &[&str]) -> Vec<Bytes> {
        texts.iter().map(|t| t.as_bytes().to_vec()).collect()
    }

    /// HEADER as written back: blank lines before the first key aren't kept
    const WRITTEN: &str = "WINE REGISTRY Version 2\n;; All keys relative to \\\\Machine\n#arch=win64\n";

    fn merged(texts: &[&str]) -> String {
        String::from_utf8(merge(&layers(texts)).unwrap()).unwrap()
    }

    fn diffed(current: &str, texts: &[&str], keep: bool) -> String {
        String::from_utf8(diff(current.as_bytes(), &layers(texts), keep).unwrap()).unwrap()
    }

    #[test]
    fn merge_overrides_values_and_keeps_order() {
        let base = format!(
            "{HEADER}\n[Software\\\\A] 1700000000\n#time=1da0000000000000\n\"x\"=dword:00000001\n\"y\"=\"old\"\n\n[Software\\\\B] 1700000000\n\"z\"=\"b\"\n"
        );
        let cart = "[software\\\\a] 0\n\"Y\"=\"new\"\n\"w\"=\"added\"\n";
        assert_eq!(
            merged(&[&base, cart]),
            format!(
                "{}\n[Software\\\\A] 1700000000\n\"x\"=dword:00000001\n\"Y\"=\"new\"\n\"w\"=\"added\"\n\n[Software\\\\B] 1700000000\n\"z\"=\"b\"\n",
                WRITTEN
            )
        );
    }

    #[test]
    fn merge_deletes_keys_and_values() {
        let base = format!(
            "{HEADER}\n[A] 1\n\"keep\"=\"1\"\n\"drop\"=\"2\"\n\n[A\\\\B] 1\n\"v\"=\"3\"\n\n[A\\\\B\\\\C] 1\n\"v\"=\"4\"\n\n[A\\\\BC] 1\n\"v\"=\"5\"\n"
        );
        let cart = "[-a\\\\b]\n\n[A] 0\n\"DROP\"=-\n";
        assert_eq!(
            merged(&[&base, cart]),
            format!("{}\n[A] 1\n\"keep\"=\"1\"\n\n[A\\\\BC] 1\n\"v\"=\"5\"\n", WRITTEN)
        );
    }

    #[test]
    fn merge_without_preamble() {
        assert_eq!(merged(&["[A] 0\n@=\"x\"\n"]), "WINE REGISTRY Version 2\n\n[A] 0\n@=\"x\"\n");
    }

    #[test]
    fn hex_continuations_compare_joined() {
        let base = "[A] 1\n\"h\"=hex:01,02,\\\n  03,04\n";
        let current = "[A] 5\n#time=1\n\"h\"=hex:01,02,\\\n03,04\n";
        assert_eq!(diffed(current, &[base], false), "WINE REGISTRY Version 2\n");
        assert_eq!(merged(&[base]), "WINE REGISTRY Version 2\n\n[A] 1\n\"h\"=hex:01,02,03,04\n");
    }

    #[test]
    fn diff_changes_additions_and_deletions() {
        let base = "[A] 1\n\"same\"=\"1\"\n\"changed\"=\"1\"\n\"gone\"=\"1\"\n\n[Gone] 1\n\"v\"=\"1\"\n\n[Gone\\\\Sub] 1\n\"v\"=\"1\"\n";
        let current = "[a] 9\n\"same\"=\"1\"\n\"changed\"=\"2\"\n\"new\"=\"3\"\n\n[New] 9\n#class=\"c\"\n\"v\"=\"1\"\n";
        assert_eq!(
            diffed(current, &[base], false),
            "WINE REGISTRY Version 2\n\n[-Gone]\n\n[a] 0\n\"changed\"=\"2\"\n\"new\"=\"3\"\n\"gone\"=-\n\n[New] 0\n#class=\"c\"\n\"v\"=\"1\"\n"
        );
        assert_eq!(
            diffed(current, &[base], true),
            "WINE REGISTRY Version 2\n\n[a] 0\n\"changed\"=\"2\"\n\"new\"=\"3\"\n\n[New] 0\n#class=\"c\"\n\"v\"=\"1\"\n"
        );
    }

    #[test]
    fn diff_keeps_keys_that_only_hold_subkeys() {
        let base = "[A] 1\n\"v\"=\"1\"\n\n[A\\\\B] 1\n\"v\"=\"1\"\n\n[E] 1\n";
        let current = "[A\\\\B] 1\n\"v\"=\"1\"\n";
        assert_eq!(diffed(current, &[base], false), "WINE REGISTRY Version 2\n\n[A] 0\n\"v\"=-\n\n[-E]\n");
    }

    #[test]
    fn diff_then_merge_round_trips() {
        let base = format!("{HEADER}\n[A] 1\n\"x\"=\"1\"\n\"y\"=\"2\"\n\n[A\\\\B] 1\n\"z\"=\"3\"\n");
        let current = format!("{HEADER}\n[A] 1\n\"x\"=\"9\"\n\n[C] 4\n\"n\"=\"1\"\n");
        let saved = diffed(&current, &[&base], false);
        assert_eq!(merged(&[&base, &saved]), format!("{}\n[A] 1\n\"x\"=\"9\"\n\n[C] 0\n\"n\"=\"1\"\n", WRITTEN));
    }

    #[test]
    fn errors() {
        assert!(parse(b"[A] 0\n\"unterminated=1\n").is_err());
        assert!(parse(b"[A 0\n").is_err());
    }
}
