use clap::Parser;

/// Replay scenarios through the proxy under each packet strategy and score them.
#[derive(Parser)]
struct Args {
    /// Scenario files or directories (default: ./scenarios).
    paths: Vec<std::path::PathBuf>,
    /// Only this strategy (default: all four).
    #[arg(long)]
    strategy: Option<String>,
    /// Print JSON instead of a Markdown table.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let roots = if args.paths.is_empty() { vec!["scenarios".into()] } else { args.paths };
    let mut files = Vec::new();
    for p in roots {
        if p.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(&p)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect();
            entries.sort();
            files.extend(entries);
        } else {
            files.push(p);
        }
    }
    let strategies: Vec<String> = match &args.strategy {
        Some(s) => vec![s.clone()],
        None => handover_eval::STRATEGIES.iter().map(|s| s.to_string()).collect(),
    };
    let mut scores = Vec::new();
    for f in &files {
        let scenario = handover_eval::load(f)?;
        for s in &strategies {
            scores.push(handover_eval::run(&scenario, s).await?);
        }
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&scores)?);
    } else {
        print!("{}", handover_eval::table(&scores));
    }
    Ok(())
}
