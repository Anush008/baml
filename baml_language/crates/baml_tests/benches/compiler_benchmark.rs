//! Compiler pipeline profiler / benchmark (compiler2).
//!
//! Measures where time goes when compiling BAML source all the way through
//! bytecode generation, using the real `ProjectDatabase` salsa pipeline.
//!
//! Unlike a microbenchmark, this is a *profiler*: it attributes cold-compile
//! wall-clock to pipeline stages (parse+HIR / typecheck-TIR / codegen-MIR+emit),
//! isolates the fixed stdlib/builtin overhead, counts per-query salsa executions
//! (the key signal for "what runs too often"), and measures incremental
//! (editor-latency) recompiles after a single-file edit.
//!
//! Run:
//!   cargo bench -p baml_tests --bench compiler_benchmark
//!   BAML_CORPUS=crates/baml_tests/projects/parser_stress BAML_RUNS=5 \
//!       cargo bench -p baml_tests --bench compiler_benchmark
//!
//! Env:
//!   BAML_CORPUS  directory of .baml files to compile (default: crates/baml_tests/baml_src)
//!   BAML_RUNS    number of cold runs to take the median of (default: 7)
//!   BAML_OUT     optional path to append a machine-readable summary line

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use baml_project::{ProjectDatabase, collect_compiler2_diagnostics};

// ----------------------------------------------------------------------------
// Corpus loading
// ----------------------------------------------------------------------------

struct Corpus {
    root: PathBuf,
    files: Vec<(PathBuf, String)>,
    total_bytes: usize,
    total_lines: usize,
}

fn collect_baml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_baml_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("baml") {
            out.push(path);
        }
    }
}

fn load_corpus(root: &Path) -> Corpus {
    let mut paths = Vec::new();
    collect_baml_files(root, &mut paths);
    paths.sort();
    let mut files = Vec::new();
    let mut total_bytes = 0;
    let mut total_lines = 0;
    for p in paths {
        if let Ok(content) = std::fs::read_to_string(&p) {
            total_bytes += content.len();
            total_lines += content.lines().count();
            files.push((p, content));
        }
    }
    Corpus {
        root: root.to_path_buf(),
        files,
        total_bytes,
        total_lines,
    }
}

// ----------------------------------------------------------------------------
// DB construction
// ----------------------------------------------------------------------------

fn fresh_db(corpus: &Corpus) -> ProjectDatabase {
    let mut db = ProjectDatabase::new();
    db.set_project_root(&corpus.root);
    for (path, content) in &corpus.files {
        db.add_file(path, content);
    }
    db
}

/// Build a db with an event callback that records the full database key of every
/// query salsa actually executes (WillExecute). Returns (db, recorded handle).
/// Capturing the full key (not just the ingredient) lets us distinguish
/// legitimate per-key execution from fixpoint-cycle RE-execution of the same key.
fn fresh_db_counting(
    corpus: &Corpus,
) -> (ProjectDatabase, Arc<Mutex<Vec<salsa::DatabaseKeyIndex>>>) {
    let raw: Arc<Mutex<Vec<salsa::DatabaseKeyIndex>>> = Arc::new(Mutex::new(Vec::new()));
    let raw2 = raw.clone();
    let mut db = ProjectDatabase::new_with_event_callback(Box::new(move |e| {
        if let salsa::EventKind::WillExecute { database_key } = &e.kind {
            raw2.lock().unwrap().push(*database_key);
        }
    }));
    db.set_project_root(&corpus.root);
    for (path, content) in &corpus.files {
        db.add_file(path, content);
    }
    (db, raw)
}

struct QueryStat {
    execs: u64,
    distinct_keys: usize,
}

/// Group executions by query ingredient name, counting both total executions
/// and the number of distinct keys (so execs/distinct = re-execution factor).
fn resolve_counts(db: &ProjectDatabase, raw: &[salsa::DatabaseKeyIndex]) -> BTreeMap<String, QueryStat> {
    use std::collections::HashSet;
    let mut by_name: BTreeMap<String, (u64, HashSet<salsa::DatabaseKeyIndex>)> = BTreeMap::new();
    for &key in raw {
        let name = (db as &dyn salsa::Database).ingredient_debug_name(key.ingredient_index());
        let e = by_name.entry(name.to_string()).or_default();
        e.0 += 1;
        e.1.insert(key);
    }
    by_name
        .into_iter()
        .map(|(k, (execs, keys))| {
            (
                k,
                QueryStat {
                    execs,
                    distinct_keys: keys.len(),
                },
            )
        })
        .collect()
}

