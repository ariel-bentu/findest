use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Instant;
use findest_core::search::*;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [mode, root, pattern] = args.as_slice()
        && mode == "live"
    {
        return live(root.into(), pattern);
    }
    let home = findest_core::paths::home_dir();
    let roots = vec![home.clone()];
    let ex = Excludes::new(&["**/node_modules".into(), "**/.git".into()]);
    let t = Instant::now();
    let index = Index::build(&roots, &ex, &AtomicUsize::new(0), &AtomicBool::new(false));
    println!("built {} entries in {:?}", index.len(), t.elapsed());
    let file = std::env::temp_dir().join("findest-bench-index.bin");
    let t = Instant::now(); index.save(&file).unwrap(); println!("saved in {:?} ({} MB)", t.elapsed(), std::fs::metadata(&file).unwrap().len() / 1_000_000);
    let t = Instant::now(); let index = Index::load(&file).unwrap(); println!("loaded in {:?}", t.elapsed());
    for q in ["main", "*.rs", "ext:pdf size:>1mb", "regex:^IMG_\\d+", "src/main", "dm:week file:", "ext:md content:TODO"] {
        let query = parse(q).unwrap();
        let t = Instant::now();
        let r = search(&index, &query, &SearchOptions::default(), &AtomicUsize::new(0), &AtomicBool::new(false)).unwrap();
        println!("{q:28} {:>8} results in {:?}", r.ids.len(), t.elapsed());
    }
}

/// Live search (no index) over a folder, next to `fd` when it's installed:
/// `cargo run --release -p findest-core --example search_bench -- live ~/dev main.rs`
fn live(root: std::path::PathBuf, pattern: &str) {
    let query = parse(pattern).unwrap();
    let ex = Excludes::new(&[]);
    let opts = SearchOptions { include_hidden: true, ..Default::default() };
    for _ in 0..2 {
        let progress = LiveProgress::default();
        let t = Instant::now();
        let found = live_search(std::slice::from_ref(&root), &query, &opts, &ex, &progress, &AtomicBool::new(false)).unwrap();
        let checked = progress.checked.load(std::sync::atomic::Ordering::Relaxed);
        println!("findest live: {} matches of {checked} items in {:?}", found.len(), t.elapsed());
        let t = Instant::now();
        if let Ok(out) = std::process::Command::new("fd").args(["-H", "-I", "-c", "never", pattern]).arg(&root).output() {
            println!("fd:           {} matches in {:?}", out.stdout.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count(), t.elapsed());
        }
    }
}
