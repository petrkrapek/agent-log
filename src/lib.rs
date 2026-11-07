//! Záznam akcí agenta do JSON Lines souboru s omezenou cestou a právy `0o600`.

#![deny(unsafe_code)]

#[cfg(not(unix))]
compile_error!("agent-log vyžaduje Unix kvůli O_NOFOLLOW, flock a právům 0o600");

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use chrono::{Local, SecondsFormat};
use serde::{Deserialize, Serialize};

/// Nejdelší přijatá zpráva v bajtech (UTF-8).
pub const MAX_MESSAGE_BYTES: usize = 4096;

/// Nejdelší řádek, který `list` ještě přečte. Kryje nejhorší JSON únik řídicích znaků
/// ve zprávě o délce [`MAX_MESSAGE_BYTES`].
const MAX_LINE_BYTES: usize = 32 * 1024;

/// Jeden záznam v logu.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ts: String,
    pub message: String,
}

/// Chyba operace nad logem.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    MessageTooLong { len: usize, max: usize },
    OutsideWorkspace,
    Symlink,
    HardLink,
    NotAFile,
    Missing,
    SkippedLines(usize),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::MessageTooLong { len, max } => {
                write!(f, "zpráva má {len} bajtů, limit je {max}")
            }
            Self::OutsideWorkspace => write!(
                f,
                "cesta k logu musí zůstat v aktuálním adresáři; pro jinou cestu použijte --allow-outside"
            ),
            Self::Symlink => write!(f, "cesta k logu nesmí obsahovat symlink"),
            Self::HardLink => write!(f, "soubor logu nesmí mít další pevný odkaz (hard link)"),
            Self::NotAFile => write!(f, "cíl musí být obyčejný soubor"),
            Self::Missing => write!(f, "log neexistuje"),
            Self::SkippedLines(n) => write!(f, "{}", skipped_lines_message(*n)),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        map_io(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::Io(err.into())
    }
}

fn skipped_lines_message(n: usize) -> String {
    match n {
        1 => "v logu je 1 poškozený řádek".to_owned(),
        2..=4 => format!("v logu jsou {n} poškozené řádky"),
        _ => format!("v logu je {n} poškozených řádků"),
    }
}

fn map_io(err: io::Error) -> Error {
    match err.raw_os_error() {
        Some(libc::ELOOP) => Error::Symlink,
        // ENXIO vrací neblokující open FIFO pro zápis, když ji nikdo nečte.
        Some(libc::EISDIR | libc::ENXIO) => Error::NotAFile,
        _ => Error::Io(err),
    }
}

