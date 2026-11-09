use std::{
    collections::HashSet,
    fs::{self},
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

// The generic payload, shared by every customer and compiled in once per release.
#[cfg(windows)]
const BIN_DATA: &[u8] = include_bytes!("../data.bin");

// The per-customer payload, injected into the RCDATA resource after the template
// has been built, so that customizing a client needs no recompilation.
#[cfg(windows)]
const PACKAGE_RESOURCE_NAME: &str = "RDPKG";

// 4bytes
const LENGTH: usize = 4;
const IDENTIFIER: &[u8] = b"rustdesk";
const IDENTIFIER_LENGTH: usize = 8;
const MD5_LENGTH: usize = 32;
const BUF_SIZE: usize = 4096;

pub(crate) struct BinaryData {
    pub md5_code: &'static [u8],
    // compressed gzip data
    pub raw: &'static [u8],
    pub path: String,
}

pub(crate) struct BinaryReader {
    pub files: Vec<BinaryData>,
    pub exe: String,
    // Paths supplied by the per-customer package. Recorded so that a file dropped
    // from a later package -- a logo the customer removed, say -- can be deleted
    // from an existing extraction, which the timestamp wipe no longer covers now
    // that the packer is built once per release rather than once per customer.
    pub package_paths: Vec<String>,
}

impl BinaryReader {
    pub fn new() -> Result<Self, String> {
        let package = read_package()?;
        let package_paths = package.0.iter().map(|f| f.path.clone()).collect();
        let (files, exe) = merge(read_embedded()?, package);
        Ok(Self {
            files,
            exe,
            package_paths,
        })
    }
}

// Folds the per-customer package into the generic payload.
fn merge(
    embedded: (Vec<BinaryData>, String),
    package: (Vec<BinaryData>, String),
) -> (Vec<BinaryData>, String) {
    let (mut files, generic_exe) = embedded;
    let (package_files, package_exe) = package;

    let exe = if package_exe.is_empty() {
        generic_exe.clone()
    } else {
        package_exe
    };

    // The generic payload ships the executable under its stock name, the package
    // decides the final one. Rename on extraction so the process is always
    // `<appname>.exe`, which the app itself relies on to find its own sessions.
    if !generic_exe.is_empty() && normalize_path(&exe) != normalize_path(&generic_exe) {
        let generic_key = normalize_path(&generic_exe);
        for file in files.iter_mut() {
            if normalize_path(&file.path) == generic_key {
                file.path = exe.clone();
            }
        }
    }

    // Per-customer entries replace the generic ones they shadow.
    if !package_files.is_empty() {
        let overridden: HashSet<String> = package_files
            .iter()
            .map(|file| normalize_path(&file.path))
            .collect();
        files.retain(|file| !overridden.contains(&normalize_path(&file.path)));
        files.extend(package_files);
    }

    (files, exe)
}

pub(crate) fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .to_lowercase()
}