// ----------------------------------------------------------------------------
// Stage drivers (each forces a chunk of the pipeline)
// ----------------------------------------------------------------------------

/// Force lex+parse+HIR (item trees + semantic index) for every file.
fn run_parse_hir(db: &ProjectDatabase) {
    let all = baml_compiler2_hir::compiler2_all_files(db);
    for f in &all {
        let _ = baml_compiler2_hir::file_item_tree(db, *f);
        let _ = baml_compiler2_hir::file_semantic_index(db, *f);
    }
}

/// Force full typecheck (parse+HIR+TIR diagnostics).
fn run_typecheck(db: &ProjectDatabase) -> usize {
    collect_compiler2_diagnostics(db).len()
}

/// Force codegen (MIR + emit -> bytecode).
fn run_codegen(db: &ProjectDatabase) -> usize {
    db.get_bytecode().map(|p| p.globals.len()).unwrap_or(0)
}

// ----------------------------------------------------------------------------
// Timing helpers
// ----------------------------------------------------------------------------

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

struct StageRow {
    name: &'static str,
    median: Duration,
    min: Duration,
}

fn time_fresh<F: Fn(&ProjectDatabase)>(corpus: &Corpus, runs: usize, f: F) -> (Duration, Duration) {
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let db = fresh_db(corpus);
        let t = Instant::now();
        f(&db);
        samples.push(t.elapsed());
    }
    (median(samples.clone()), *samples.iter().min().unwrap())
}

// ----------------------------------------------------------------------------
// Main
// ----------------------------------------------------------------------------

