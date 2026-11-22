//! Tells the user when a newer Findest is out: checked at launch and then
//! daily (unless `check_for_updates = false`), shown in every window's
//! status bar until updated.

use std::time::Duration;

use findest_core::config::Config;
use findest_core::update;
use gpui::{App, Global};

const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The newer version found, if any.
#[derive(Default)]
pub struct UpdateCheck {
    pub available: Option<String>,
}
impl Global for UpdateCheck {}

pub fn init(cx: &mut App) {
    cx.set_global(UpdateCheck::default());
    cx.spawn(async move |cx| {
        loop {
            let enabled = Config::load(&Config::path()).map(|c| c.general.check_for_updates).unwrap_or(true);
            if enabled {
                let latest = cx.background_executor().spawn(async { update::latest_version() }).await;
                if let Ok(latest) = latest
                    && update::is_newer(&latest, update::CURRENT)
                {
                    cx.update(|cx| cx.set_global(UpdateCheck { available: Some(latest) })).ok();
                }
            }
            cx.background_executor().timer(INTERVAL).await;
        }
    })
    .detach();
}

/// How to get the new version, for the status bar's tooltip.
pub fn how_to_update() -> &'static str {
    let from_brew = std::env::current_exe().is_ok_and(|p| p.components().any(|c| c.as_os_str() == "Cellar"));
    if cfg!(target_os = "macos") || from_brew {
        "To update: brew upgrade findest, then run findest (or git pull and cargo install --path crates/findest)"
    } else {
        "To update: git pull, then cargo install --path crates/findest"
    }
}