fn read_u32(blob: &[u8], at: usize) -> Option<u32> {
    let bytes = blob.get(at..at + LENGTH)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

// Returns the files and the executable to launch, or None if the blob is absent or malformed.
fn parse(blob: &'static [u8]) -> Option<(Vec<BinaryData>, String)> {
    let mut base = 0usize;
    let mut parsed = Vec::new();
    if blob.get(base..base + IDENTIFIER_LENGTH)? != IDENTIFIER {
        return None;
    }
    base += IDENTIFIER_LENGTH;
    loop {
        if blob.get(base..base + IDENTIFIER_LENGTH)? == IDENTIFIER {
            base += IDENTIFIER_LENGTH;
            break;
        }
        let path_length = read_u32(blob, base)? as usize;
        base += LENGTH;
        let path = std::str::from_utf8(blob.get(base..base + path_length)?)
            .ok()?
            .to_owned();
        base += path_length;
        let file_length = read_u32(blob, base)? as usize;
        base += LENGTH;
        let raw = blob.get(base..base + file_length)?;
        base += file_length;
        let md5_code = blob.get(base..base + MD5_LENGTH)?;
        base += MD5_LENGTH;
        parsed.push(BinaryData {
            md5_code,
            raw,
            path,
        });
    }
    let executable = std::str::from_utf8(blob.get(base..)?).ok()?.to_owned();
    Some((parsed, executable))
}

#[cfg(windows)]
fn read_embedded() -> Result<(Vec<BinaryData>, String), String> {
    parse(BIN_DATA).ok_or_else(|| "bin file is not valid!".to_owned())
}

#[cfg(not(windows))]
fn read_embedded() -> Result<(Vec<BinaryData>, String), String> {
    Ok(Default::default())
}

fn parse_package_blob(blob: Option<&'static [u8]>) -> Result<(Vec<BinaryData>, String), String> {
    let Some(blob) = blob else {
        return Ok(Default::default());
    };
    let package = parse(blob).ok_or_else(|| "RDPKG resource is invalid".to_owned())?;
    if package.1.trim().is_empty() {
        return Err("RDPKG resource has no executable".to_owned());
    }
    Ok(package)
}

#[cfg(windows)]
fn read_package() -> Result<(Vec<BinaryData>, String), String> {
    parse_package_blob(read_resource(PACKAGE_RESOURCE_NAME))
}

#[cfg(not(windows))]
fn read_package() -> Result<(Vec<BinaryData>, String), String> {
    Ok(Default::default())
}

// Reads an RCDATA resource out of the running image. Resources live in the mapped
// image for the lifetime of the process, so the slice is genuinely 'static and no
// copy is needed.
#[cfg(windows)]
fn read_resource(name: &str) -> Option<&'static [u8]> {
    use std::ptr::null_mut;
    use winapi::um::libloaderapi::{FindResourceW, LoadResource, LockResource, SizeofResource};

    // MAKEINTRESOURCEW(10), avoids depending on the winuser feature for RT_RCDATA.
    const RT_RCDATA: *const u16 = 10 as _;

    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let info = FindResourceW(null_mut(), name.as_ptr(), RT_RCDATA);
        if info.is_null() {
            return None;
        }
        let size = SizeofResource(null_mut(), info) as usize;
        if size == 0 {
            return None;
        }
        let handle = LoadResource(null_mut(), info);
        if handle.is_null() {
            return None;
        }
        let data = LockResource(handle) as *const u8;
        if data.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(data, size))
    }
}

/// A file of the payload that could not be put in place, and why.
#[derive(Debug)]
pub(crate) struct ExtractError {
    pub path: PathBuf,
    pub error: std::io::Error,
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}

impl BinaryData {
    fn decompress(&self) -> std::io::Result<Vec<u8>> {
        let cursor = Cursor::new(self.raw);
        let mut decoder = brotli::Decompressor::new(cursor, BUF_SIZE);
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn md5_record(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.md5_code)
    }

    /// Puts this file under `prefix`, unless an identical copy is already there.
    ///
    /// Every failure is returned rather than ignored: an extraction that silently
    /// left a file empty or half-written used to be launched anyway, and a 0-byte
    /// librustdesk.dll then only surfaced as Windows' "bad image" error, or as an app
    /// that does nothing on double-click, with no hint at the cause. The content is
    /// checked against the recorded md5 before anything is written (a damaged
    /// download), and written to a temporary file that replaces the old one only when
    /// complete, so a write that fails half-way -- a file in use, an antivirus
    /// blocking it, a full disk -- leaves the previous file as it was.
    pub fn write_to_file(&self, prefix: &Path) -> Result<(), ExtractError> {
        let p = prefix.join(&self.path);
        let fail = |error: std::io::Error| ExtractError {
            path: p.clone(),
            error,
        };
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(fail)?;
        }
        if p.exists() {
            // check md5
            let f = fs::read(&p).unwrap_or_default();
            let digest = format!("{:x}", md5::compute(&f));
            let md5_record = self.md5_record();
            if digest == md5_record {
                // same, skip this file
                println!("skip {}", &self.path);
                return Ok(());
            } else {
                println!("writing {}", p.display());
                println!("{} -> {}", md5_record, digest)
            }
        }
        let content = self.decompress().map_err(fail)?;
        let digest = format!("{:x}", md5::compute(&content));
        if digest != self.md5_record() {
            return Err(fail(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "the packaged copy is damaged (md5 {} instead of {})",
                    digest,
                    self.md5_record()
                ),
            )));
        }
        let mut tmp = p.clone().into_os_string();
        tmp.push(".part");
        let tmp = PathBuf::from(tmp);
        if let Err(error) = fs::write(&tmp, &content) {
            fs::remove_file(&tmp).ok();
            return Err(fail(error));
        }
        // On Windows this is MoveFileExW(MOVEFILE_REPLACE_EXISTING): it fails, and
        // leaves the old file, when that one is in use -- a DLL a running instance has
        // loaded, say.
        if let Err(error) = fs::rename(&tmp, &p) {
            fs::remove_file(&tmp).ok();
            return Err(fail(error));
        }
        Ok(())
    }
}

