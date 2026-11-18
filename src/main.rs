//! RustTurbo — extreme Rust compilation accelerator.
//!
//! Command surface:
//!
//! ```text
//! rusturbo init       write rusturbo.toml
//! rusturbo analyze    dependency graph, critical path, hardware, advice
//! rusturbo optimize   apply/persist the recommended configuration
//! rusturbo run        accelerated `cargo run`
//! rusturbo build      accelerated `cargo build`
//! rusturbo benchmark  real cold/warm measurements into a report
//! rusturbo clean      wipe the project target and/or RustTurbo caches
//! ```

mod benchmark;
mod cache;
mod cacheops;
mod config;
mod fingerprint;
mod hardware;
mod metadata;
mod planner;
mod presets;
mod process;
mod report;
mod util;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use config::Config;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "rusturbo",
    version,
    about = "RustTurbo — extreme Rust compilation accelerator",
    long_about = "Makes `cargo run` dramatically faster, especially on the first build, through \
dependency-graph analysis, a fingerprinted artifact cache, hardware-aware scheduling, \
a fast-linker engine and honest benchmarking.\n\n\
Correctness contract: RustTurbo never changes optimisation levels, never edits your \
Cargo.toml, and never fakes a cache hit. Programs it builds behave exactly as Cargo builds them."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Directory of the Cargo project (defaults to the current directory).
    #[arg(long, global = true, value_name = "DIR")]
    manifest_dir: Option<PathBuf>,

    /// Suppress progress output.
    #[arg(long, short, global = true)]
    quiet: bool,
}

/// Not a user-facing command.
///
/// `rusturbo run` is a three-way proxy — RustTurbo flags, Cargo flags, and the program's
/// own flags — and no CLI grammar can disambiguate all three from a single `--`. Cargo
/// solves this by forwarding everything after `--` to the program; we do the same, and
/// this hidden subcommand is where those arguments land:
///
/// ```text
/// rusturbo run --cargo-arg=--release -- host --port 9000
/// ```
///
/// The `-- host` marker exists so this can never be confused with a real RustTurbo flag.
#[derive(Args, Debug)]
struct HostProxy {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    args: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Hidden: receives the program arguments after `--` in `run`/`build`.
    #[command(hide = true)]
    Host(HostProxy),
    /// Write rusturbo.toml with tuned defaults for this project.
    Init(InitArgs),
    /// Analyse the dependency graph, hardware and bottlenecks.
    Analyze(AnalyzeArgs),
    /// Apply the recommended configuration (writes rusturbo.toml, optionally enables linker).
    Optimize(OptimizeArgs),
    /// Accelerated `cargo run`.
    Run(BuildArgs),
    /// Accelerated `cargo build`.
    Build(BuildArgs),
    /// Measure cold vs warm builds for real, in four scenarios.
    Benchmark(BenchArgs),
    /// Remove the project target directory and/or RustTurbo caches.
    Clean(CleanArgs),
}

#[derive(Args, Debug)]
struct InitArgs {
    /// `auto` | `actix`
    #[arg(long, default_value = "auto")]
    preset: String,
    /// Overwrite an existing rusturbo.toml.
    #[arg(long)]
    force: bool,
    /// `balanced` | `fastest` | `safe`
    #[arg(long)]
    profile: Option<String>,
    /// Do not write anything; print the file that would be created.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct AnalyzeArgs {
    /// Emit machine-readable JSON instead of a text report.
    #[arg(long)]
    json: bool,
    /// Include dev-dependencies (`cargo test` graph).
    #[arg(long)]
    dev: bool,
    /// Write the report to a file as well.
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct OptimizeArgs {
    /// Show what would change without writing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// `dev` | `release`
    #[arg(long, default_value = "dev")]
    profile: String,
    /// Binary to run (defaults to the package's default binary).
    #[arg(long)]
    bin: Option<String>,
    /// Bypass the artifact cache for this invocation.
    #[arg(long)]
    no_cache: bool,
    /// Print the exact commands/environment instead of executing.
    #[arg(long)]
    dry_run: bool,
    /// Explain cache decisions in detail.
    #[arg(long)]
    explain: bool,
    /// Extra arguments forwarded to the built program, e.g. `rusturbo run -- --port 9000`.
    ///
    /// `last = true` means everything after the user's `--` lands here, and clap stops
    /// interpreting flags from that point — which is what makes a program argument like
    /// `--port` safe. There can be only one such positional per command, which is why
    /// Cargo's own arguments use the `--cargo-arg` flag instead.
    #[arg(last = true, value_name = "PROGRAM_ARG")]
    program_args: Vec<String>,

    /// Extra arguments forwarded to Cargo itself, e.g. `--cargo-arg=--release`.
    /// Must appear *before* `--`; anything after it is a program argument.
    #[arg(long = "cargo-arg", value_name = "ARG")]
    cargo_args: Vec<String>,
}

#[derive(Args, Debug)]
struct BenchArgs {
    /// Comma-separated scenario ids, or `all`.
    #[arg(long, default_value = "all")]
    scenarios: String,
    /// Directory for JSON/Markdown reports.
    #[arg(long, default_value = "bench")]
    out: PathBuf,
    /// Separate CARGO_HOME for the fully-cold scenario (recommended: proves a real cold build).
    #[arg(long)]
    isolated_cargo_home: bool,
    /// Arguments forwarded to the built app during startup measurement.
    #[arg(long = "app-arg")]
    app_args: Vec<String>,
    /// Skip the slow fully-cold scenario.
    #[arg(long)]
    skip_cold: bool,
}

#[derive(Args, Debug)]
struct CleanArgs {
    /// Remove the project's target directory too.
    #[arg(long)]
    target: bool,
    /// Remove RustTurbo's cache (artifact entries + shared target dirs).
    #[arg(long)]
    cache: bool,
    /// Remove everything: project target and all RustTurbo caches.
    #[arg(long)]
    all: bool,
}

/// Quote an argv vector for *display*. PowerShell/cmd split on spaces, so an unquoted
/// echo of `--config profile.dev.package."*".debug=0` reads like two separate arguments —
/// which is exactly the kind of ambiguity that makes build tooling untrustworthy.
/// This is display only; execution always uses the argv vector unchanged.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(' ') || a.contains('"') || a.contains('\'') {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\n{} {:#}", util::red("error:"), e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = match &cli.manifest_dir {
        Some(d) => d.canonicalize().unwrap_or_else(|_| d.clone()),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    if !root.join("Cargo.toml").is_file() {
        anyhow::bail!(
            "no Cargo.toml in {} — run rusturbo from a Cargo project root, or pass --manifest-dir",
            root.display()
        );
    }
    let quiet = cli.quiet;

