//! Checking GitHub for a newer Findest release.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// The releases page, where a new version's notes are.
pub const RELEASES_URL: &str = "https://github.com/ariel-bentu/findest/releases/latest";
const LATEST_API: &str = "https://api.github.com/repos/ariel-bentu/findest/releases/latest";

/// The running version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The latest published version (`0.13.0`), asked of GitHub with the system
/// `curl` (blocking; run it off the main thread).
pub fn latest_version() -> Result<String> {
    let output = Command::new("curl")
        .args(["-fsSL", "--max-time", "15", "-H", "Accept: application/vnd.github+json", LATEST_API])
        .output()
        .context("couldn't run curl")?;
    if !output.status.success() {
        bail!("couldn't reach GitHub ({})", String::from_utf8_lossy(&output.stderr).trim());
    }
    parse_latest(&output.stdout)
}

fn parse_latest(json: &[u8]) -> Result<String> {
    let value: serde_json::Value = serde_json::from_slice(json).context("unexpected reply from GitHub")?;
    let tag = value["tag_name"].as_str().context("no release found")?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// `latest` is a later version than `current` (dotted numbers; anything
/// after a `-`, like `-beta`, is ignored).
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v').split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (mut a, mut b) = (parts(latest), parts(current));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.14.0", "0.13.0"));
        assert!(is_newer("v0.13.1", "0.13.0"));
        assert!(is_newer("1.0", "0.99.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.13.0", "0.13.0"));
        assert!(!is_newer("0.13", "0.13.0"));
        assert!(!is_newer("0.12.5", "0.13.0"));
    }

    #[test]
    fn reads_the_tag() {
        assert_eq!(parse_latest(br#"{"tag_name":"v0.14.0","name":"Findest 0.14.0"}"#).unwrap(), "0.14.0");
        assert!(parse_latest(br#"{"message":"Not Found"}"#).is_err());
    }
}
