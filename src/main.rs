use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::available_parallelism;

use clap::Parser;
use log::*;
use rayon::prelude::*;

use cargo_group_imports::*;

fn main_impl() -> anyhow::Result<()> {
    let MainFlags::GroupImports(args) = MainFlags::parse();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .write_style(args.write_style())
        .init();
    let start = std::time::Instant::now();

    // Retrieve workspace packages
    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path("Cargo.toml")
        .current_dir(&args.workspace)
        .exec()?;
    let workspace_packages: WorkspacePackages = metadata
        .workspace_packages()
        .into_iter()
        .map(|p| {
            (
                p.name.replace('-', "_"),
                p.manifest_path.parent().unwrap().into(),
            )
        })
        .collect();
    info!("Workspace packages: {:?}", workspace_packages);
    // Edition per package, passed to rustfmt (which reads from stdin and would otherwise
    // default to edition 2015).
    let editions: HashMap<String, String> = metadata
        .workspace_packages()
        .into_iter()
        .map(|p| (p.name.replace('-', "_"), p.edition.to_string()))
        .collect();

    let output = workspace_packages
        .par_iter()
        // Process each package
        .flat_map(|(name, root)| {
            info!("Processing {}", name);
            let edition = editions.get(name).map_or("2021", String::as_str);

            // Process each file in the package
            let files: Vec<PathBuf> = walkdir::WalkDir::new(root)
                .into_iter()
                .filter_entry(|e| {
                    let dir_name = e.file_name().to_string_lossy();
                    // Exclude cargo generated code
                    !dir_name.contains("target")
                    // Exclude hidden directories (.git)
                    && !dir_name.starts_with(".")
                    // Exclude e.g. target/ for the root package
                    && !e.path().join("CACHEDIR.TAG").exists()
                    // Exclude other workspace members (especially for the root crate)
                    && workspace_packages
                        .iter()
                        .filter(|(name2, _)| name2 != &name )
                        .all(|(_, root2)| {
                            root2 == &metadata.workspace_root || !e.path().starts_with(root2)})
                })
                .filter_map(|e| e.ok())
                // Rust source files
                .filter(|f| {
                    f.file_type().is_file() && f.path().extension().map_or(false, |e| e == "rs")
                })
                .map(|f| f.path().to_owned())
                .collect();
            files
                .into_par_iter()
                .with_max_len(available_parallelism().map(|n| n.get()).unwrap_or(8))
                .map(|f| process_file(&f, name, edition, &workspace_packages, &args))
        })
        .collect::<Result<Vec<_>, _>>()?;
    info!(
        "Processed {} files in {:?}, {} changed",
        output.len(),
        start.elapsed(),
        output.iter().filter(|x| **x).count()
    );
    if !args.fix && output.iter().any(|x| *x) {
        warn!("Not all files are formatted. Rerun with --fix to attempt to fix the issues");
        std::process::exit(1);
    }

    Ok(())
}

fn main() {
    if let Err(e) = main_impl() {
        error!("{}", e);
        std::process::exit(2);
    }
}
