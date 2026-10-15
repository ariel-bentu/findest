use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Instant;
use findest_core::search::*;

fn main() {
    let home = findest_core::paths::home_dir();
    let roots = vec![home.clone(), "/Applications".into()];
    let ex = Excludes::new(&["~/Library".into(), "~/.Trash".into(), "**/node_modules".into(), "**/.git".into()]);
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
