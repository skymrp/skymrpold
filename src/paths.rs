//! Paths for host files used by skymrp: settings, fonts, etc.
//!
//! There are three categories of files:
//!
//! * Resources bundled with skymrp that neither skymrp nor the user should
//!   modify: [DYLIBS_DIR], [FONTS_DIR], [DEFAULT_OPTIONS_FILE]. Depending on
//!   the platform these may or may not be ordinary files, and must be accessed
//!   through [ResourceFile].
//! * Files the user is expected to modify, but not skymrp: [APPS_DIR],
//!   [USER_OPTIONS_FILE], [WALLPAPER_FILES]. These are ordinary files and are
//!   found in [user_data_base_path].
//! * Files that skymrp will create and modify, and the user may modify if
//!   they want to: [SANDBOX_DIR]. These are ordinary files and are found in
//!   [user_data_base_path].
//!
//! See also [crate::fs], which provides a virtual filesystem for the guest app
//! and defines path types.

use std::borrow::Cow;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

pub const MYTHROAD_DIR_NAME: &str = "mythroad";
pub const MYTHROAD_DIR_ENV: &str = "SKYMRP_MYTHROAD_DIR";
pub const REQUIRED_SYSTEM_FILES: &[&str] = &[
    "cfunction.ext",
    "dsm_gm.mrp",
    "system/gb16.uc2",
    "system/gb12.uc2",
];

/// Name of the directory containing ARMv6 dynamic libraries bundled with
/// skymrp.
pub const DYLIBS_DIR: &str = "skymrp_dylibs";

/// Name of the directory containing fonts bundled with skymrp.
pub const FONTS_DIR: &str = "skymrp_fonts";

/// Name of the file containing skymrp's default options for various apps.
pub const DEFAULT_OPTIONS_FILE: &str = "skymrp_default_options.txt";

/// macOS-only: If skymrp is located in a .app bundle, return the path of the
/// Resources directory. If skymrp is not located in a .app bundle, return
/// [None].
#[allow(dead_code)]
fn get_macos_bundled_resources_path() -> Option<PathBuf> {
    if std::env::consts::OS != "macos" {
        return None;
    }
    let base_path = PathBuf::from(sdl2::filesystem::base_path().ok()?);
    if base_path.file_name().is_some_and(|p| p == "Resources") {
        Some(base_path)
    } else {
        None
    }
}

/// Abstraction over a platform-specific type for accessing a resource bundled
/// with skymrp.
pub struct ResourceFile {
    #[cfg(target_os = "android")]
    file: sdl2::rwops::RWops<'static>,
    #[cfg(not(target_os = "android"))]
    file: std::fs::File,
}
impl ResourceFile {
    pub fn open(path: &str) -> Result<Self, String> {
        Ok(Self {
            // On Android, these resources are included as "assets" within the
            // APK. We access them via SDL2's wrapper of Android's assets API.
            #[cfg(target_os = "android")]
            file: sdl2::rwops::RWops::from_file(path, "r")?,

            // On other OSes, resources are accessed as ordinary files.
            #[cfg(not(target_os = "android"))]
            file: {
                let base_path = get_macos_bundled_resources_path();
                // When not in a bundle, look in the current directory.
                let path = base_path.as_deref().unwrap_or(Path::new(".")).join(path);
                std::fs::File::open(path).map_err(|e| e.to_string())?
            },
        })
    }
    pub fn get(&mut self) -> &mut (impl Read + Seek) {
        &mut self.file
    }
}
impl std::fmt::Debug for ResourceFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "ResourceFile")
    }
}

/// Whether various resources are in user-accessible files. If they aren't,
/// skymrp has to be able to display their license terms.
pub const RESOURCES_ARE_EXTERNAL_FILES: bool = cfg!(not(target_os = "android"));

/// Name of the directory where the user can put apps if they want them to
/// appear in the app picker.
pub const APPS_DIR: &str = "skymrp_apps";

/// Name of the file intended for the user's own options.
pub const USER_OPTIONS_FILE: &str = "skymrp_options.txt";

/// Names of files the user can put a wallpaper image (for the app picker) in.
#[allow(unused)]
pub const WALLPAPER_FILES: &[&str] = &[
    "skymrp_wallpaper.png",
    "skymrp_wallpaper.jpg",
    "skymrp_wallpaper.jpeg",
];

/// Name of the directory where skymrp will store sandboxed app data, e.g.
/// the `Documents` directory.
pub const SANDBOX_DIR: &str = "skymrp_sandbox";

