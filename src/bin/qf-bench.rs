use quickfind::{
    cache,
    engine::{SearchEngine, SearchMode, SearchOptions, SearchSession, SortField},
    indexer,
    model::{Entry, Index},
    platform,
};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

#[derive(Serialize)]
struct Measurement {
    query: String,
    mode: String,
    match_path: bool,
    matches: usize,
    cold_ms: f64,
    median_ms: f64,
    p95_ms: f64,
}
#[derive(Serialize)]
struct SortMeasurement {
    field: String,
    matches: usize,
    cold_ms: f64,
    warm_ms: f64,
}
#[derive(Serialize)]
struct TypingMeasurement {
    query: String,
    matches: usize,
    full_scan_median_ms: f64,
    session_median_ms: f64,
    scanned_rows: usize,
}
#[derive(Serialize)]
struct Report {
    generated_at: String,
    source: String,
    roots: Vec<String>,
    entries: usize,
    folders: usize,
    skipped_directories: u64,
    scan_ms: u64,
    engine_prepare_ms: f64,
    cache_save_ms: f64,
    cache_load_ms: f64,
    cache_bytes: u64,
    index_allocated_mib: f64,
    working_set_mib: f64,
    peak_working_set_mib: f64,
    queries: Vec<Measurement>,
    sorts: Vec<SortMeasurement>,
    typing: Vec<TypingMeasurement>,
    ntfs_probe: serde_json::Value,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let mut roots = Vec::new();
    let mut output = PathBuf::from("qa/benchmark.json");
    let mut synthetic = None;
    let mut probe = false;
    let mut from_cache = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => roots.push(PathBuf::from(args.next().ok_or("missing --root value")?)),
            "--out" => output = PathBuf::from(args.next().ok_or("missing --out value")?),
            "--synthetic" => synthetic = Some(args.next().ok_or("missing count")?.parse::<usize>().map_err(|e| e.to_string())?),
            "--ntfs" => probe = true,
            "--from-cache" => from_cache = Some(PathBuf::from(args.next().ok_or("missing cache path")?)),
            _ => return Err("Usage: qf-bench [--root C:\\] [--out qa/benchmark.json] [--synthetic 1000000] [--ntfs]".into()),
        }
    }
    if roots.is_empty() {
        roots = indexer::default_roots();
    }
    let ntfs_probe = if probe {
        match indexer::probe_ntfs(&roots[0]) {
            Ok(p) => serde_json::to_value(p).map_err(|e| e.to_string())?,
            Err(e) => serde_json::json!({"available":false,"reason":e}),
        }
    } else {
        serde_json::Value::Null
    };
    let mut index = if let Some(path) = &from_cache {
        eprintln!("Using the existing filesystem snapshot (no new disk scan).");
        cache::load(path, &roots)?
    } else if let Some(count) = synthetic {
        let start = Instant::now();
        let folders = (0..2000)
            .map(|n| format!("C:\\benchmark\\folder_{n:04}"))
            .collect();
        let entries = (0..count)
            .map(|n| Entry {
                name: format!(
                    "{}_{n:08}.{}",
                    if n % 7 == 0 {
                        "보고서"
                    } else if n % 3 == 0 {
                        "test"
                    } else {
                        "document"
                    },
                    ["txt", "pdf", "xlsx", "rs", "docx"][n % 5]
                ),
                parent: (n % 2000) as u32,
                size: Some((n * 137 % 10_000_000) as u64),
                created: Some(1_700_000_000 + (n % 100_000) as i64),
                modified: Some(1_710_000_000 + (n % 200_000) as i64),
                is_dir: false,
            })
            .collect();
        Index {
            entries,
            folders,
            roots: vec!["C:\\benchmark".into()],
            scan_ms: start.elapsed().as_millis() as u64,
            ..Default::default()
        }
    } else {
        eprintln!("Indexing file names and metadata...");
        let last = std::sync::atomic::AtomicU64::new(0);
        // Progress reports contain counts only, never filenames.
        indexer::scan(&roots, &AtomicBool::new(false), |p| {
            let bucket = p.elapsed_ms / 5000;
            if bucket > last.load(std::sync::atomic::Ordering::Relaxed) {
                last.store(bucket, std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "{} entries / {} directories / {} ms",
                    p.entries, p.folders, p.elapsed_ms
                );
            }
        })?
    };
    index.compact();
    let index_allocated_mib = index.allocated_bytes() as f64 / 1048576.0;
    let entries = index.entries.len();
    let folders = index.folders.len();
    let skipped_directories = index.skipped;
    let scan_ms = index.scan_ms;
    eprintln!("Collected {entries} entries in {scan_ms} ms. Measuring search...");
    let cache_dir = output
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("private-benchmark");
    std::fs::create_dir_all(&cache_dir).map_err(|e| e.to_string())?;
    let cache_path = cache_dir.join("index.qfi");
    let start = Instant::now();
    cache::save(&cache_path, &index)?;
    let cache_save_ms = start.elapsed().as_secs_f64() * 1000.0;
    let cache_bytes = std::fs::metadata(&cache_path)
        .map_err(|e| e.to_string())?
        .len();
    let start = Instant::now();
    let cache_roots: Vec<PathBuf> = index.roots.iter().map(PathBuf::from).collect();
    let restored = cache::load(&cache_path, &cache_roots)?;
    let cache_load_ms = start.elapsed().as_secs_f64() * 1000.0;
    if restored.entries.len() != entries {
        return Err("Cache round trip changed entry count".into());
    }
    drop(restored);
    let start = Instant::now();
    let engine = Arc::new(SearchEngine::new(Arc::new(index)));
    let engine_prepare_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut queries = Vec::new();
    for (q, mode, match_path) in [
        ("report", SearchMode::Plain, false),
        ("보고서", SearchMode::Plain, false),
        ("*.pdf", SearchMode::Wildcard, false),
        ("*test*", SearchMode::Wildcard, false),
        (r"\.(pdf|xlsx|docx)$", SearchMode::Regex, false),
        (r"\windows\system32\", SearchMode::Plain, true),
        (r"*\system32\*.dll", SearchMode::Wildcard, true),
    ] {
        let options = SearchOptions {
            mode,
            match_path,
            ..Default::default()
        };
        let cold = engine.search(q, &options)?;
        let mut samples = Vec::new();
        for _ in 0..9 {
            samples.push(engine.search(q, &options)?.elapsed_ms);
        }
        samples.sort_by(f64::total_cmp);
        queries.push(Measurement {
            query: q.into(),
            mode: format!("{mode:?}"),
            match_path,
            matches: cold.rows.len(),
            cold_ms: cold.elapsed_ms,
            median_ms: samples[4],
            p95_ms: samples[8],
        });
    }
    let mut sorts = Vec::new();
    for sort in [
        SortField::Name,
        SortField::Path,
        SortField::Size,
        SortField::Created,
        SortField::Modified,
    ] {
        let options = SearchOptions {
            sort,
            ..Default::default()
        };
        let cold = engine.search("", &options)?;
        let warm = engine.search("", &options)?;
        sorts.push(SortMeasurement {
            field: format!("{sort:?}"),
            matches: warm.rows.len(),
            cold_ms: cold.elapsed_ms,
            warm_ms: warm.elapsed_ms,
        });
    }
    let steps = ["보", "보고", "보고서", "r", "re", "rep", "repo", "report"];
    let mut full_samples: Vec<Vec<f64>> = vec![Vec::new(); steps.len()];
    let mut session_samples: Vec<Vec<f64>> = vec![Vec::new(); steps.len()];
    let mut counts = vec![0; steps.len()];
    let mut scanned = vec![0; steps.len()];
    for _ in 0..9 {
        let mut session = SearchSession::default();
        for (i, query) in steps.iter().enumerate() {
            let full = engine.search(query, &SearchOptions::default())?;
            let cached = session.search(&engine, query, &SearchOptions::default())?;
            if full.rows != cached.rows {
                return Err(format!("Candidate search changed results: {query}"));
            }
            full_samples[i].push(full.elapsed_ms);
            session_samples[i].push(cached.elapsed_ms);
            counts[i] = cached.rows.len();
            scanned[i] = cached.scanned_rows;
        }
    }
    let typing = steps
        .iter()
        .enumerate()
        .map(|(i, query)| {
            full_samples[i].sort_by(f64::total_cmp);
            session_samples[i].sort_by(f64::total_cmp);
            TypingMeasurement {
                query: query.to_string(),
                matches: counts[i],
                full_scan_median_ms: full_samples[i][4],
                session_median_ms: session_samples[i][4],
                scanned_rows: scanned[i],
            }
        })
        .collect();
    let (working, peak) = platform::memory_usage();
    let report = Report {
        generated_at: chrono::Local::now().to_rfc3339(),
        source: if from_cache.is_some() {
            "local_filesystem_cache"
        } else if synthetic.is_some() {
            "synthetic"
        } else {
            "local_filesystem"
        }
        .into(),
        roots: engine.index().roots.clone(),
        entries,
        folders,
        skipped_directories,
        scan_ms,
        engine_prepare_ms,
        cache_save_ms,
        cache_load_ms,
        cache_bytes,
        index_allocated_mib,
        working_set_mib: working as f64 / 1048576.0,
        peak_working_set_mib: peak as f64 / 1048576.0,
        queries,
        sorts,
        typing,
        ntfs_probe,
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(&output, &json).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}