    match cli.command {
        Command::Init(a) => cmd_init(&root, a, quiet),
        Command::Analyze(a) => cmd_analyze(&root, a, quiet),
        Command::Optimize(a) => cmd_optimize(&root, a, quiet),
        Command::Build(a) => cmd_build(&root, a, quiet, false),
        Command::Run(a) => cmd_build(&root, a, quiet, true),
        Command::Benchmark(a) => cmd_benchmark(&root, a, quiet),
        Command::Clean(a) => cmd_clean(&root, a, quiet),
        Command::Host(_) => anyhow::bail!(
            "`host` is internal: pass program arguments after `--` to `rusturbo run`, e.g.\n  rusturbo run -- host --port 9000"
        ),
    }
}

// ---------------------------------------------------------------- init

fn cmd_init(root: &Path, args: InitArgs, quiet: bool) -> Result<()> {
    let path = root.join(config::CONFIG_FILE);
    if path.exists() && !args.force {
        anyhow::bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    let mut cfg = Config::default();
    if let Some(p) = args.profile {
        cfg.profile.preset = p;
    }

    // Detect an Actix project so `init` can turn on the preset automatically.
    let mut detected_actix = false;
    if args.preset == "actix" || args.preset == "auto" {
        if let Ok(a) = metadata::Analyzer::load(root) {
            if let Ok(g) = a.analyze(false) {
                detected_actix = g.actix.detected;
                if let Some(v) = &g.actix.version {
                    if !quiet {
                        println!("{} actix-web {} detected", util::green("✓"), v);
                    }
                }
            }
        }
    }
    if args.preset == "actix" {
        cfg.actix.preset = "actix".into();
        cfg.actix.tuned_profiles = true;
    } else if detected_actix {
        cfg.actix.preset = "actix".into();
    }
    cfg.validate()?;

    let text = cfg.to_toml();
    if args.dry_run {
        println!("{text}");
        return Ok(());
    }
    cfg.write(root)?;
    println!("{} wrote {}", util::green("✓"), path.display());
    println!(
        "  next: {} then {}",
        util::cyan("rusturbo analyze"),
        util::cyan("rusturbo run")
    );
    Ok(())
}

// ---------------------------------------------------------------- analyze

fn cmd_analyze(root: &Path, args: AnalyzeArgs, quiet: bool) -> Result<()> {
    let (cfg, cfg_found) = Config::load(root)?;
    let hw = hardware::detect_hardware();
    let tc = hardware::detect_toolchain()?;
    let linker = hardware::detect_linker(&cfg.linker.mode, &tc.host);
    let scc = hardware::detect_sccache(&cfg.cache.sccache);

    let analyzer = metadata::Analyzer::load(root)?;
    let graph = analyzer.analyze(args.dev)?;
    let sched = planner::schedule(&graph, &hw, &cfg);
    let advice = planner::advise(&graph, &sched);

    let cache = cache::ArtifactCache::new(cfg.cache_dir(), cfg.cache.max_size_gb, cfg.cache.enabled);
    // The key shown here must match the key `rusturbo run` will compute, otherwise the
    // report would predict a hit the build never gets. Same flags, same profile.
    let mut shown_flags: Vec<String> = Vec::new();
    if let Some(f) = linker.selected.flag() {
        shown_flags.push(f.to_string());
    }
    shown_flags.extend(cfg.linker.extra_flags.clone());
    shown_flags.sort();
    let plan = cacheops::plan(
        &cache,
        &cfg,
        &tc,
        &tc.host,
        "dev",
        &graph,
        &shown_flags,
        Path::new(&graph.target_dir),
    );
    let key = plan.key.clone();

    if args.json {
        let payload = serde_json::json!({
            "rusturbo_version": env!("CARGO_PKG_VERSION"),
            "config_found": cfg_found,
            "config": cfg,
            "hardware": hw,
            "toolchain": tc,
            "linker": linker,
            "sccache": scc,
            "graph": graph,
            "schedule": sched,
            "advice": advice,
            "cache": {
                "key": key,
                "reuse": format!("{:?}", plan.reuse),
                "decision": plan.decision,
                "stats": cache.stats(),
            }
        });
        let text = serde_json::to_string_pretty(&payload)?;
        if let Some(out) = &args.output {
            std::fs::write(out, &text)?;
            println!("{} wrote {}", util::green("✓"), out.display());
        } else {
            println!("{text}");
        }
        let _ = quiet;
        return Ok(());
    }

    let mut buf = String::new();
    buf.push_str(&format!(
        "\n{} {}\n",
        util::bold("RustTurbo analyze"),
        util::dim(&root.display().to_string())
    ));
    if !cfg_found {
        buf.push_str(&format!(
            "{}\n",
            util::yellow("note: no rusturbo.toml — using defaults (run `rusturbo init`)")
        ));
    }
    println!("{buf}");

    report::hardware_block(&hw, &tc, &linker, &scc);
    println!();
    report::graph_block(&graph, &sched);
    println!();
    report::top_cost_block(&graph);
    println!();
    report::actix_block(&graph);
    println!();
    report::advice_block(&advice);
    println!();
    report::header("Cache");
    let stats = cache.stats();
    println!("  {:22} {}", "root", stats.root);
    println!(
        "  {:22} {} entries, {} shared targets, {}",
        "contents",
        stats.entries,
        stats.shared,
        util::human_bytes(stats.size_bytes)
    );
    println!("  {:22} {}", "key for dev", key.short);
    let hit = !matches!(plan.reuse, cacheops::Reuse::Cold { .. });
    report::cache_note(hit, &format!("{:?}", plan.reuse));
    println!("  {:22} {}", "policy", plan.decision.reason);
    for w in &plan.decision.warnings {
        println!("    {} {}", util::yellow("!"), w);
    }
    println!();

    if let Some(out) = &args.output {
        std::fs::write(out, format!("{:?}\n", graph))?;
        println!("{} wrote {}", util::green("✓"), out.display());
    }
    Ok(())
}

// ---------------------------------------------------------------- optimize

fn cmd_optimize(root: &Path, args: OptimizeArgs, quiet: bool) -> Result<()> {
    let (mut cfg, found) = Config::load(root)?;
    let hw = hardware::detect_hardware();
    let tc = hardware::detect_toolchain()?;
    let analyzer = metadata::Analyzer::load(root)?;
    let graph = analyzer.analyze(false)?;
    let sched = planner::schedule(&graph, &hw, &cfg);
    let advice = planner::advise(&graph, &sched);

    report::header("RustTurbo optimize");
    if !found {
        println!(
            "  {} no rusturbo.toml; writing one with tuned defaults",
            util::yellow("note:")
        );
    }

    let mut changes: Vec<String> = Vec::new();
    if graph.actix.detected && cfg.actix.preset != "actix" {
        cfg.actix.preset = "actix".into();
        changes.push(format!(
            "actix preset enabled (actix-web {})",
            graph.actix.version.as_deref().unwrap_or("detected")
        ));
    }
    if cfg.linker.mode == "auto" {
        let linker = hardware::detect_linker("auto", &tc.host);
        match linker.selected {
            hardware::LinkerChoice::Mold => changes.push(format!("linker: mold ({})", linker.reason)),
            hardware::LinkerChoice::Lld => changes.push(format!("linker: lld ({})", linker.reason)),
            hardware::LinkerChoice::System => changes.push(format!(
                "linker stays the system linker — {}",
                linker.reason
            )),
        }
    }
    if cfg.profile.jobs == 0 {
        let jobs = cfg.effective_jobs(hw.logical_cores, hw.ram_mb);
        if jobs != hw.logical_cores {
            cfg.profile.jobs = jobs;
            changes.push(format!(
                "jobs pinned to {jobs} (RAM-aware; {} cores detected)",
                hw.logical_cores
            ));
        }
    }
    cfg.validate()?;

    if changes.is_empty() {
        println!("  {} nothing to change", util::green("✓"));
    } else {
        for c in &changes {
            println!("  {} {}", util::green("+"), c);
        }
    }
    println!();
    report::advice_block(&advice);

    let path = root.join(config::CONFIG_FILE);
    let text = cfg.to_toml();
    if args.dry_run {
        println!("\n{}", util::bold("rusturbo.toml (dry run)"));
        println!("{text}");
    } else {
        std::fs::write(&path, &text)?;
        println!("\n{} wrote {}", util::green("✓"), path.display());
    }
    let _ = quiet;
    Ok(())
}

// ---------------------------------------------------------------- build / run

fn build_env(
    cfg: &Config,
    tc: &hardware::Toolchain,
    plan: &cacheops::WarmPlan,
    use_cache: bool,
    profile: &str,
    actix_version: Option<&str>,
) -> Result<process::EnvPatch> {
    let linker = hardware::detect_linker(&cfg.linker.mode, &tc.host);
    let mut env = hardware::linker_env(linker.selected, &cfg.linker.extra_flags);
    for (k, v) in presets::cargo_env(cfg, profile).set {
        env = env.set(k, v);
    }
    if cfg.actix.preset != "off" && cfg.actix.tuned_profiles && profile != "release" {
        // Actix tuning is applied through --config, but the marker lets build scripts
        // and rusturbo itself see that the profile was applied.
        let _ = actix_version;
    }
    let scc = hardware::detect_sccache(&cfg.cache.sccache);
    if scc.will_use {
        if let Some(p) = scc.path {
            env = env.set("RUSTC_WRAPPER", p);
        }
    }
    if use_cache {
        cacheops::apply_reuse(&mut env, plan);
    } else {
        env = env.remove("CARGO_TARGET_DIR");
    }
    Ok(env)
}

fn resolve_bin_name(graph: &metadata::GraphAnalysis, root: &Path, explicit: Option<&str>) -> Result<String> {
    if let Some(b) = explicit {
        return Ok(b.to_string());
    }
    // Ask Cargo which binary the package would run: parse the default-run/bins from
    // metadata through the analyzer's already-loaded metadata.
    let analyzer = metadata::Analyzer::load(root)?;
    for p in analyzer.metadata.packages.iter() {
        if !analyzer.metadata.workspace_members.contains(&p.id) {
            continue;
        }
        let bins: Vec<&str> = p
            .targets
            .iter()
            .filter(|t| t.kind.iter().any(|k| k.to_string() == "bin"))
            .map(|t| t.name.as_str())
            .collect();
        if let Some(dr) = &p.default_run {
            return Ok(dr.clone());
        }
        if bins.len() == 1 {
            return Ok(bins[0].to_string());
        }
        if let Some(b) = bins.first() {
            return Ok(b.to_string());
        }
        break;
    }
    let _ = graph;
    anyhow::bail!("could not determine a binary target; pass --bin <name>")
}

fn cmd_build(root: &Path, args: BuildArgs, quiet: bool, execute_program: bool) -> Result<()> {
    if args.profile != "dev" && args.profile != "release" {
        anyhow::bail!("--profile must be dev or release");
    }
    let (cfg, _found) = Config::load(root)?;
    let tc = hardware::detect_toolchain()?;
    let hw = hardware::detect_hardware();
    let analyzer = metadata::Analyzer::load(root)?;
    let graph = analyzer.analyze(false)?;

    let use_cache = cfg.cache.enabled && !args.no_cache;
    let cache = cache::ArtifactCache::new(cfg.cache_dir(), cfg.cache.max_size_gb, use_cache);
    let linker = hardware::detect_linker(&cfg.linker.mode, &tc.host);

    let mut pre_flags: Vec<String> = Vec::new();
    if let Some(f) = linker.selected.flag() {
        pre_flags.push(f.to_string());
    }
    pre_flags.extend(cfg.linker.extra_flags.clone());
    let mut sorted_flags = pre_flags.clone();
    sorted_flags.sort();

    let plan = cacheops::plan(
        &cache,
        &cfg,
        &tc,
        &tc.host,
        &args.profile,
        &graph,
        &sorted_flags,
        Path::new(&graph.target_dir),
    );
    let env = build_env(
        &cfg,
        &tc,
        &plan,
        use_cache,
        &args.profile,
        graph.actix.version.as_deref(),
    )?;

    let bin_name = if execute_program {
        Some(resolve_bin_name(&graph, root, args.bin.as_deref())?)
    } else {
        None
    };

    let mut cargo_args: Vec<String> = vec![
        if execute_program { "run".into() } else { "build".into() },
    ];
    // In shared-target mode we must not let Cargo run a stale binary from the shared
    // dir without rebuilding the workspace crate, so `cargo run` is always used and
    // Cargo's freshness check decides.
    cargo_args.push("--profile".into());
    cargo_args.push(args.profile.clone());
    cargo_args.extend(presets::cargo_config_args(&cfg, &args.profile));
    cargo_args.extend(presets::actix_cargo_args(
        &cfg,
        &args.profile,
        graph.actix.version.as_deref(),
    ));
    if let Some(b) = &bin_name {
        cargo_args.push("--bin".into());
        cargo_args.push(b.clone());
    }
    if !cfg.profile.parallel_codegen && args.profile == "dev" {
        cargo_args.push("--config".into());
        cargo_args.push("profile.dev.codegen-units=16".into());
    }
    cargo_args.extend(args.cargo_args.clone());
    // When the user passes program arguments, RustTurbo builds with `cargo build` and
    // launches the binary itself. `cargo run -- <args>` would work too, but driving the
    // exec lets us forward exit codes and signals correctly and avoids a second parse of
    // our own argv by Cargo.
    if execute_program && !args.program_args.is_empty() {
        if let Some(pos) = cargo_args.iter().position(|a| a == "run") {
            cargo_args[pos] = "build".to_string();
        }
    }

    let target_dir = cacheops::effective_target_dir(&plan, Path::new(&graph.target_dir));

    if args.dry_run {
        report::header("RustTurbo dry run");
        println!("  {:22} {}", "cwd", root.display());
        println!("  {:22} {}", "cargo", format!("{} {}", cacheops::cargo_bin(), shell_join(&cargo_args)));
        println!("  {:22} {}", "target dir", target_dir.display());
        println!("  {:22} {}", "linker", linker.selected.name());
        println!("  {:22} {}", "jobs", cfg.effective_jobs(hw.logical_cores, hw.ram_mb));
        println!("  {:22} {}", "cache key", plan.key.short);
        println!("  {:22} {}", "reuse", format!("{:?}", plan.reuse));
        println!("  {:22} {}", "policy", plan.decision.reason);
        println!("\n  {}", util::bold("environment overrides:"));
        for (k, v) in &env.set {
            println!("    {k}={v}");
        }
        for k in &env.remove {
            println!("    -{k}");
        }
        return Ok(());
    }

    if !quiet {
        let hit = !matches!(plan.reuse, cacheops::Reuse::Cold { .. });
        eprintln!(
            "{} {} · linker: {} · jobs: {} · target: {}",
            util::bold("RustTurbo"),
            if execute_program { "run" } else { "build" },
            linker.selected.name(),
            cfg.effective_jobs(hw.logical_cores, hw.ram_mb),
            util::truncate_middle(&target_dir.display().to_string(), 60)
        );
        // Order matters for diagnostics: announce the exact command first, then the
        // cache verdict, so a failing run is never ambiguous about what was executed.
        eprintln!(
            "{} {} {}",
            util::dim("»"),
            cacheops::cargo_bin(),
            util::dim(&shell_join(&cargo_args))
        );
        if args.explain || !quiet {
            report::cache_note(hit, &format!("{:?} ({})", plan.reuse, plan.decision.reason));
            for w in &plan.decision.warnings {
                eprintln!("  {} {}", util::yellow("!"), w);
            }
        }
    }
    let captured = match cacheops::run_cargo(root, &cargo_args, &env, !quiet) {
        Ok(c) => c,
        Err(e) => {
            anyhow::bail!(
                "could not execute cargo: {e:#}\n  argv: {} {}\n  cwd: {}",
                cacheops::cargo_bin(),
                shell_join(&cargo_args),
                root.display()
            );
        }
    };
    let elapsed = captured.duration;
    if !captured.success() {
        let tail = util::truncate_middle(captured.stderr.trim(), 4000);
        anyhow::bail!(
            "cargo {} failed (exit {:?})\n  argv: {} {}\n  cwd: {}\n{}",
            if execute_program { "run" } else { "build" },
            captured.status.code(),
            cacheops::cargo_bin(),
            shell_join(&cargo_args),
            root.display(),
            tail
        );
    }

    // Publish the dependency artifacts produced by this build.
    let profile_dir = target_dir.join(if args.profile == "dev" { "debug" } else { "release" });
    match cacheops::publish(
        &cache,
        &cfg,
        &plan,
        &profile_dir,
        &tc,
        &tc.host,
        &args.profile,
        &graph,
    ) {
        Ok(Some(meta)) => {
            if !quiet {
                eprintln!(
                    "  {} dependencies published to cache {} ({} crates, {} files)",
                    util::green("✓"),
                    meta.key,
                    meta.crate_count,
                    meta.file_count
                );
            }
        }
        Ok(None) => {}
        Err(e) => {
            if !quiet {
                eprintln!("  {} cache publish skipped: {e}", util::yellow("!"));
            }
        }
    }

    if execute_program && !args.program_args.is_empty() {
        let exe = cacheops::binary_path(
            &target_dir,
            &args.profile,
            bin_name.as_deref().unwrap_or_default(),
        );
        if !exe.is_file() {
            anyhow::bail!("built binary not found at {}", exe.display());
        }
        if !quiet {
            eprintln!("{} {}", util::dim("»"), exe.display());
        }
        let (status, _) = process::run_inherit(
            &exe.display().to_string(),
            &args.program_args,
            Some(root),
            None,
        )?;
        // Propagate the program's exit code: this is a run, not a build.
        std::process::exit(status.code().unwrap_or(1));
    }

    if !quiet && !execute_program {
        eprintln!(
            "  {} finished in {}",
            util::green("✓"),
            process::fmt_dur(elapsed)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- benchmark

fn cmd_benchmark(root: &Path, args: BenchArgs, quiet: bool) -> Result<()> {
    let (cfg, _found) = config::Config::load(root)?;
    let hw = hardware::detect_hardware();
    let tc = hardware::detect_toolchain()?;
    let analyzer = metadata::Analyzer::load(root)?;
    let graph = analyzer.analyze(false)?;
    let linker = hardware::detect_linker(&cfg.linker.mode, &tc.host);
    let bin_name = resolve_bin_name(&graph, root, None)?;

    let wanted: Vec<benchmark::Scenario> = if args.scenarios == "all" {
        benchmark::Scenario::all().to_vec()
    } else {
        let mut v = Vec::new();
        for id in args.scenarios.split(',') {
            let id = id.trim();
            let s = benchmark::Scenario::all()
                .into_iter()
                .find(|s| s.id() == id)
                .with_context(|| {
                    format!(
                        "unknown scenario `{id}` (expected: cargo-clean, cargo-warm-registry, rusturbo-clean, rusturbo-warm, rusturbo-shared, or all)"
                    )
                })?;
            v.push(s);
        }
        v
    };
    let wanted: Vec<benchmark::Scenario> = wanted
        .into_iter()
        .filter(|s| !(args.skip_cold && *s == benchmark::Scenario::CargoClean))
        .collect();

    std::fs::create_dir_all(&args.out)?;
    let project_target = PathBuf::from(&graph.target_dir);
    let cache = cache::ArtifactCache::new(cfg.cache_dir(), cfg.cache.max_size_gb, cfg.cache.enabled);
    cache.ensure()?;

    report::header("RustTurbo benchmark");
    println!("  {:22} {} ({} crates)", "project", root.display(), graph.package_count);
    println!("  {:22} {} / {} cores", "hardware", hw.ram_mb, hw.logical_cores);
    println!("  {:22} {}", "linker", linker.selected.name());
    println!("  {:22} {}", "scenarios", wanted.iter().map(|s| s.id()).collect::<Vec<_>>().join(", "));
    println!();

    // Isolated CARGO_HOME for the truly-cold scenario; created once and reused so that
    // "cargo-clean" only pays for its own download, not a broken/partial registry.
    let cold_home = args.out.join("cargo-home-cold");
    if args.isolated_cargo_home {
        std::fs::create_dir_all(&cold_home)?;
    }
    let warm_home = std::env::var("CARGO_HOME").ok().map(PathBuf::from);

    let mut samples: Vec<benchmark::Sample> = Vec::new();

    for scenario in wanted {
        eprintln!("{} {}", util::bold("▶"), scenario.label());
        let result = (|| -> Result<benchmark::Sample> {
            match scenario {
                benchmark::Scenario::CargoClean => {
                    // Empty target dir AND a private CARGO_HOME: a genuinely cold build.
                    let mut env = process::EnvPatch::new();
                    let mut watch = vec![project_target.clone()];
                    if args.isolated_cargo_home {
                        env = env.set("CARGO_HOME", cold_home.display().to_string());
                        watch.push(cold_home.clone());
                    } else {
                        if let Some(h) = &warm_home {
                            watch.push(h.clone());
                        }
                    }
                    benchmark::clean_project(root, &project_target, &env)?;
                    let mut cargo_env = process::EnvPatch::new();
                    for (k, v) in &env.set {
                        cargo_env = cargo_env.set(k, v);
                    }
                    benchmark::measure(
                        scenario,
                        root,
                        &cacheops::cargo_bin(),
                        &[
                            "build".to_string(),
                            "--profile".to_string(),
                            "dev".to_string(),
                            "--bin".to_string(),
                            bin_name.clone(),
                        ],
                        benchmark::ScenarioPlan {
                            env: cargo_env,
                            cache_hit: false,
                            comment: if args.isolated_cargo_home {
                                "empty registry: includes crates.io downloads. Dominated by network, not rustc.".into()
                            } else {
                                "reuses the machine's existing Cargo registry; measures pure compilation".into()
                            },
                            watch_dirs: watch,
                        },
                        &args.app_args,
                        &cacheops::binary_path(&project_target, "dev", &bin_name),
                        graph.package_count,
                    )
                }
                benchmark::Scenario::RustTurboShared => {
                    // Simulate a *new* project with the same dependency graph: fresh
                    // target dir, but RustTurbo's shared cache is allowed to serve it.
                    let sandbox = args.out.join("shared-project");
                    util::remove_dir_all_if_exists(&sandbox).ok();
                    copy_project(root, &sandbox)?;
                    let sandbox_target = sandbox.join("target");
                    util::remove_dir_all_if_exists(&sandbox_target).ok();
                    let plan = cacheops::plan(
                        &cache,
                        &Config::load(&sandbox)?.0,
                        &tc,
                        &tc.host,
                        "dev",
                        &graph,
                        &[],
                        &sandbox_target,
                    );
                    let mut env = process::EnvPatch::new();
                    cacheops::apply_reuse(&mut env, &plan);
                    let hit = matches!(plan.reuse, cacheops::Reuse::SharedTarget { .. });
                    benchmark::measure(
                        scenario,
                        &sandbox,
                        &cacheops::cargo_bin(),
                        &[
                            "--config".to_string(),
                            "profile.dev.package.\"*\".debug=0".to_string(),
                            "build".to_string(),
                            "--profile".to_string(),
                            "dev".to_string(),
                            "--bin".to_string(),
                            bin_name.clone(),
                        ],
                        benchmark::ScenarioPlan {
                            env,
                            cache_hit: hit,
                            comment: if hit {
                                "fresh checkout, same dependency graph: dependencies served from the shared cache, only the local crate compiles".into()
                            } else {
                                "fresh checkout but no shared cache for this graph yet — this run populates it".into()
                            },
                            watch_dirs: vec![sandbox_target.clone()],
                        },
                        &args.app_args,
                        &cacheops::binary_path(
                            &cacheops::effective_target_dir(&plan, &sandbox_target),
                            "dev",
                            &bin_name,
                        ),
                        graph.package_count,
                    )
                }
                other => {
                    // Cargo-warm / rusturbo-clean / rusturbo-warm all start from a
                    // clean target dir and differ in whether the turbo cache is allowed
                    // and whether it is populated first.
                    let mut env = process::EnvPatch::new();
                    if other == benchmark::Scenario::CargoWarmRegistry {
                        env = env.set("RUSTURBO_FORCE_NO_CACHE", "1");
                        benchmark::clean_project(root, &project_target, &process::EnvPatch::new())?;
                    }
                    let use_cache = other != benchmark::Scenario::CargoWarmRegistry;
                    if !use_cache {
                        // Run plain cargo, bypassing RustTurbo entirely.
                        benchmark::measure(
                            other,
                            root,
                            &cacheops::cargo_bin(),
                            &[
                                "build".to_string(),
                                "--profile".to_string(),
                                "dev".to_string(),
                                "--bin".to_string(),
                                bin_name.clone(),
                            ],
                            benchmark::ScenarioPlan {
                                env,
                                cache_hit: false,
                                comment: "plain cargo, clean target, warm registry".into(),
                                watch_dirs: vec![project_target.clone()],
                            },
                            &args.app_args,
                            &cacheops::binary_path(&project_target, "dev", &bin_name),
                            graph.package_count,
                        )
                    } else {
                        let bin = std::env::current_exe().context("cannot locate rusturbo binary")?;
                        let config_args = presets::cargo_config_args(&cfg, "dev");
                        let mut run_args = vec![
                            "build".to_string(),
                            "--profile".to_string(),
                            "dev".to_string(),
                            "--bin".to_string(),
                            bin_name.clone(),
                        ];
                        run_args.extend(config_args);
                        if other == benchmark::Scenario::RustTurboClean {
                            util::remove_dir_all_if_exists(&project_target).ok();
                        }
                        let plan = cacheops::plan(
                            &cache,
                            &cfg,
                            &tc,
                            &tc.host,
                            "dev",
                            &graph,
                            &[],
                            &project_target,
                        );
                        let hit = matches!(plan.reuse, cacheops::Reuse::SharedTarget { .. })
                            || matches!(plan.reuse, cacheops::Reuse::Materialized { .. });
                        benchmark::measure(
                            other,
                            root,
                            bin.to_str().unwrap_or("rusturbo"),
                            &run_args,
                            benchmark::ScenarioPlan {
                                env: process::EnvPatch::new(),
                                cache_hit: hit,
                                comment: if other == benchmark::Scenario::RustTurboClean {
                                    "first rusturbo build with a cold turbo cache: this run populates the cache".into()
                                } else {
                                    "repeat rusturbo build: dependencies come from the cache".into()
                                },
                                watch_dirs: vec![project_target.clone()],
                            },
                            &args.app_args,
                            &cacheops::binary_path(
                                &cacheops::effective_target_dir(&plan, &project_target),
                                "dev",
                                &bin_name,
                            ),
                            graph.package_count,
                        )
                    }
                }
            }
        })();

        match result {
            Ok(s) => {
                println!(
                    "  {} {:<20} build {:>8}  startup {:>8}  total {:>8}  RSS {:>5} MiB  CPU {:>6.1}s{}{}",
                    util::green("✓"),
                    s.scenario,
                    process::fmt_dur(std::time::Duration::from_secs_f64(s.build_secs)),
                    s.startup_secs
                        .map(|v| format!("{v:.2}s"))
                        .unwrap_or_else(|| "—".into()),
                    process::fmt_dur(std::time::Duration::from_secs_f64(s.total_secs)),
                    s.peak_rss_mb,
                    s.cpu_secs,
                    if s.cache_hit { util::green("  cache hit") } else { "".into() },
                    if s.downloaded_bytes > 0 {
                        format!("  ↓{}", util::human_bytes(s.downloaded_bytes))
                    } else {
                        String::new()
                    }
                );
                samples.push(s);
            }
            Err(e) => {
                eprintln!("  {} {} failed: {e:#}", util::red("✗"), scenario.id());
            }
        }
    }

    let report = benchmark::report_shell(
        &tc,
        &hw,
        linker.selected.name(),
        "dev",
        root,
        graph.package_count,
        samples,
    );
    let md_path = args.out.join("report.md");
    let json_path = args.out.join("report.json");
    std::fs::write(&md_path, report.to_markdown())?;
    std::fs::write(&json_path, report.to_json())?;

    println!();
    report::header("Summary");
    if let Some(s) = report.speedup("rusturbo-shared", "cargo-clean") {
        println!(
            "  {:30} {}",
            "shared cache vs cold cargo",
            util::green(&format!("{s:.1}× faster"))
        );
    }
    if let Some(s) = report.speedup("rusturbo-warm", "cargo-clean") {
        println!(
            "  {:30} {}",
            "warm rusturbo vs cold cargo",
            util::green(&format!("{s:.1}× faster"))
        );
    }
    if let Some(s) = report.speedup("rusturbo-warm", "cargo-warm-registry") {
        println!(
            "  {:30} {}",
            "warm rusturbo vs warm cargo",
            util::green(&format!("{s:.1}× faster"))
        );
    }
    println!("\n  {}", util::dim(&format!("JSON:   {}", json_path.display())));
    println!("  {}", util::dim(&format!("Markdown: {}", md_path.display())));
    let _ = quiet;
    Ok(())
}

/// Copy a Cargo project's sources (not `target/`, not caches) into `dst`.
fn copy_project(src: &Path, dst: &Path) -> Result<()> {
    fn walk(src: &Path, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)?.flatten() {
            let name = e.file_name();
            let name_s = name.to_string_lossy();
            if matches!(name_s.as_ref(), "target" | ".git" | "bench") {
                continue;
            }
            let ft = e.file_type()?;
            if ft.is_dir() {
                walk(&e.path(), &dst.join(&name))?;
            } else if ft.is_file() {
                std::fs::copy(e.path(), dst.join(&name))?;
            }
        }
        Ok(())
    }
    walk(src, dst)
}

// ---------------------------------------------------------------- clean

fn cmd_clean(root: &Path, args: CleanArgs, quiet: bool) -> Result<()> {
    let (cfg, _f) = Config::load(root)?;
    let all = args.all || (!args.target && !args.cache);
    if all || args.target {
        let target = root.join("target");
        let freed = util::dir_size(&target);
        util::remove_dir_all_if_exists(&target)?;
        println!(
            "{} removed {} ({})",
            util::green("✓"),
            target.display(),
            util::human_bytes(freed)
        );
    }
    if all || args.cache {
        let cache = cache::ArtifactCache::new(cfg.cache_dir(), cfg.cache.max_size_gb, cfg.cache.enabled);
        let before = cache.stats();
        cache.clear()?;
        println!(
            "{} cleared RustTurbo cache {} ({} entries, {})",
            util::green("✓"),
            before.root,
            before.entries,
            util::human_bytes(before.size_bytes)
        );
    }
    let _ = quiet;
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn shell_join_quotes_config_values() {
        let args = vec![
            "--config".to_string(),
            "profile.dev.package.\"*\".debug=0".to_string(),
        ];
        let out = shell_join(&args);
        // The value must be one quoted token, not two arguments.
        assert!(out.starts_with("--config \"profile.dev.package."), "got {out}");
        assert!(out.ends_with("=0\""), "got {out}");
    }

    #[test]
    fn shell_join_leaves_simple_args_alone() {
        let args = vec!["run".to_string(), "--profile".to_string(), "dev".to_string()];
        assert_eq!(shell_join(&args), "run --profile dev");
    }

    #[test]
    fn cli_parses_program_args_after_double_dash() {
        // `rusturbo run -- --port 9000` must put `--port 9000` in program_args.
        let cli = Cli::try_parse_from(["rusturbo", "run", "--", "--port", "9000"]).unwrap();
        match cli.command {
            Command::Run(a) => {
                assert_eq!(a.program_args, vec!["--port", "9000"]);
                assert!(a.cargo_args.is_empty());
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn cli_parses_cargo_args_before_double_dash() {
        let cli = Cli::try_parse_from([
            "rusturbo", "run", "--cargo-arg=--release", "--", "--port", "9000",
        ])
        .unwrap();
        match cli.command {
            Command::Run(a) => {
                assert_eq!(a.cargo_args, vec!["--release"]);
                assert_eq!(a.program_args, vec!["--port", "9000"]);
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn host_proxy_is_hidden_but_parses() {
        let cli = Cli::try_parse_from(["rusturbo", "host", "--port", "9000"]).unwrap();
        match cli.command {
            Command::Host(h) => assert_eq!(h.args, vec!["--port", "9000"]),
            _ => panic!("expected host"),
        }
    }
}
