use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::Opener;

/// Substitutes `{path}` and `{dir}` in each argument of `template`.
pub fn expand_command(template: &[String], target: &Path) -> Vec<String> {
    let path = target.display().to_string();
    let dir = if target.is_dir() { target } else { target.parent().unwrap_or(target) };
    let dir = dir.display().to_string();
    template.iter().map(|arg| arg.replace("{path}", &path).replace("{dir}", &dir)).collect()
}

/// Runs the opener's command for `target` without blocking.
pub fn run_opener(opener: &Opener, target: &Path) -> Result<()> {
    spawn_detached(&expand_command(&opener.command, target))
        .with_context(|| format!("running \"{}\"", opener.name))
}

/// Opens `path` with its default application, like double-clicking in a file manager.
pub fn open_default(path: &Path) -> Result<()> {
    spawn_detached(&[opener().to_string(), path.display().to_string()])
}

/// Opens a URL with its handler.
pub fn open_url(url: &str) -> Result<()> {
    spawn_detached(&[opener().to_string(), url.to_string()])
}

pub fn reveal_in_finder(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        spawn_detached(&["open".to_string(), "-R".to_string(), path.display().to_string()])
    }
    #[cfg(not(target_os = "macos"))]
    {
        let owned = path.to_path_buf();
        let uri = crate::paths::file_url(&owned);
        let fallback_dir = if owned.is_dir() { owned.clone() } else { owned.parent().unwrap_or(&owned).to_path_buf() };
        std::thread::spawn(move || {
            let ok = Command::new("dbus-send")
                .args([
                    "--session",
                    "--print-reply",
                    "--dest=org.freedesktop.FileManager1",
                    "--type=method_call",
                    "/org/freedesktop/FileManager1",
                    "org.freedesktop.FileManager1.ShowItems",
                    &format!("array:string:{uri}"),
                    "string:",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                let _ = Command::new(opener())
                    .arg(fallback_dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        });
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn opener() -> &'static str {
    "open"
}

#[cfg(not(target_os = "macos"))]
fn opener() -> &'static str {
    "xdg-open"
}

fn spawn_detached(argv: &[String]) -> Result<()> {
    let Some((program, args)) = argv.split_first() else { bail!("empty command") };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    // Reap the child in the background so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a b.txt");
        std::fs::write(&file, "").unwrap();
        let tpl: Vec<String> = ["code", "--goto", "{path}", "--cwd={dir}"].map(String::from).into();

        let out = expand_command(&tpl, &file);
        assert_eq!(out[2], file.display().to_string());
        assert_eq!(out[3], format!("--cwd={}", tmp.path().display()));

        // For a folder, {dir} is the folder itself.
        let out = expand_command(&tpl, tmp.path());
        assert_eq!(out[3], format!("--cwd={}", tmp.path().display()));
    }

    #[test]
    fn default_opener_binary() {
        #[cfg(target_os = "macos")]
        assert_eq!(opener(), "open");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(opener(), "xdg-open");
    }
}
