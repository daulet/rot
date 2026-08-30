use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::PathBuf,
};

use anyhow::{Context, Result};
use ra_ap_syntax::Edition;
use rayon::{ThreadPoolBuilder, prelude::*};

use crate::{
    cfg::CfgProfile,
    cli::FastCli,
    model::{
        BucketReport, Contexts, Diagnostic, DiagnosticSeverity, FileReport, OutputRole, Report,
        SelectedPathKind, SelectedPathReport, SelectionReport, SourceMetrics,
    },
    paths::{canonical_or_original, portable},
    source::{LocalFile, analyze_file, reachability_states},
    workspace::{Inventory, inventory},
};

pub fn analyze(cli: &FastCli) -> Result<Report> {
    let mut inventory = inventory(cli)?;
    let cfg_profile = CfgProfile::new(
        inventory.cfg_true.clone(),
        inventory.cfg_false.clone(),
        inventory.cfg_closed_world.clone(),
        &cli.test_attribute,
    );
    let workers = cli
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    let pool = ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|index| format!("rot-{index}"))
        .build()
        .context("cannot create analysis worker pool")?;

    let mut pending = inventory.sources.iter().cloned().collect::<BTreeSet<_>>();
    let mut files = BTreeMap::new();
    while !pending.is_empty() {
        let batch = std::mem::take(&mut pending)
            .into_iter()
            .filter(|path| !files.contains_key(path))
            .map(|path| {
                let package = inventory.package_for(&path);
                let edition = package
                    .and_then(|package| package.edition.parse::<Edition>().ok())
                    .unwrap_or(Edition::CURRENT);
                (
                    path,
                    edition,
                    package.map(|package| package.features.clone()),
                )
            })
            .collect::<Vec<_>>();
        let analyzed = pool.install(|| {
            batch
                .into_par_iter()
                .map(|(path, edition, features)| {
                    let result =
                        analyze_file(path.clone(), edition, &cfg_profile, features.as_ref());
                    (path, result)
                })
                .collect::<Vec<_>>()
        });

        for (path, result) in analyzed {
            match result {
                Ok(file) => {
                    for edge in &file.edges {
                        let target = canonical_or_original(&edge.target);
                        if !files.contains_key(&target) {
                            pending.insert(target);
                        }
                    }
                    files.insert(path, file);
                }
                Err(error) => {
                    if inventory.should_report(&path) {
                        inventory.diagnostics.push(Diagnostic {
                            severity: DiagnosticSeverity::Error,
                            path: Some(inventory.display_path(&path)),
                            message: format!("cannot read source: {error}"),
                        });
                    }
                }
            }
        }
    }

    let path_indices = files
        .keys()
        .enumerate()
        .map(|(index, path)| (path.clone(), index))
        .collect::<HashMap<_, _>>();
    let contexts = classify_module_graph(&inventory, &files, &path_indices);
    let selection = selection_report(&inventory, cli);
    Ok(build_report(inventory, files, contexts, selection))
}

fn classify_module_graph(
    inventory: &Inventory,
    files: &BTreeMap<PathBuf, LocalFile>,
    path_indices: &HashMap<PathBuf, usize>,
) -> Vec<Contexts> {
    let mut contexts = vec![Contexts::default(); files.len()];
    let mut queue = VecDeque::new();
    for seed in &inventory.targets {
        let path = canonical_or_original(&seed.path);
        if let Some(&index) = path_indices.get(&path)
            && contexts[index].merge(seed.contexts)
        {
            queue.push_back(index);
        }
    }

    if inventory.packages.is_empty() {
        for path in files.keys().filter(|path| {
            matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("lib.rs" | "main.rs")
            )
        }) {
            let index = path_indices[path];
            if contexts[index].merge(Contexts::production()) {
                queue.push_back(index);
            }
        }
    }

    propagate(files, path_indices, &mut contexts, &mut queue);

    if inventory.packages.is_empty() {
        for (index, context) in contexts.iter_mut().enumerate() {
            if !context.referenced && context.merge(Contexts::production()) {
                queue.push_back(index);
            }
        }
        propagate(files, path_indices, &mut contexts, &mut queue);
    }
    contexts
}

