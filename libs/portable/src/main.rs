#![windows_subsystem = "windows"]

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use bin_reader::{normalize_path, BinaryReader};

pub mod bin_reader;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
const APP_METADATA: &[u8] = include_bytes!("../app_metadata.toml");
#[cfg(not(windows))]
const APP_METADATA: &[u8] = &[];
const APP_METADATA_CONFIG: &str = "meta.toml";
const META_LINE_PREFIX_TIMESTAMP: &str = "timestamp = ";
const META_LINE_PREFIX_FILE: &str = "file = ";
const APP_PREFIX: &str = "rustdesk";
#[cfg(windows)]
const VERIFIED_UPDATE_FILE_PREFIX: &str = "rustdesk-verified-";
const APPNAME_RUNTIME_ENV_KEY: &str = "RUSTDESK_APPNAME";
#[cfg(windows)]
const SET_FOREGROUND_WINDOW_ENV_KEY: &str = "SET_FOREGROUND_WINDOW";

// The extraction directory follows whatever executable the payload asks for, so a
// custom client gets its own directory instead of sharing RustDesk's. Falls back to
// APP_PREFIX when no package is injected, which keeps stock builds unchanged.
fn app_dir_name(exe: &str) -> String {
    Path::new(&exe.replace('\\', "/"))
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.trim().to_lowercase())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| APP_PREFIX.to_owned())
}

fn is_timestamp_matches(dir: &Path, ts: &mut u64) -> bool {
    is_timestamp_matches_with(APP_METADATA, dir, ts)
}

fn is_timestamp_matches_with(app_metadata: &[u8], dir: &Path, ts: &mut u64) -> bool {
    let Ok(app_metadata) = std::str::from_utf8(app_metadata) else {
        return true;
    };
    for line in app_metadata.lines() {
        if line.starts_with(META_LINE_PREFIX_TIMESTAMP) {
            if let Ok(stored_ts) = line.replace(META_LINE_PREFIX_TIMESTAMP, "").parse::<u64>() {
                *ts = stored_ts;
                break;
            }
        }
    }
    if *ts == 0 {
        return true;
    }

    if let Ok(content) = std::fs::read_to_string(dir.join(APP_METADATA_CONFIG)) {
        for line in content.lines() {
            if line.starts_with(META_LINE_PREFIX_TIMESTAMP) {
                if let Ok(stored_ts) = line.replace(META_LINE_PREFIX_TIMESTAMP, "").parse::<u64>() {
                    return *ts == stored_ts;
                }
            }
        }
    }
    false
}

fn write_meta(dir: &Path, ts: u64, package_paths: &[String]) {
    let meta_file = dir.join(APP_METADATA_CONFIG);
    let mut content = format!("{}{}\n", META_LINE_PREFIX_TIMESTAMP, ts);
    for path in package_paths {
        content.push_str(&format!("{}{}\n", META_LINE_PREFIX_FILE, path));
    }
    // Ignore is ok here
    let _ = std::fs::write(meta_file, content);
}

fn previous_package_files(dir: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(dir.join(APP_METADATA_CONFIG)) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| line.strip_prefix(META_LINE_PREFIX_FILE))
        .map(|path| path.trim().to_owned())
        .collect()
}