/// Get a platform-specific base path needed for accessing skymrp's
/// user-modifiable files. This is empty on platforms other than Android.
pub fn user_data_base_path() -> Cow<'static, Path> {
    #[cfg(target_os = "android")]
    unsafe {
        // This is an exception to the rule that SDL2 should only be used
        // directly from src/window.rs. This is just too distant from windowing
        // to belong there.

        // Android storage has evolved in a quite messy fashion. Both "internal
        // storage" and "external storage" (aka the "SD card") are likely to be
        // internal on a modern device, as absurd as that might sound. SDL2 has
        // APIs to get paths for both. We use the "external storage" because
        // it's more likely to be user-accessible.
        extern "C" {
            fn SDL_AndroidGetExternalStoragePath() -> *const std::ffi::c_char;
        }
        let path = SDL_AndroidGetExternalStoragePath();
        if path.is_null() {
            log!("Couldn't get Android external storage path!");
            panic!();
        }
        Cow::from(Path::new(std::ffi::CStr::from_ptr(path).to_str().unwrap()))
    }
    #[cfg(not(target_os = "android"))]
    {
        // When skymrp is run from a .app bundle on macOS, the user might not
        // be able to control the current directory, so user data needs to go in
        // a standard location.
        if get_macos_bundled_resources_path().is_some() {
            return Cow::from(PathBuf::from(
                sdl2::filesystem::pref_path("skymrp.org", "skymrp").unwrap(),
            ));
        }
        Cow::from(Path::new("."))
    }
}

fn platform_data_dir() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        return user_data_base_path().into_owned();
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("skymrp");
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("skymrp");
        }
    }

    #[cfg(all(unix, not(target_os = "android"), not(target_os = "macos")))]
    {
        if let Some(xdg_data_home) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(xdg_data_home).join("skymrp");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("skymrp");
        }
    }

    user_data_base_path().into_owned()
}

pub fn mythroad_dir() -> PathBuf {
    if let Some(path) = std::env::var_os(MYTHROAD_DIR_ENV) {
        return PathBuf::from(path);
    }
    platform_data_dir().join(MYTHROAD_DIR_NAME)
}

pub fn ensure_mythroad_dir() -> Result<PathBuf, String> {
    let dir = mythroad_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Failed to create {}: {err}", dir.display()))?;
    Ok(dir)
}

pub fn missing_required_system_files() -> Vec<PathBuf> {
    let dir = mythroad_dir();
    REQUIRED_SYSTEM_FILES
        .iter()
        .filter_map(|path| {
            let path = Path::new(path);
            let full_path = dir.join(path);
            (!full_path.is_file()).then(|| path.to_path_buf())
        })
        .collect()
}

pub fn resolve_mythroad_path(path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_path_buf();
    }

    let mut components = path.components();
    if components
        .next()
        .is_some_and(|component| component.as_os_str() == MYTHROAD_DIR_NAME)
    {
        return mythroad_dir().join(components.as_path());
    }

    mythroad_dir().join(path)
}

/// Get a URI that can be used to open a file manager or similar for the path
/// that [user_data_base_path] represents.
pub fn url_for_opening_user_data_dir() -> Result<String, String> {
    if std::env::consts::OS == "android" {
        // See DocumentsProvider.kt, app/build.gradle and AndroidManifest.xml
        let brand = crate::branding();
        Ok(format!(
            "content://org.skymrp.android{}{}.provider/root/root",
            if brand.is_empty() { "" } else { "." },
            brand.to_lowercase()
        ))
    } else {
        let path = user_data_base_path()
            .join(".")
            .canonicalize()
            .map_err(|e| format!("Can't canonicalize path to user data directory: {e}"))?;
        let path = path
            .to_str()
            .ok_or_else(|| "User data directory path is not UTF-8".to_string())?;
        // std::fs::canonicalize() on Windows uses the extended-length path
        // syntax, but Windows Explorer doesn't understand it.
        let path = if std::env::consts::OS == "windows" {
            path.strip_prefix("\\\\?\\").unwrap_or(path)
        } else {
            path
        };
        Ok(format!("file://{path}"))
    }
}

/// Only meaningful on certain OSes: create the user data directory if it
/// doesn't exist, and populate it with templates or README files. (On other
/// platforms these are simply bundled with skymrp in a ZIP file.)
pub fn prepopulate_user_data_dir() {
    const MYTHROAD_README: &str = "\
Put required system files and apps in this directory.

Required system files:
- cfunction.ext
- dsm_gm.mrp
- system/gb16.uc2
- system/gb12.uc2

You can also set SKYMRP_MYTHROAD_DIR to point skymrp at another mythroad directory.
";

    if std::env::consts::OS != "android" && std::env::consts::OS != "macos" {
        return;
    }
    let base_path = user_data_base_path();
    if base_path == Path::new(".") {
        return;
    }

    let apps_dir = base_path.join(APPS_DIR);
    if !apps_dir.is_dir() {
        match std::fs::create_dir(&apps_dir) {
            Ok(()) => {
                log!("Created: {}", apps_dir.display());
            }
            Err(e) => {
                log!("Warning: Couldn't create {}: {}", apps_dir.display(), e);
            }
        }
    }

    fn create_file(path: &Path, content: &str) {
        match std::fs::write(path, content) {
            Ok(()) => {
                log!("Created: {}", path.display());
            }
            Err(e) => {
                log!("Warning: Couldn't create {}: {}", path.display(), e);
            }
        }
    }

    let apps_dir_readme = apps_dir.join("README.txt");
    if !apps_dir_readme.is_file() {
        create_file(&apps_dir_readme, MYTHROAD_README);
    }

    let user_options = base_path.join(USER_OPTIONS_FILE);
    if !user_options.is_file() {
        let content = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/skymrp_options.txt"));
        create_file(&user_options, content);
    }
}