impl BinaryReader {
    #[cfg(linux)]
    pub fn configure_permission(&self, prefix: &Path) {
        use std::os::unix::prelude::PermissionsExt;

        let exe_path = prefix.join(&self.exe);
        if exe_path.exists() {
            if let Ok(f) = File::open(exe_path) {
                if let Ok(meta) = f.metadata() {
                    let mut permissions = meta.permissions();
                    permissions.set_mode(0o755);
                    f.set_permissions(permissions).ok();
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Builds a blob in the same layout generate.py writes, so these tests pin the
    // cross-language format contract as well as the merge rules.
    fn blob(files: &[(&str, &[u8])], exe: &str) -> &'static [u8] {
        let mut out = Vec::new();
        out.extend_from_slice(IDENTIFIER);
        for (path, data) in files {
            out.extend_from_slice(&(path.len() as u32).to_be_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(data);
            out.extend_from_slice(&[b'a'; MD5_LENGTH]);
        }
        out.extend_from_slice(IDENTIFIER);
        out.extend_from_slice(exe.as_bytes());
        Box::leak(out.into_boxed_slice())
    }

    fn entry<'a>(files: &'a [BinaryData], path: &str) -> Option<&'a BinaryData> {
        files
            .iter()
            .find(|file| normalize_path(&file.path) == normalize_path(path))
    }

    /// A payload entry the way generate.py makes one: brotli data, md5 of the original.
    pub(crate) fn packed(path: &str, content: &[u8]) -> BinaryData {
        let mut raw = Vec::new();
        {
            let mut w = brotli::CompressorWriter::new(&mut raw, BUF_SIZE, 5, 22);
            std::io::Write::write_all(&mut w, content).unwrap();
        }
        let md5 = format!("{:x}", md5::compute(content));
        BinaryData {
            md5_code: Box::leak(md5.into_bytes().into_boxed_slice()),
            raw: Box::leak(raw.into_boxed_slice()),
            path: path.to_owned(),
        }
    }

    pub(crate) fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portable-test-{}-{}", std::process::id(), name));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect()
    }

    #[test]
    fn writes_a_new_file_completely() {
        let dir = scratch_dir("new");
        packed("./data/app.dll", b"core").write_to_file(&dir).unwrap();
        assert_eq!(fs::read(dir.join("data/app.dll")).unwrap(), b"core");
        assert!(leftovers(&dir.join("data")).is_empty());
    }

    #[test]
    fn replaces_an_empty_file_and_skips_an_identical_one() {
        let dir = scratch_dir("replace");
        // What a failed extraction used to leave behind.
        fs::write(dir.join("librustdesk.dll"), b"").unwrap();
        let file = packed("./librustdesk.dll", b"core");
        file.write_to_file(&dir).unwrap();
        assert_eq!(fs::read(dir.join("librustdesk.dll")).unwrap(), b"core");
        file.write_to_file(&dir).unwrap();
        assert_eq!(fs::read(dir.join("librustdesk.dll")).unwrap(), b"core");
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn a_damaged_payload_is_an_error_and_keeps_the_old_file() {
        let dir = scratch_dir("damaged");
        fs::write(dir.join("librustdesk.dll"), b"old").unwrap();
        let mut file = packed("./librustdesk.dll", b"core");
        file.raw = b"\xff\xff not brotli at all";
        let err = file.write_to_file(&dir).unwrap_err();
        assert!(err.path.ends_with("librustdesk.dll"));
        assert_eq!(fs::read(dir.join("librustdesk.dll")).unwrap(), b"old");
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn content_that_does_not_match_its_md5_is_not_written() {
        let dir = scratch_dir("md5");
        let mut file = packed("./librustdesk.dll", b"core");
        file.md5_code = packed("x", b"something else").md5_code;
        let err = file.write_to_file(&dir).unwrap_err();
        assert_eq!(err.error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!dir.join("librustdesk.dll").exists());
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn a_target_that_cannot_be_replaced_is_an_error_without_leftovers() {
        let dir = scratch_dir("blocked");
        // A directory where the file should go: the rename onto it fails, as it does
        // on Windows for a DLL a running instance has loaded.
        fs::create_dir_all(dir.join("librustdesk.dll").join("inside")).unwrap();
        let err = packed("./librustdesk.dll", b"core").write_to_file(&dir).unwrap_err();
        assert!(err.to_string().contains("librustdesk.dll"));
        assert!(dir.join("librustdesk.dll").join("inside").is_dir());
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn distinguishes_an_absent_package_from_a_malformed_one() {
        assert!(parse_package_blob(None).unwrap().0.is_empty());
        assert!(parse_package_blob(Some(b"damaged")).is_err());
        assert!(parse_package_blob(Some(blob(&[("./custom.txt", b"cfg")], ""))).is_err());
    }

    #[test]
    fn without_a_package_the_stock_payload_is_untouched() {
        let embedded = parse(blob(&[("./rustdesk.exe", b"app")], "./rustdesk.exe")).unwrap();
        let (files, exe) = merge(embedded, Default::default());
        assert_eq!(exe, "./rustdesk.exe");
        assert!(entry(&files, "./rustdesk.exe").is_some());
    }

    #[test]
    fn renames_the_stock_executable_to_the_package_name() {
        // x86: the big executable stays in the generic payload and only gets renamed.
        let embedded = parse(blob(
            &[("./rustdesk.exe", b"app"), ("./sciter.dll", b"dll")],
            "./rustdesk.exe",
        ))
        .unwrap();
        let package = parse(blob(&[("./custom.txt", b"cfg")], "./acme.exe")).unwrap();

        let (files, exe) = merge(embedded, package);

        assert_eq!(exe, "./acme.exe");
        assert!(entry(&files, "./acme.exe").is_some());
        assert!(entry(&files, "./rustdesk.exe").is_none());
        // Untouched neighbours survive.
        assert_eq!(entry(&files, "./sciter.dll").unwrap().raw, b"dll");
        assert_eq!(entry(&files, "./custom.txt").unwrap().raw, b"cfg");
    }

    #[test]
    fn package_entries_win_over_the_generic_payload() {
        // x64: the customized executable and icons ship in the package instead.
        let embedded = parse(blob(
            &[
                ("./data/flutter_assets/assets/icon.ico", b"stock-icon"),
                ("./librustdesk.dll", b"core"),
            ],
            "./rustdesk.exe",
        ))
        .unwrap();
        let package = parse(blob(
            &[
                ("./acme.exe", b"branded"),
                ("./data/flutter_assets/assets/icon.ico", b"acme-icon"),
            ],
            "./acme.exe",
        ))
        .unwrap();

        let (files, exe) = merge(embedded, package);

        assert_eq!(exe, "./acme.exe");
        assert_eq!(
            entry(&files, "./data/flutter_assets/assets/icon.ico")
                .unwrap()
                .raw,
            b"acme-icon"
        );
        assert_eq!(
            files
                .iter()
                .filter(|f| normalize_path(&f.path) == "data/flutter_assets/assets/icon.ico")
                .count(),
            1
        );
        assert_eq!(entry(&files, "./librustdesk.dll").unwrap().raw, b"core");
    }
}
