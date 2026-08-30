use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use ra_ap_syntax::Edition;
use rayon::{ThreadPoolBuilder, prelude::*};

use crate::{
    cfg::CfgProfile,
    cli::FastCli,
    model::{
        BucketReport, ComplexityMetrics, Contexts, Diagnostic, DiagnosticSeverity, FileReport,
        LineCounts, OutputRole, Report, SelectedPathKind, SelectedPathReport, SelectionReport,
    },
    source::{ContentKind, LocalFile, analyze_file, reachability_states},
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

    let paths = files.keys().cloned().collect::<Vec<_>>();
    let path_indices = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (path.clone(), index))
        .collect::<HashMap<_, _>>();
    let contexts = classify_module_graph(&inventory, &files, &path_indices);
    let selection = selection_report(&inventory, cli);
    build_report(inventory, files, paths, contexts, selection)
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
        let conventional_roots = files
            .keys()
            .filter(|path| {
                matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("lib.rs" | "main.rs")
                )
            })
            .filter_map(|path| path_indices.get(path).copied())
            .collect::<Vec<_>>();
        for index in conventional_roots {
            if contexts[index].merge(Contexts::seed(
                crate::model::TargetRole::Production,
                crate::model::Reachability::BOTH,
            )) {
                queue.push_back(index);
            }
        }
    }

    propagate(files, path_indices, &mut contexts, &mut queue);

    if inventory.packages.is_empty() {
        for (index, context) in contexts.iter_mut().enumerate() {
            if !context.referenced
                && context.merge(Contexts::seed(
                    crate::model::TargetRole::Production,
                    crate::model::Reachability::BOTH,
                ))
            {
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
    paths: Vec<PathBuf>,
    contexts: Vec<Contexts>,
    selection: SelectionReport,
) -> Result<Report> {
    let mut project_buckets = empty_buckets();
    let mut project_total = LineCounts::default();
    let mut project_metrics = ComplexityMetrics::default();
    let mut file_reports = Vec::with_capacity(files.len());
    let mut bytes = 0;

    for (index, path) in paths.iter().enumerate() {
        if !inventory.should_report(path) {
            continue;
        }
        let file = &files[path];
        let context = contexts[index];
        let display_path = inventory.display_path(path);
        let package = inventory
            .package_for(path)
            .map(|package| package.name.clone());
        let mut buckets = empty_buckets();
        let mut total = LineCounts::default();
        let mut metrics = ComplexityMetrics::default();

        for line in &file.lines {
            let role = context.classify(line.reachability());
            add_line(&mut buckets[role as usize].lines, line.kind, line.doc);
            add_line(&mut total, line.kind, line.doc);

            for (reachability, count) in reachability_states().zip(line.lexical_complexity) {
                if count == 0 {
                    continue;
                }
                let event_role = context.classify(reachability);
                let contribution = ComplexityMetrics {
                    lexical_complexity: u64::from(count),
                    ..ComplexityMetrics::default()
                };
                buckets[event_role as usize].metrics.add(contribution);
                metrics.add(contribution);
            }
        }

        for &fact in &file.authored_facts {
            let role = context.classify(fact.reachability());
            let contribution = fact.metrics();
            buckets[role as usize].metrics.add(contribution);
            metrics.add(contribution);
        }

        for (reachability, count) in reachability_states().zip(file.declared_public) {
            if count == 0 {
                continue;
            }
            let role = context.classify(reachability);
            buckets[role as usize].declared_public += u64::from(count);
        }
        for bucket in &mut buckets {
            if bucket.lines.physical > 0
                || bucket.metrics.lexical_complexity > 0
                || bucket.metrics.cyclomatic_authored > 0
                || bucket.metrics.cognitive_authored > 0
                || bucket.declared_public > 0
            {
                bucket.files = 1;
            }
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
            project.files += file_bucket.files;
            project.lines.add(file_bucket.lines);
            project.metrics.add(file_bucket.metrics);
            project.declared_public += file_bucket.declared_public;
        }
        project_total.add(total);
        project_metrics.add(metrics);
        bytes += file.bytes;

        file_reports.push(FileReport {
            path: display_path,
            package,
            bytes: file.bytes,
            syntax_errors: file.syntax_errors.len() as u64,
            buckets: buckets.into_iter().filter(nonempty_bucket).collect(),
            total,
            metrics,
        });
    }

    Ok(Report {
        schema_version: 3,
        root: inventory.root.to_string_lossy().into_owned(),
        selection,
        profile: inventory.profile,
        file_count: file_reports.len() as u64,
        bytes,
        files: file_reports,
        buckets: project_buckets
            .into_iter()
            .filter(nonempty_bucket)
            .collect(),
        total: project_total,
        metrics: project_metrics,
        diagnostics: inventory.diagnostics,
    })
}

fn selection_report(inventory: &Inventory, cli: &FastCli) -> SelectionReport {
    let mut paths = inventory
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
                relative.to_string_lossy().replace('\\', "/")
            };
            SelectedPathReport { path, kind }
        })
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
    });
    paths.dedup();
    SelectionReport {
        paths,
        include_hidden: cli.hidden,
        respect_ignores: !cli.no_ignore,
        ignore_boundary: "path",
    }
}

fn empty_buckets() -> [BucketReport; crate::model::OUTPUT_ROLE_COUNT] {
    std::array::from_fn(|index| BucketReport {
        role: OutputRole::ALL[index].key().to_owned(),
        ..BucketReport::default()
    })
}

fn nonempty_bucket(bucket: &BucketReport) -> bool {
    bucket.files > 0
        || bucket.lines.physical > 0
        || bucket.metrics.lexical_complexity > 0
        || bucket.metrics.cyclomatic_authored > 0
        || bucket.metrics.cognitive_authored > 0
        || bucket.declared_public > 0
}

fn add_line(counts: &mut LineCounts, kind: ContentKind, doc: bool) {
    counts.physical += 1;
    match kind {
        ContentKind::Code => counts.code += 1,
        ContentKind::Comment => {
            counts.comments += 1;
            counts.docs += u64::from(doc);
        }
        ContentKind::Blank => counts.blank += 1,
    }
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