/// Text bezpečný pro vypsání do terminálu: řídicí, oddělovací a bidi znaky
/// jsou nahrazené U+FFFD.
#[must_use]
pub fn sanitize_for_display(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if is_unsafe_terminal_char(c) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

fn is_unsafe_terminal_char(c: char) -> bool {
    c.is_control()
        || c == '\u{2028}'
        || c == '\u{2029}'
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// Řádek výpisu `[čas] zpráva` s odfiltrovanými řídicími znaky.
#[must_use]
pub fn format_entry(entry: &Entry) -> String {
    format!(
        "[{}] {}",
        sanitize_for_display(&entry.ts),
        sanitize_for_display(&entry.message)
    )
}

/// Připíše zprávu do logu a vrátí zapsaný záznam.
pub fn add_entry(
    base_dir: &Path,
    file: &Path,
    allow_outside: bool,
    message: &str,
) -> Result<Entry, Error> {
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(Error::MessageTooLong {
            len: message.len(),
            max: MAX_MESSAGE_BYTES,
        });
    }
    let path = resolve_log_path(base_dir, file, allow_outside)?;
    let entry = Entry {
        ts: Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        message: message.to_owned(),
    };
    let mut line = serde_json::to_vec(&entry)?;
    if line.len() > MAX_LINE_BYTES {
        return Err(Error::MessageTooLong {
            len: line.len(),
            max: MAX_LINE_BYTES,
        });
    }
    line.push(b'\n');

    let mut log = open_log(&path, OpenMode::Append)?;
    unix_fs::flock_exclusive(&log)?;
    restrict_permissions(&log)?;
    // Useknutý poslední řádek (ruční úprava, plný disk) by se jinak slepil s novým záznamem.
    if !ends_with_newline(&log)? {
        line.insert(0, b'\n');
    }
    log.write_all(&line)?;
    Ok(entry)
}

/// Vypíše záznamy. Poškozené řádky přeskočí a vrátí chybu, když nějaké byly.
///
/// Zámek se drží jen po dobu čtení souboru. Pomalý čtenář výstupu (např. pager)
/// tak neblokuje souběžné `add`.
pub fn list_entries(
    base_dir: &Path,
    file: &Path,
    allow_outside: bool,
    limit: usize,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let path = resolve_log_path(base_dir, file, allow_outside)?;
    let log = match open_log(&path, OpenMode::Read) {
        Ok(file) => file,
        Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => return Err(Error::Missing),
        Err(err) => return Err(err),
    };
    let (entries, skipped) = read_entries(log, limit)?;

    for entry in &entries {
        writeln!(out, "{}", format_entry(entry))?;
    }
    if entries.is_empty() && skipped == 0 {
        writeln!(out, "Log je prázdný.")?;
    }
    if skipped > 0 {
        return Err(Error::SkippedLines(skipped));
    }
    Ok(())
}

/// Zkrátí log na prázdný soubor. Když chybí, vytvoří ho.
pub fn clear_log(base_dir: &Path, file: &Path, allow_outside: bool) -> Result<(), Error> {
    let path = resolve_log_path(base_dir, file, allow_outside)?;
    let log = open_log(&path, OpenMode::Write)?;
    unix_fs::flock_exclusive(&log)?;
    restrict_permissions(&log)?;
    log.set_len(0)?;
    Ok(())
}

/// Přečte platné záznamy pod sdíleným zámkem a vrátí je s počtem poškozených řádků.
/// Při `limit > 0` drží jen posledních `limit` záznamů. Zámek uvolní zavřením souboru.
fn read_entries(log: File, limit: usize) -> Result<(VecDeque<Entry>, usize), Error> {
    unix_fs::flock_shared(&log)?;
    let mut reader = BufReader::new(log);
    let mut skipped = 0usize;
    let mut entries = VecDeque::new();

    while let Some(line) = read_line_capped(&mut reader, MAX_LINE_BYTES)? {
        let Some(entry) = parse_line(line, &mut skipped) else {
            continue;
        };
        if limit > 0 && entries.len() == limit {
            entries.pop_front();
        }
        entries.push_back(entry);
    }
    Ok((entries, skipped))
}

fn parse_line(line: Line, skipped: &mut usize) -> Option<Entry> {
    let bytes = match line {
        Line::TooLong => {
            *skipped += 1;
            return None;
        }
        Line::Bytes(bytes) => bytes,
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let entry = String::from_utf8(bytes)
        .ok()
        .and_then(|text| serde_json::from_str::<Entry>(&text).ok());
    if entry.is_none() {
        *skipped += 1;
    }
    entry
}

/// Soubor je prázdný, nebo končí znakem `\n`.
fn ends_with_newline(file: &File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    let mut last = [0u8; 1];
    file.read_exact_at(&mut last, len - 1)?;
    Ok(last[0] == b'\n')
}

enum Line {
    Bytes(Vec<u8>),
    TooLong,
}

fn read_line_capped(reader: &mut impl BufRead, cap: usize) -> io::Result<Option<Line>> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return finish_line(buf, cap);
        }
        if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
            let piece_len = pos + 1;
            if buf.len().saturating_add(piece_len) > cap.saturating_add(2) {
                reader.consume(piece_len);
                return Ok(Some(Line::TooLong));
            }
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(piece_len);
            return finish_line(buf, cap);
        }
        if buf.len().saturating_add(available.len()) > cap {
            let pending = available.len();
            reader.consume(pending);
            discard_until_newline(reader)?;
            return Ok(Some(Line::TooLong));
        }
        buf.extend_from_slice(available);
        let pending = available.len();
        reader.consume(pending);
    }
}