fn propagate(
    files: &BTreeMap<PathBuf, LocalFile>,
    path_indices: &HashMap<PathBuf, usize>,
    contexts: &mut [Contexts],
    queue: &mut VecDeque<usize>,
) {
    let files_by_index = files.values().collect::<Vec<_>>();
    while let Some(parent_index) = queue.pop_front() {
        let parent_context = contexts[parent_index];
        for edge in &files_by_index[parent_index].edges {
            let target = canonical_or_original(&edge.target);
            let Some(&child_index) = path_indices.get(&target) else {
                continue;
            };
            let incoming = parent_context.through(edge.gate);
            if contexts[child_index].merge(incoming) {
                queue.push_back(child_index);
            }
        }
    }
}

fn build_report(
    mut inventory: Inventory,
    files: BTreeMap<PathBuf, LocalFile>,
    contexts: Vec<Contexts>,
    selection: SelectionReport,
) -> Report {
    let mut project_buckets = empty_buckets();
    let mut project = SourceMetrics::default();
    let mut file_reports = Vec::with_capacity(files.len());
    let mut bytes = 0;

    for ((path, file), context) in files.into_iter().zip(contexts) {
        if !inventory.should_report(&path) {
            continue;
        }
        let display_path = inventory.display_path(&path);
        let package = inventory
            .package_for(&path)
            .map(|package| package.name.clone());
        let mut buckets = empty_buckets();
        let mut source = SourceMetrics::default();
        for (reachability, metrics) in reachability_states().zip(file.metrics) {
            buckets[context.classify(reachability) as usize]
                .source
                .add(metrics);
            source.add(metrics);
        }
        for bucket in &mut buckets {
            bucket.files = u64::from(!bucket.source.is_empty());
        }

        if !file.syntax_errors.is_empty() {
            inventory.diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                path: Some(display_path.clone()),
                message: format!(
                    "{} syntax error(s); first: {}",
                    file.syntax_errors.len(),
                    file.syntax_errors[0]
                ),
            });
        }
        for unresolved in &file.unresolved_edges {
            let role = context.classify(unresolved.gate);
            if !matches!(role, OutputRole::Inactive | OutputRole::Orphan) {
                inventory.diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warning,
                    path: Some(display_path.clone()),
                    message: unresolved.message.clone(),
                });
            }
        }

        for (project, file_bucket) in project_buckets.iter_mut().zip(&buckets) {
            project.add(file_bucket);
        }
        project.add(source);
        bytes += file.bytes;

        file_reports.push(FileReport {
            path: display_path,
            package,
            bytes: file.bytes,
            syntax_errors: file.syntax_errors.len() as u64,
            buckets: buckets
                .into_iter()
                .filter(|bucket| !bucket.is_empty())
                .collect(),
            total: source.lines,
            metrics: source.metrics,
        });
    }

    Report {
        root: inventory.root.to_string_lossy().into_owned(),
        selection,
        profile: inventory.profile,
        file_count: file_reports.len() as u64,
        bytes,
        files: file_reports,
        buckets: project_buckets
            .into_iter()
            .filter(|bucket| !bucket.is_empty())
            .collect(),
        total: project.lines,
        metrics: project.metrics,
        diagnostics: inventory.diagnostics,
    }
}

fn selection_report(inventory: &Inventory, cli: &FastCli) -> SelectionReport {
    SelectionReport::new(
        inventory
            .requested
            .iter()
            .map(|path| {
                let kind = if path.is_dir() {
                    SelectedPathKind::Directory
                } else {
                    SelectedPathKind::File
                };
                let relative = path.strip_prefix(&inventory.root).unwrap_or(path);
                let path = if relative.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    portable(relative)
                };
                SelectedPathReport { path, kind }
            })
            .collect(),
        cli.hidden,
        !cli.no_ignore,
    )
}

fn empty_buckets() -> [BucketReport; crate::model::OUTPUT_ROLE_COUNT] {
    std::array::from_fn(|index| BucketReport {
        role: OutputRole::ALL[index],
        ..BucketReport::default()
    })
}