fn main() {
    let corpus_dir = std::env::var("BAML_CORPUS")
        .unwrap_or_else(|_| "crates/baml_tests/baml_src".to_string());
    let runs: usize = std::env::var("BAML_RUNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);

    let root = PathBuf::from(&corpus_dir)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&corpus_dir));
    let corpus = load_corpus(&root);

    println!("\n========================================================================");
    println!("BAML compiler2 pipeline profile");
    println!("========================================================================");
    println!("corpus:        {}", root.display());
    println!(
        "files:         {}   lines: {}   bytes: {} ({:.1} KB)",
        corpus.files.len(),
        corpus.total_lines,
        corpus.total_bytes,
        corpus.total_bytes as f64 / 1024.0
    );
    println!("cold runs:     {runs} (reporting median, min)");

    if corpus.files.is_empty() {
        println!("\n!! no .baml files found under corpus dir; set BAML_CORPUS");
        return;
    }

    // ---- Cold per-stage attribution ------------------------------------
    // Each stage runs on a fresh db but with all earlier stages already
    // forced/warm within that same db, so the timer isolates that stage.
    let mut stage_samples: Vec<Vec<Duration>> = vec![Vec::new(); 4];
    let mut total_samples: Vec<Duration> = Vec::new();
    for _ in 0..runs {
        let db = fresh_db(&corpus);

        let t0 = Instant::now();
        run_parse_hir(&db);
        let d_parse = t0.elapsed();

        let t1 = Instant::now();
        run_typecheck(&db);
        let d_tir = t1.elapsed();

        let t2 = Instant::now();
        run_codegen(&db);
        let d_codegen = t2.elapsed();

        // Setup cost (db build + builtin load) measured separately below.
        stage_samples[0].push(d_parse);
        stage_samples[1].push(d_tir);
        stage_samples[2].push(d_codegen);
        total_samples.push(d_parse + d_tir + d_codegen);
    }

    // Setup cost: build a fresh db (set_project_root loads builtins + add files).
    let mut setup_samples = Vec::new();
    for _ in 0..runs {
        let t = Instant::now();
        let _db = fresh_db(&corpus);
        setup_samples.push(t.elapsed());
    }
    stage_samples[3] = setup_samples;

    let rows = [
        StageRow {
            name: "setup (db + builtins + add_file)",
            median: median(stage_samples[3].clone()),
            min: *stage_samples[3].iter().min().unwrap(),
        },
        StageRow {
            name: "parse + HIR (item tree + sema index)",
            median: median(stage_samples[0].clone()),
            min: *stage_samples[0].iter().min().unwrap(),
        },
        StageRow {
            name: "typecheck TIR (+ diagnostics)",
            median: median(stage_samples[1].clone()),
            min: *stage_samples[1].iter().min().unwrap(),
        },
        StageRow {
            name: "codegen MIR + emit (bytecode)",
            median: median(stage_samples[2].clone()),
            min: *stage_samples[2].iter().min().unwrap(),
        },
    ];

    let pipeline_total = ms(median(total_samples.clone()));
    println!("\n--- Cold compile, per-stage (pipeline stages exclude setup) ---");
    println!("{:<40} {:>10} {:>10} {:>8}", "stage", "median ms", "min ms", "% pipe");
    for r in &rows {
        let pct = if r.name.starts_with("setup") {
            0.0
        } else {
            ms(r.median) / pipeline_total * 100.0
        };
        println!(
            "{:<40} {:>10.2} {:>10.2} {:>7.1}%",
            r.name,
            ms(r.median),
            ms(r.min),
            pct
        );
    }
    println!("{:<40} {:>10.2}", "PIPELINE TOTAL (parse+tir+codegen)", pipeline_total);

    // ---- Fixed overhead: empty project (builtins only) -----------------
    let empty_corpus = Corpus {
        root: root.clone(),
        files: Vec::new(),
        total_bytes: 0,
        total_lines: 0,
    };
    let (empty_check_med, _) = time_fresh(&empty_corpus, runs, |db| {
        run_parse_hir(db);
        run_typecheck(db);
    });
    println!("\n--- Fixed overhead (stdlib/builtins, zero user files) ---");
    println!(
        "parse+HIR+TIR of builtins only:          {:>10.2} ms  (this is paid every cold compile)",
        ms(empty_check_med)
    );
    let user_check = ms(rows[1].median) + ms(rows[2].median);
    if user_check > 0.0 {
        println!(
            "=> ~{:.0}% of cold check time is fixed builtin overhead",
            ms(empty_check_med) / user_check * 100.0
        );
    }

    // ---- Per-query execution profile (cold full compile) ----------------
    let (db, raw) = fresh_db_counting(&corpus);
    run_parse_hir(&db);
    run_typecheck(&db);
    run_codegen(&db);
    let raw_vec = raw.lock().unwrap().clone();
    let counts = resolve_counts(&db, &raw_vec);
    let total_exec: u64 = counts.values().map(|s| s.execs).sum();
    let mut sorted: Vec<(&String, &QueryStat)> = counts.iter().collect();
    sorted.sort_by(|a, b| b.1.execs.cmp(&a.1.execs));
    println!("\n--- Per-query salsa executions (cold full compile) ---");
    println!("total query executions: {total_exec}");
    println!(
        "{:<46} {:>9} {:>9} {:>7} {:>8}",
        "query (ingredient)", "execs", "keys", "x-reexec", "% total"
    );
    for (name, stat) in sorted.iter().take(30) {
        let reexec = if stat.distinct_keys > 0 {
            stat.execs as f64 / stat.distinct_keys as f64
        } else {
            0.0
        };
        println!(
            "{:<46} {:>9} {:>9} {:>6.1}x {:>7.1}%",
            truncate(name, 46),
            stat.execs,
            stat.distinct_keys,
            reexec,
            stat.execs as f64 / total_exec as f64 * 100.0
        );
    }

    // ---- Steady-state re-query with NO edit -----------------------------
    // After a fully warm compile, call typecheck/codegen again WITHOUT any
    // edit. This separates "untracked work paid on every call" (if these are
    // slow) from "salsa revalidation triggered by an edit" (if these are fast
    // but the incremental-after-edit numbers below are slow).
    {
        let db = fresh_db(&corpus);
        run_parse_hir(&db);
        run_typecheck(&db);
        run_codegen(&db);
        let t = Instant::now();
        run_typecheck(&db);
        let tc2 = t.elapsed();
        let t = Instant::now();
        run_codegen(&db);
        let cg2 = t.elapsed();
        println!("\n--- Steady-state re-query, NO edit (fully warm db) ---");
        println!(
            "typecheck (cached): {:>8.2} ms     codegen (cached): {:>8.2} ms",
            ms(tc2),
            ms(cg2)
        );
        println!("(high values here = untracked work redone every call, independent of edits)");
    }

    // ---- Incremental recompile (editor latency) -------------------------
    // Warm a counting db fully, then edit ONE user file and re-check.
    incremental_probe(&corpus, runs);

    // ---- Machine-readable summary line ----------------------------------
    if let Ok(out_path) = std::env::var("BAML_OUT") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&out_path)
        {
            let _ = writeln!(
                f,
                "{{\"corpus\":\"{}\",\"files\":{},\"lines\":{},\"setup_ms\":{:.2},\"parse_hir_ms\":{:.2},\"tir_ms\":{:.2},\"codegen_ms\":{:.2},\"pipeline_ms\":{:.2},\"fixed_builtin_ms\":{:.2},\"total_query_execs\":{}}}",
                root.display(),
                corpus.files.len(),
                corpus.total_lines,
                ms(rows[0].median),
                ms(rows[1].median),
                ms(rows[2].median),
                ms(rows[3].median),
                pipeline_total,
                ms(empty_check_med),
                total_exec,
            );
        }
    }

    println!("\n(done)\n");
}

