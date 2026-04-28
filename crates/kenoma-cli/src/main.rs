use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use kenoma_backtester::engine::{
    BacktestEngine, BuyFirstBarStrategy, NoopStrategy, RunManifest, Strategy,
};
use kenoma_data::{
    canonical_schema, read_bar_csv, read_bar_parquet, read_jsonl_events, write_jsonl_events,
};
use kenoma_types::MarketEvent;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "kenoma-bt", about = "Kenoma research backtester")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        #[arg(long)]
        manifest: PathBuf,
    },
    Audit {
        #[arg(long)]
        run_dir: PathBuf,
    },
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    Convert {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        input_kind: String,
        #[arg(long)]
        output_jsonl: PathBuf,
    },
    BookVerify {
        #[arg(long)]
        mbo: PathBuf,
        #[arg(long)]
        mbp10: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SchemaCommand {
    Print,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run { manifest } => run_manifest(manifest),
        Commands::Audit { run_dir } => audit_run(run_dir),
        Commands::Schema {
            command: SchemaCommand::Print,
        } => {
            println!("{}", serde_json::to_string_pretty(&canonical_schema())?);
            Ok(())
        }
        Commands::Convert {
            input,
            input_kind,
            output_jsonl,
        } => convert(input, &input_kind, output_jsonl),
        Commands::BookVerify { mbo, mbp10 } => book_verify(mbo, mbp10),
    }
}

fn run_manifest(path: PathBuf) -> Result<()> {
    let manifest = RunManifest::from_path(&path)?;
    match manifest.strategy.name.as_str() {
        "noop" => run_with_strategy(NoopStrategy, manifest),
        "buy_first_bar" => {
            let instrument_id = manifest
                .strategy
                .params
                .get("instrument_id")
                .and_then(|v| v.as_integer())
                .unwrap_or_else(|| manifest.universe.first().map(|s| s.id as i64).unwrap_or(0))
                as u32;
            let qty = manifest
                .strategy
                .params
                .get("qty")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .unwrap_or(1.0);
            run_with_strategy(BuyFirstBarStrategy::new(instrument_id, qty), manifest)
        }
        other => bail!(
            "unknown built-in strategy '{other}'. Library users can run arbitrary Rust Strategy implementations through kenoma_engine."
        ),
    }
}

fn run_with_strategy<S: Strategy>(strategy: S, manifest: RunManifest) -> Result<()> {
    let output_dir = manifest.output.dir.clone();
    let (report, audit) = BacktestEngine::run_manifest(strategy, manifest.clone())?;
    kenoma_backtester::engine::write_run_artifacts(&manifest, &audit, &report, &output_dir)?;
    println!(
        "run_id={} end_equity={:.2} fills={} artifacts={}",
        report.run_id,
        report.metrics.end_equity,
        report.fills.len(),
        output_dir.display()
    );
    Ok(())
}

fn audit_run(run_dir: PathBuf) -> Result<()> {
    let audit_path = run_dir.join("audit.json");
    let audit: kenoma_backtester::audit::AuditTrail = serde_json::from_reader(
        std::fs::File::open(&audit_path)
            .with_context(|| format!("opening {}", audit_path.display()))?,
    )?;
    let warnings = audit.warnings().count();
    let errors = audit.errors().count();
    println!(
        "run_dir={} audit_events={} warnings={} errors={}",
        run_dir.display(),
        audit.events.len(),
        warnings,
        errors
    );
    if errors > 0 {
        bail!("audit contains {errors} errors");
    }
    if audit.mode == kenoma_backtester::ValidationMode::Strict && warnings > 0 {
        bail!("strict audit contains {warnings} warnings");
    }
    Ok(())
}

fn convert(input: PathBuf, input_kind: &str, output_jsonl: PathBuf) -> Result<()> {
    let events: Vec<MarketEvent> = match input_kind {
        "bar_csv" => read_bar_csv(&input)?,
        "bar_parquet" => read_bar_parquet(&input)?,
        "jsonl_events" => read_jsonl_events(&input)?,
        other => bail!("unsupported input_kind '{other}'"),
    };
    write_jsonl_events(&output_jsonl, &events)?;
    println!(
        "wrote {} events to {}",
        events.len(),
        output_jsonl.display()
    );
    Ok(())
}

fn book_verify(mbo: PathBuf, mbp10: Option<PathBuf>) -> Result<()> {
    let mbp10 = mbp10
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<not provided>".to_string());
    println!(
        "book verification entrypoint is wired. DBN replay is available through kenoma_data::ingest_dbn_mbo with the dbn feature. mbo={} mbp10={}",
        mbo.display(),
        mbp10
    );
    Ok(())
}