// meta.toml is plain text in a user-writable directory, and it now drives deletion,
// so the path is rebuilt from plain components rather than joined as written. A
// prefix, root or parent component would otherwise escape the extraction directory:
// Path::join replaces the base entirely when given an absolute path.
fn resolve_within(dir: &Path, relative: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut path = dir.to_path_buf();
    let mut any = false;
    for component in Path::new(&relative.replace('\\', "/")).components() {
        match component {
            Component::Normal(part) => {
                // A drive-relative name like "C:x" parses as Normal, and only a
                // Windows host would classify "C:/..." as a Prefix, so the colon is
                // rejected outright rather than relying on the host's parser.
                if part.to_string_lossy().contains(':') {
                    return None;
                }
                path.push(part);
                any = true;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    if any {
        Some(path)
    } else {
        None
    }
}

// A customer who drops a branding asset gets a package without it, and the file
// would otherwise linger in an existing extraction and keep being used. The wipe
// cannot cover this: it is keyed on the packer's build timestamp, which is now the
// same for every customer of a release.
fn remove_dropped_package_files_with<F>(
    dir: &Path,
    current: &[String],
    mut remove_file: F,
) -> Vec<String>
where
    F: FnMut(&Path) -> std::io::Result<()>,
{
    let keep: std::collections::HashSet<String> =
        current.iter().map(|p| normalize_path(p)).collect();
    let mut failed = Vec::new();
    for previous in previous_package_files(dir) {
        if keep.contains(&normalize_path(&previous)) {
            continue;
        }
        let Some(path) = resolve_within(dir, &previous) else {
            continue;
        };
        if path.is_file() {
            println!("removing dropped {}", previous);
            if let Err(error) = remove_file(&path) {
                eprintln!("failed to remove dropped {}: {}", previous, error);
                failed.push(previous);
            }
        }
    }
    failed
}

fn remove_dropped_package_files(dir: &Path, current: &[String]) -> Vec<String> {
    remove_dropped_package_files_with(dir, current, |path| std::fs::remove_file(path))
}

/// Why the payload could not be unpacked; the app is not started then.
#[derive(Debug)]
enum SetupError {
    /// This file itself is damaged, e.g. a download that stopped early.
    Payload(String),
    NoDataDir,
    Extract(bin_reader::ExtractError),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Payload(e) => write!(f, "this file is damaged ({}); download it again", e),
            SetupError::NoDataDir => write!(f, "the local application data folder was not found"),
            SetupError::Extract(e) => write!(f, "{}", e),
        }
    }
}

fn setup(
    reader: BinaryReader,
    dir: Option<PathBuf>,
    clear: bool,
    args: &Vec<String>,
    ui: &mut bool,
) -> Result<PathBuf, SetupError> {
    setup_with(APP_METADATA, reader, dir, clear, args, ui)
}

fn setup_with(
    app_metadata: &[u8],
    reader: BinaryReader,
    dir: Option<PathBuf>,
    clear: bool,
    _args: &Vec<String>,
    _ui: &mut bool,
) -> Result<PathBuf, SetupError> {
    let dir = if let Some(dir) = dir {
        dir
    } else {
        // home dir
        dirs::data_local_dir()
            .map(|dir| dir.join(app_dir_name(&reader.exe)))
            .ok_or(SetupError::NoDataDir)?
    };

    let mut ts = 0;
    if clear || !is_timestamp_matches_with(app_metadata, &dir, &mut ts) {
        #[cfg(windows)]
        if _args.is_empty() {
            *_ui = true;
            ui::setup();
        }
        std::fs::remove_dir_all(&dir).ok();
    }
    bin_reader::remove_partial_files(&dir);
    let mut metadata_paths = reader.package_paths.clone();
    metadata_paths.extend(remove_dropped_package_files(&dir, &reader.package_paths));
    for file in reader.files.iter() {
        if let Err(e) = file.write_to_file(&dir) {
            // A matching timestamp is what lets the next run trust this directory and
            // skip the wipe; recorded as 0 it never matches, so the next run starts
            // over from an empty directory. The file records stay, for the cleanup of
            // dropped package files.
            write_meta(&dir, 0, &metadata_paths);
            return Err(SetupError::Extract(e));
        }
    }
    write_meta(&dir, ts, &metadata_paths);
    // Not in tests: it runs taskkill on the privacy-mode helper.
    #[cfg(all(windows, not(test)))]
    win::copy_runtime_broker(&dir);
    #[cfg(linux)]
    reader.configure_permission(&dir);
    Ok(dir.join(&reader.exe))
}

/// The display name for messages: the executable the payload launches, without
/// its extension, e.g. "jerinteDesk".
fn app_display_name(exe: &str) -> String {
    Path::new(&exe.replace('\\', "/"))
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.trim().to_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "RustDesk".to_owned())
}

/// The name to show when the payload cannot be read: this file's own name.
fn current_exe_display_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|n| app_display_name(&n))
        .unwrap_or_else(|| app_display_name(""))
}

fn setup_failure_message(app: &str, error: &SetupError) -> String {
    format!(
        "{app} 解压失败，没有启动。\n\
         {error}\n\n\
         请依次尝试：\n\
         1. 退出正在运行的 {app}（包括屏幕右下角托盘里的图标），再运行一次本文件；\n\
         2. 右键本文件，选择「以管理员身份运行」；\n\
         3. 检查杀毒软件是否拦截或隔离了 {app} 的文件，加入信任后再试。\n\n\
         {app} could not be unpacked and was not started.\n\
         {error}\n\n\
         Quit any running {app} (including its tray icon) and run this file again. If it \
         still fails, right-click it and choose \"Run as administrator\", or check whether \
         antivirus software blocked {app}'s files."
    )
}