fn finish_line(mut buf: Vec<u8>, cap: usize) -> io::Result<Option<Line>> {
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.len() > cap {
        Ok(Some(Line::TooLong))
    } else {
        Ok(Some(Line::Bytes(buf)))
    }
}

fn discard_until_newline(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
            reader.consume(pos + 1);
            return Ok(());
        }
        let pending = available.len();
        reader.consume(pending);
    }
}

fn resolve_log_path(base_dir: &Path, file: &Path, allow_outside: bool) -> Result<PathBuf, Error> {
    let base = base_dir.canonicalize()?;
    let full = lexical_absolute(&base, file)?;
    if !allow_outside && !is_inside(&base, &full) {
        return Err(Error::OutsideWorkspace);
    }
    Ok(full)
}

fn is_inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root) && path != root
}

fn lexical_absolute(base: &Path, requested: &Path) -> io::Result<PathBuf> {
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        base.join(requested)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cesta k logu uniká nad kořen souborového systému",
                    ));
                }
            }
            Component::Normal(name) => out.push(name),
        }
    }
    if !out.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cestu k logu nelze ukotvit",
        ));
    }
    Ok(out)
}

enum OpenMode {
    Read,
    Append,
    Write,
}

fn open_log(path: &Path, mode: OpenMode) -> Result<File, Error> {
    unix_fs::open_log(path, mode)
}