fn incremental_probe(corpus: &Corpus, runs: usize) {
    println!("\n--- Incremental recompile after single-file edit (editor latency) ---");

    // Choose the largest user file as the "hot" edit target, and the smallest
    // as the "leaf" target.
    let mut by_size: Vec<&(PathBuf, String)> = corpus.files.iter().collect();
    by_size.sort_by_key(|(_, c)| c.len());
    let leaf = by_size.first().cloned();
    let big = by_size.last().cloned();

    for (label, target) in [("smallest file", leaf), ("largest file", big)] {
        let Some((path, content)) = target else {
            continue;
        };

        // no-op edit (append comment) — measures salsa early-cutoff quality.
        let mut noop_samples: Vec<(Duration, Duration)> = Vec::new();
        let mut real_samples: Vec<(Duration, Duration)> = Vec::new();
        let mut noop_execs = 0u64;
        let mut real_execs = 0u64;
        for i in 0..runs {
            let (mut db, raw) = fresh_db_counting(corpus);
            // warm
            run_parse_hir(&db);
            run_typecheck(&db);
            run_codegen(&db);
            raw.lock().unwrap().clear();

            // no-op edit
            let edited = format!("{content}\n// bench edit {i}\n");
            db.add_or_update_file(path, &edited);
            let t = Instant::now();
            run_typecheck(&db);
            let noop_tc = t.elapsed();
            let t = Instant::now();
            run_codegen(&db);
            let noop_cg = t.elapsed();
            noop_samples.push((noop_tc, noop_cg));
            if i == 0 {
                noop_execs = raw.lock().unwrap().len() as u64;
            }
            raw.lock().unwrap().clear();

            // real edit (append a new class — a genuine semantic change)
            let real = format!("{edited}\nclass BenchInjected{i} {{ x int }}\n");
            db.add_or_update_file(path, &real);
            let t = Instant::now();
            run_typecheck(&db);
            let real_tc = t.elapsed();
            let t = Instant::now();
            run_codegen(&db);
            let real_cg = t.elapsed();
            real_samples.push((real_tc, real_cg));
            if i == 0 {
                real_execs = raw.lock().unwrap().len() as u64;
            }
        }
        let noop_tc = median(noop_samples.iter().map(|s| s.0).collect());
        let noop_cg = median(noop_samples.iter().map(|s| s.1).collect());
        let real_tc = median(real_samples.iter().map(|s| s.0).collect());
        let real_cg = median(real_samples.iter().map(|s| s.1).collect());
        println!(
            "{:<16} no-op: typecheck {:>7.2} + codegen {:>7.2} = {:>7.2} ms ({} q)   real: tc {:>7.2} + cg {:>7.2} = {:>7.2} ms ({} q)",
            label,
            ms(noop_tc), ms(noop_cg), ms(noop_tc + noop_cg), noop_execs,
            ms(real_tc), ms(real_cg), ms(real_tc + real_cg), real_execs,
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n - 1])
    }
}