fn use_null_stdio() -> bool {
    #[cfg(windows)]
    {
        // When running in CMD on Windows 7, using Stdio::inherit() with spawn returns an "invalid handle" error.
        // Since using Stdio::null() didn’t cause any issues, and determining whether the program is launched from CMD or by double-clicking would require calling more APIs during startup, we also use Stdio::null() when launched by double-clicking on Windows 7.
        let is_windows_7 = is_windows_7();
        println!("is windows7: {}", is_windows_7);
        return is_windows_7;
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn is_windows_7() -> bool {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    unsafe {
        let mut version_info = OSVERSIONINFOW::default();
        version_info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;

        if RtlGetVersion(&mut version_info).is_ok() {
            // Windows 7 is version 6.1
            println!(
                "Windows version: {}.{}",
                version_info.dwMajorVersion, version_info.dwMinorVersion
            );
            return version_info.dwMajorVersion == 6 && version_info.dwMinorVersion == 1;
        }
    }
    false
}

fn execute(path: PathBuf, args: Vec<String>, _ui: bool) {
    println!("executing {}", path.display());
    // setup env
    let exe = std::env::current_exe().unwrap_or_default();
    let exe_name = exe.file_name().unwrap_or_default();
    // run executable
    let mut cmd = Command::new(path);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);
        if _ui {
            cmd.env(SET_FOREGROUND_WINDOW_ENV_KEY, "1");
        }
    }

    cmd.env(APPNAME_RUNTIME_ENV_KEY, exe_name);
    if use_null_stdio() {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    } else {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    }
    let _child = cmd.spawn();

    #[cfg(windows)]
    if _ui {
        match _child {
            Ok(child) => unsafe {
                winapi::um::winuser::AllowSetForegroundWindow(child.id() as u32);
            },
            Err(e) => {
                eprintln!("{:?}", e);
            }
        }
    }
}

fn main() -> Result<(), String> {
    let mut args = Vec::new();
    let mut arg_exe = Default::default();
    let mut i = 0;
    for arg in std::env::args() {
        if i == 0 {
            arg_exe = arg.clone();
        } else {
            args.push(arg);
        }
        i += 1;
    }
    let click_setup = args.is_empty() && arg_exe.to_lowercase().ends_with("install.exe");
    #[cfg(windows)]
    let quick_support = args.is_empty() && win::is_quick_support_exe(&arg_exe);
    #[cfg(not(windows))]
    let quick_support = false;

    #[cfg(windows)]
    if args.first().is_some_and(|arg| arg == "--update") {
        if let Err(err) = win::schedule_verified_update_cleanup() {
            eprintln!("Failed to schedule verified update file cleanup: {}", err);
        }
    }

    let mut ui = false;
    // Only a double-click gets a dialog: --silent-install and the other command
    // lines run unattended, where a message box would wait for a click forever.
    // That includes --update, which the service starts in the user's session: when
    // it fails the installed version is left as it was and keeps working, and the
    // next update check tries again, so it fails quietly.
    let interactive = args.is_empty();
    let outcome = match BinaryReader::new() {
        Err(e) => {
            #[cfg(windows)]
            let app = win::own_product_name().unwrap_or_else(current_exe_display_name);
            #[cfg(not(windows))]
            let app = current_exe_display_name();
            Err((app, SetupError::Payload(e)))
        }
        Ok(reader) => {
            // The product name in this file's version resource is what users know
            // the app as ("jerinteDesk"); the executable may keep RustDesk's name.
            #[cfg(windows)]
            let app = win::own_product_name().unwrap_or_else(|| app_display_name(&reader.exe));
            #[cfg(not(windows))]
            let app = app_display_name(&reader.exe);
            setup(
                reader,
                None,
                click_setup || args.contains(&"--silent-install".to_owned()),
                &args,
                &mut ui,
            )
            .map_err(|e| (app, e))
        }
    };
    match outcome {
        Ok(exe) => {
            if click_setup {
                args = vec!["--install".to_owned()];
            } else if quick_support {
                args = vec!["--quick_support".to_owned()];
            }
            execute(exe, args, ui);
            Ok(())
        }
        Err((app, e)) => {
            let message = setup_failure_message(&app, &e);
            eprintln!("{}", message);
            #[cfg(windows)]
            if interactive {
                ui::close();
                win::show_error(&app, &message);
            }
            #[cfg(not(windows))]
            let _ = interactive;
            Err(e.to_string())
        }
    }
}