fn restrict_permissions(file: &File) -> Result<(), Error> {
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[allow(unsafe_code)]
mod unix_fs {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io::{self, ErrorKind};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path};

    use super::{map_io, Error, OpenMode};

    /// Otevře log procházením cesty od `/` přes `openat` s `O_NOFOLLOW` v každém kroku.
    /// Odmítne symlink, pevný odkaz i cokoli jiného než obyčejný soubor.
    pub(super) fn open_log(path: &Path, mode: OpenMode) -> Result<File, Error> {
        let parts = split_absolute(path)?;
        let Some((file_name, dirs)) = parts.split_last() else {
            return Err(io::Error::new(ErrorKind::InvalidInput, "prázdná cesta k logu").into());
        };
        let mut dir = open_root().map_err(map_io)?;
        for component in dirs {
            dir = open_dir(&dir, &c_name(component)?).map_err(map_io)?;
        }
        let file = File::from(open_file(&dir, &c_name(file_name)?, mode).map_err(map_io)?);
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(Error::NotAFile);
        }
        // Pevný odkaz vede na cizí soubor, který by `add` a `clear` přepsaly a `list` vypsal.
        if meta.nlink() != 1 {
            return Err(Error::HardLink);
        }
        clear_nonblock(&file)?;
        Ok(file)
    }

    pub(super) fn flock_exclusive(file: &File) -> Result<(), Error> {
        flock_wait(file.as_raw_fd(), libc::LOCK_EX)
    }

    pub(super) fn flock_shared(file: &File) -> Result<(), Error> {
        flock_wait(file.as_raw_fd(), libc::LOCK_SH)
    }

    fn open_root() -> io::Result<OwnedFd> {
        // SAFETY: `c"/"` je platný C řetězec; návratovou hodnotu kontroluje `owned_fd`.
        owned_fd(unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })
    }

    fn open_dir(dir: &OwnedFd, name: &CString) -> io::Result<OwnedFd> {
        // SAFETY: `dir` je otevřený adresář a `name` platný C řetězec.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        match owned_fd(fd) {
            // Linux vrací u symlinku s O_DIRECTORY kód ENOTDIR, ne ELOOP.
            Err(err) if err.raw_os_error() == Some(libc::ENOTDIR) && is_symlink(dir, name) => {
                Err(io::Error::from_raw_os_error(libc::ELOOP))
            }
            result => result,
        }
    }

    fn is_symlink(dir: &OwnedFd, name: &CString) -> bool {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` je C řetězec a `stat` je výstupní buffer volání `fstatat`.
        // `AT_SYMLINK_NOFOLLOW` čte samotný symlink v adresáři, který už držíme.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return false;
        }
        // SAFETY: `fstatat` uspěl, takže buffer je inicializovaný.
        let stat = unsafe { stat.assume_init() };
        (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK
    }

    fn open_file(dir: &OwnedFd, name: &CString, mode: OpenMode) -> io::Result<OwnedFd> {
        let access = match mode {
            OpenMode::Read => libc::O_RDONLY,
            // Čtení kvůli kontrole, zda soubor končí znakem nového řádku.
            OpenMode::Append => libc::O_RDWR | libc::O_CREAT | libc::O_APPEND,
            OpenMode::Write => libc::O_WRONLY | libc::O_CREAT,
        };
        // O_NONBLOCK: open na FIFO nebo zařízení nesmí čekat. Typ souboru se ověří až po
        // otevření a příznak se pak zase shodí. O_NOCTTY: terminál se nestane řídicím.
        let flags = access | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
        // Variadický argument: `mode_t` je na macOS `u16`, proto se předává jako `c_uint`.
        let perms: libc::c_uint = 0o600;
        // SAFETY: `dir` je otevřený adresář a `name` platný C řetězec.
        owned_fd(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, perms) })
    }

    fn clear_nonblock(file: &File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` patří otevřenému `File`; F_GETFL/F_SETFL jen čtou a mění příznaky.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: viz výše.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn owned_fd(fd: RawFd) -> io::Result<OwnedFd> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` je právě otevřený deskriptor, který nikdo jiný nevlastní.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn flock_wait(fd: RawFd, op: i32) -> Result<(), Error> {
        loop {
            // SAFETY: `fd` patří otevřenému `File`, které volající drží po celou dobu zámku.
            let rc = unsafe { libc::flock(fd, op) };
            if rc == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(map_io(err));
        }
    }

    fn split_absolute(path: &Path) -> io::Result<Vec<&OsStr>> {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => parts.push(name),
                _ => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        "cesta k logu není normalizovaná",
                    ));
                }
            }
        }
        Ok(parts)
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes().to_vec())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "složka cesty obsahuje znak NUL"))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn sanitize_replaces_controls_and_keeps_text() {
        let raw = "a\nb\u{1b}[31mX\u{202e}Y";
        let clean = sanitize_for_display(raw);
        assert_eq!(clean.chars().filter(|c| *c == '\u{FFFD}').count(), 3);
        assert!(!clean.contains('\n'));
        assert!(!clean.contains('\u{1b}'));
        assert!(clean.contains('a') && clean.contains('X') && clean.contains('Y'));
    }

    #[test]
    fn lexical_parent_stays_absolute_and_inside_check_rejects_escape() {
        let base = Path::new("/work/project");
        let inside = lexical_absolute(base, Path::new("agent.log")).unwrap();
        assert_eq!(inside, Path::new("/work/project/agent.log"));
        assert!(is_inside(base, &inside));

        let outside = lexical_absolute(base, Path::new("../secret.log")).unwrap();
        assert_eq!(outside, Path::new("/work/secret.log"));
        assert!(!is_inside(base, &outside));
    }

    #[test]
    fn capped_reader_keeps_short_lines_and_skips_long_ones() {
        let mut input = Vec::new();
        input.extend_from_slice(b"ok\n");
        input.extend(std::iter::repeat_n(b'A', 10));
        input.push(b'\n');
        input.extend_from_slice(b"tail");
        let mut cursor = Cursor::new(input);
        let cap = 4;
        let first = read_line_capped(&mut cursor, cap).unwrap().unwrap();
        match first {
            Line::Bytes(bytes) => assert_eq!(bytes, b"ok"),
            Line::TooLong => panic!("krátký řádek"),
        }
        assert!(matches!(
            read_line_capped(&mut cursor, cap).unwrap(),
            Some(Line::TooLong)
        ));
        match read_line_capped(&mut cursor, cap).unwrap().unwrap() {
            Line::Bytes(bytes) => assert_eq!(bytes, b"tail"),
            Line::TooLong => panic!("poslední řádek"),
        }
        assert!(read_line_capped(&mut cursor, cap).unwrap().is_none());
    }
}
