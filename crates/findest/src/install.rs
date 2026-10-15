//! Self-install. Run outside an app bundle (e.g. after `cargo install`), a
//! release build of `findest` installs or updates Findest.app, then hands off
//! to it. Running as a real app matters on macOS: privacy permissions (cloud
//! folders, Full Disk Access) belong to the launching app, so a terminal-run
//! binary would otherwise get the terminal's permissions.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

pub const BUNDLE_ID: &str = "dev.findest.Findest";
const ICON: &[u8] = include_bytes!("../../../assets/AppIcon.icns");
/// Compiled asset catalog; macOS 26+ draws the icon full size only from this.
const ASSETS: &[u8] = include_bytes!("../../../assets/Assets.car");

/// Returns true if this process handed off to Findest.app and should exit.
pub fn handoff_if_needed() -> bool {
    if cfg!(debug_assertions) || running_in_bundle() || std::env::var_os("FINDEST_NO_BUNDLE").is_some() {
        return false;
    }
    match install_and_open() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("findest: couldn't install Findest.app ({e:#}); running in place");
            false
        }
    }
}

fn running_in_bundle() -> bool {
    std::env::current_exe().is_ok_and(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
}

fn install_and_open() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let app = bundle_location()?;
    let stamp = stamp_for(&exe)?;
    let installed = fs::read_to_string(app.join("Contents/Resources/install-stamp")).ok();
    if installed.as_deref() != Some(stamp.as_str()) {
        quit_running_app();
        write_bundle(&exe, &app, &stamp)?;
        eprintln!("findest: installed {}", app.display());
    }

    let mut open = Command::new("open");
    open.arg("-a").arg(&app);
    for arg in std::env::args_os().skip(1) {
        open.arg(std::path::absolute(PathBuf::from(arg))?);
    }
    if !open.status().context("running open")?.success() {
        bail!("open failed");
    }
    Ok(())
}

/// `/Applications/Findest.app` when writable, else `~/Applications/Findest.app`.
fn bundle_location() -> Result<PathBuf> {
    let system = Path::new("/Applications");
    let probe = system.join(".findest-write-test");
    if fs::write(&probe, b"").is_ok() {
        let _ = fs::remove_file(&probe);
        return Ok(system.join("Findest.app"));
    }
    let user = findest_core::paths::home_dir().join("Applications");
    fs::create_dir_all(&user)?;
    Ok(user.join("Findest.app"))
}

/// Identifies the binary being installed, so unchanged installs are skipped.
fn stamp_for(exe: &Path) -> Result<String> {
    let meta = fs::metadata(exe)?;
    let modified = meta.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    Ok(format!("{} {} {modified}", exe.display(), meta.len()))
}

fn quit_running_app() {
    let script = format!("if application id \"{BUNDLE_ID}\" is running then tell application id \"{BUNDLE_ID}\" to quit");
    let _ = Command::new("osascript").args(["-e", &script]).output();
    // Give it a moment to exit before its bundle is replaced.
    for _ in 0..30 {
        let running = Command::new("pgrep").args(["-f", "Findest.app/Contents/MacOS/findest"]).output();
        if running.is_ok_and(|o| o.stdout.is_empty()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn write_bundle(exe: &Path, app: &Path, stamp: &str) -> Result<()> {
    // Build next to the destination, then swap it in.
    let tmp = app.with_extension(format!("app.tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let contents = tmp.join("Contents");
    fs::create_dir_all(contents.join("MacOS"))?;
    fs::create_dir_all(contents.join("Resources"))?;
    fs::copy(exe, contents.join("MacOS/findest")).context("copying binary")?;
    fs::write(contents.join("Resources/AppIcon.icns"), ICON)?;
    fs::write(contents.join("Resources/Assets.car"), ASSETS)?;
    fs::write(contents.join("Resources/install-stamp"), stamp)?;
    fs::write(contents.join("Info.plist"), info_plist())?;
    sign(&tmp, &current_signer(app))?;

    if app.exists() {
        fs::remove_dir_all(app).with_context(|| format!("removing old {}", app.display()))?;
    }
    fs::rename(&tmp, app).with_context(|| format!("installing {}", app.display()))?;
    Ok(())
}

/// Signs with `$FINDEST_SIGN_IDENTITY`, else an "Apple Development" certificate,
/// else ad-hoc. macOS ties privacy permissions to the signing identity, so the
/// choice must be stable: keep the installed app's signer if it's still
/// available, otherwise take the first certificate alphabetically (keychain
/// listing order isn't stable).
fn sign(app: &Path, previous_signer: &Option<String>) -> Result<()> {
    let identity = std::env::var("FINDEST_SIGN_IDENTITY")
        .ok()
        .or_else(|| {
            let mut ids = development_identities();
            ids.sort();
            previous_signer.clone().filter(|p| ids.contains(p)).or_else(|| ids.into_iter().next())
        })
        .unwrap_or_else(|| "-".into());
    let status = Command::new("codesign")
        .args(["--force", "--deep", "--sign", &identity])
        .arg(app)
        .output()
        .context("running codesign")?;
    if !status.status.success() {
        bail!("codesign failed: {}", String::from_utf8_lossy(&status.stderr).trim());
    }
    if identity == "-" {
        eprintln!("findest: signed ad-hoc; privacy permissions will reset on each update");
    }
    Ok(())
}

fn development_identities() -> Vec<String> {
    let Ok(out) = Command::new("security").args(["find-identity", "-v", "-p", "codesigning"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let start = line.find("\"Apple Development: ")? + 1;
            let end = line[start..].find('"')? + start;
            Some(line[start..end].to_string())
        })
        .collect()
}

/// The certificate that signed the currently installed app, if any.
fn current_signer(app: &Path) -> Option<String> {
    let out = Command::new("codesign").args(["-dv", "--verbose=2"]).arg(app).output().ok()?;
    // codesign prints details on stderr; the first Authority line is the leaf certificate.
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find_map(|l| l.strip_prefix("Authority=").map(str::to_string))
}

fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Findest</string>
    <key>CFBundleDisplayName</key><string>Findest</string>
    <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
    <key>CFBundleExecutable</key><string>findest</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleIconName</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>Folder</string>
            <key>CFBundleTypeRole</key><string>Viewer</string>
            <!-- Offered in "Open With", never made the default. -->
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array><string>public.folder</string></array>
        </dict>
    </array>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_valid() {
        let tmp = std::env::temp_dir().join(format!("findest-plist-{}.plist", std::process::id()));
        fs::write(&tmp, info_plist()).unwrap();
        let lint = Command::new("plutil").arg("-lint").arg(&tmp).output().unwrap();
        let _ = fs::remove_file(&tmp);
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
    }
}