#[cfg(windows)]
mod win {
    use std::{fs, os::windows::process::CommandExt, path::Path, process::Command};

    // Used for privacy mode(magnifier impl).
    pub const RUNTIME_BROKER_EXE: &'static str = "C:\\Windows\\System32\\RuntimeBroker.exe";
    pub const WIN_TOPMOST_INJECTED_PROCESS_EXE: &'static str = "RuntimeBroker_rustdesk.exe";

    pub(super) fn schedule_verified_update_cleanup() -> std::io::Result<()> {
        use windows::{
            core::{HSTRING, PCWSTR},
            Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT},
        };

        let current_exe = std::env::current_exe()?;
        let is_verified_update = current_exe
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(super::VERIFIED_UPDATE_FILE_PREFIX) && name.ends_with(".exe")
            });
        if !is_verified_update {
            return Ok(());
        }

        unsafe {
            MoveFileExW(
                &HSTRING::from(current_exe.as_path()),
                PCWSTR::null(),
                MOVEFILE_DELAY_UNTIL_REBOOT,
            )
        }
        .map_err(std::io::Error::other)
    }

    pub(super) fn copy_runtime_broker(dir: &Path) {
        let src = RUNTIME_BROKER_EXE;
        let tgt = WIN_TOPMOST_INJECTED_PROCESS_EXE;
        let target_file = dir.join(tgt);
        if target_file.exists() {
            if let (Ok(src_file), Ok(tgt_file)) = (fs::read(src), fs::read(&target_file)) {
                let src_md5 = format!("{:x}", md5::compute(&src_file));
                let tgt_md5 = format!("{:x}", md5::compute(&tgt_file));
                if src_md5 == tgt_md5 {
                    return;
                }
            }
        }
        let _allow_err = Command::new("taskkill")
            .args(&["/F", "/IM", "RuntimeBroker_rustdesk.exe"])
            .creation_flags(winapi::um::winbase::CREATE_NO_WINDOW)
            .output();
        let _allow_err = std::fs::copy(src, &format!("{}\\{}", dir.to_string_lossy(), tgt));
    }

    /// ProductName from this executable's version resource, e.g. "jerinteDesk".
    pub(super) fn own_product_name() -> Option<String> {
        use winapi::um::winver::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
        let path = std::env::current_exe().ok()?;
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let path = wide(&path.to_string_lossy());
        unsafe {
            let size = GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut());
            if size == 0 {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            if GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr() as _) == 0 {
                return None;
            }
            // The first language/code page pair names the string table to read.
            let mut translation: *mut u16 = std::ptr::null_mut();
            let mut len = 0u32;
            let key = wide("\\VarFileInfo\\Translation");
            if VerQueryValueW(data.as_ptr() as _, key.as_ptr(), &mut translation as *mut _ as _, &mut len) == 0
                || len < 4
            {
                return None;
            }
            let (language, code_page) = (*translation, *translation.add(1));
            let key = wide(&format!(
                "\\StringFileInfo\\{:04x}{:04x}\\ProductName",
                language, code_page
            ));
            let mut value: *mut u16 = std::ptr::null_mut();
            if VerQueryValueW(data.as_ptr() as _, key.as_ptr(), &mut value as *mut _ as _, &mut len) == 0
                || len == 0
                || value.is_null()
            {
                return None;
            }
            let name = String::from_utf16_lossy(std::slice::from_raw_parts(value, len as usize));
            let name = name.trim_end_matches('\0').trim().to_owned();
            (!name.is_empty()).then_some(name)
        }
    }

    /// A blocking error dialog, in front of the setup animation.
    pub(super) fn show_error(title: &str, message: &str) {
        use winapi::um::winuser::{
            MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
        };
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (title, message) = (wide(title), wide(message));
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
            );
        }
    }

    /// Check if the executable is a Quick Support version.
    /// Note: This function must be kept in sync with `src/core_main.rs`.
    #[inline]
    pub(super) fn is_quick_support_exe(exe: &str) -> bool {
        let exe = exe.to_lowercase();
        exe.contains("-qs-") || exe.contains("-qs.exe") || exe.contains("_qs.exe")
    }
}

#[cfg(test)]
mod meta_tests {
    use super::*;
    use bin_reader::tests::{packed, scratch_dir};

    fn reader(files: Vec<bin_reader::BinaryData>) -> BinaryReader {
        BinaryReader {
            files,
            exe: "./jerinteDesk.exe".to_owned(),
            package_paths: Vec::new(),
        }
    }

    // A non-empty argument list keeps the Windows setup animation out of the test.
    fn args() -> Vec<String> {
        vec!["--test".to_owned()]
    }

    // What a release build embeds: the packer's build time.
    const PACKER: &[u8] = b"timestamp = 42\n";

    fn trusted_next_time(dir: &Path) -> bool {
        is_timestamp_matches_with(PACKER, dir, &mut 0)
    }

    #[test]
    fn a_complete_extraction_is_recorded_and_launched() {
        let dir = scratch_dir("setup-ok");
        let mut ui = false;
        let exe = setup_with(
            PACKER,
            reader(vec![packed("./jerinteDesk.exe", b"exe"), packed("./librustdesk.dll", b"core")]),
            Some(dir.clone()),
            false,
            &args(),
            &mut ui,
        )
        .unwrap();
        assert_eq!(exe, dir.join("./jerinteDesk.exe"));
        assert!(trusted_next_time(&dir));
    }

    #[test]
    fn a_failed_extraction_is_not_launched_and_not_trusted_next_time() {
        let dir = scratch_dir("setup-fail");
        // A previous complete run of this very packer, so this run does not wipe the
        // directory and gets to the files.
        std::fs::write(dir.join(APP_METADATA_CONFIG), "timestamp = 42\n").unwrap();
        assert!(trusted_next_time(&dir));
        let mut broken = packed("./librustdesk.dll", b"core");
        broken.raw = b"not brotli";
        let mut ui = false;
        let mut reader = reader(vec![packed("./jerinteDesk.exe", b"exe"), broken]);
        reader.package_paths = vec!["./brand.png".to_owned()];
        let err = setup_with(PACKER, reader, Some(dir.clone()), false, &args(), &mut ui)
            .unwrap_err();
        assert!(matches!(err, SetupError::Extract(_)));
        // The next run wipes the directory and starts over...
        assert!(!trusted_next_time(&dir));
        // ...and the package's file records are kept, for the cleanup of files a later
        // package drops (deleting meta.toml would have lost them).
        assert_eq!(previous_package_files(&dir), vec!["./brand.png".to_owned()]);
    }

    #[test]
    fn the_failure_message_names_the_app_the_file_and_what_to_do() {
        let err = SetupError::Extract(bin_reader::ExtractError {
            path: PathBuf::from(r"C:\Users\u\AppData\Local\jerintedesk\librustdesk.dll"),
            error: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        });
        let app = app_display_name("./jerinteDesk.exe");
        assert_eq!(app, "jerinteDesk");
        let message = setup_failure_message(&app, &err);
        assert!(message.contains("jerinteDesk 解压失败"));
        assert!(message.contains("librustdesk.dll"));
        assert!(message.contains("以管理员身份运行"));
        assert!(message.contains("Run as administrator"));
        assert_eq!(app_display_name(""), "RustDesk");
    }

    #[test]
    fn resolve_within_rejects_paths_that_escape() {
        let base = Path::new("/base");
        assert_eq!(
            resolve_within(base, "./data/logo.png"),
            Some(base.join("data").join("logo.png"))
        );
        assert_eq!(
            resolve_within(base, ".\\data\\logo.png"),
            Some(base.join("data").join("logo.png"))
        );
        // meta.toml is user-writable, so these must not reach remove_file.
        assert_eq!(resolve_within(base, "../../etc/passwd"), None);
        assert_eq!(resolve_within(base, "/etc/passwd"), None);
        assert_eq!(resolve_within(base, "C:\\Windows\\System32\\x.dll"), None);
        assert_eq!(resolve_within(base, "."), None);
        assert_eq!(resolve_within(base, ""), None);
    }
}
