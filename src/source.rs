use std::{
    fs,
    path::{Path, PathBuf},
};

use ra_ap_syntax::{
    AstNode, AstToken, Edition, NodeOrToken, SourceFile, SyntaxKind, SyntaxNode, SyntaxToken,
    TextRange, WalkEvent,
    ast::{self, HasAttrs, HasLoopBody, HasName, VisibilityKind},
};

use crate::{
    cfg::{CfgProfile, PackageFeatures},
    model::{ComplexityMetrics, Reachability, SourceMetrics},
    paths::containing_directory,
};

const REACHABILITY_STATES: usize = 9;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContentKind {
    #[default]
    Blank,
    Comment,
    Code,
}

#[derive(Clone, Debug, Default)]
pub struct LocalLine {
    pub kind: ContentKind,
    pub doc: bool,
    pub significant_reach: Reachability,
    pub any_reach: Reachability,
    pub lexical_complexity: [u32; REACHABILITY_STATES],
}

impl LocalLine {
    pub fn reachability(&self) -> Reachability {
        if self.kind == ContentKind::Blank {
            self.any_reach
        } else {
            self.significant_reach
        }
    }
}

#[derive(Clone, Debug)]
pub struct SourceEdge {
    pub target: PathBuf,
    pub gate: Reachability,
}

#[derive(Clone, Debug)]
pub struct UnresolvedEdge {
    pub gate: Reachability,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct LocalFile {
    pub bytes: u64,
    pub metrics: [SourceMetrics; REACHABILITY_STATES],
    #[cfg(test)]
    pub lines: Vec<LocalLine>,
    pub syntax_errors: Vec<String>,
    pub edges: Vec<SourceEdge>,
    pub unresolved_edges: Vec<UnresolvedEdge>,
}

impl LocalFile {
    fn invalid_utf8(bytes: &[u8]) -> Self {
        let mut lines = raw_lines(bytes);
        for line in &mut lines {
            line.significant_reach = Reachability::BOTH;
            line.any_reach = Reachability::BOTH;
        }
        Self {
            bytes: bytes.len() as u64,
            metrics: aggregate_metrics(&lines, Default::default(), Default::default()),
            #[cfg(test)]
            lines,
            syntax_errors: vec![
                "source is not valid UTF-8; only raw line kinds are available".to_owned(),
            ],
            edges: Vec::new(),
            unresolved_edges: Vec::new(),
        }
    }
}

pub fn analyze_file(
    path: PathBuf,
    edition: Edition,
    profile: &CfgProfile,
    features: Option<&PackageFeatures>,
) -> Result<LocalFile, std::io::Error> {
    let bytes = fs::read(&path)?;
    let Ok(source) = std::str::from_utf8(&bytes) else {
        return Ok(LocalFile::invalid_utf8(&bytes));
    };
    let line_index = LineIndex::new(source);
    let parse = SourceFile::parse(source, edition);
    let syntax_errors = parse.errors().iter().map(ToString::to_string).collect();
    let tree = parse.tree();
    let root = tree.syntax();
    let mut lines = vec![LocalLine::default(); line_index.starts.len()];
    collect_tokens(root, &line_index, &mut lines, profile, features);
    let semantics = collect_semantics(&path, root, profile, features);

    Ok(LocalFile {
        bytes: bytes.len() as u64,
        metrics: aggregate_metrics(&lines, semantics.authored, semantics.declared_public),
        #[cfg(test)]
        lines,
        syntax_errors,
        edges: semantics.edges,
        unresolved_edges: semantics.unresolved_edges,
    })
}

fn aggregate_metrics(
    lines: &[LocalLine],
    authored: [ComplexityMetrics; REACHABILITY_STATES],
    declared_public: [u64; REACHABILITY_STATES],
) -> [SourceMetrics; REACHABILITY_STATES] {
    let mut metrics = [SourceMetrics::default(); REACHABILITY_STATES];
    for line in lines {
        let counts = &mut metrics[line.reachability().index()].lines;
        counts.physical += 1;
        match line.kind {
            ContentKind::Code => counts.code += 1,
            ContentKind::Comment => {
                counts.comments += 1;
                counts.docs += u64::from(line.doc);
            }
            ContentKind::Blank => counts.blank += 1,
        }
        for (metrics, count) in metrics.iter_mut().zip(line.lexical_complexity) {
            metrics.metrics.lexical_complexity += u64::from(count);
        }
    }
    for index in 0..REACHABILITY_STATES {
        metrics[index].metrics.add(authored[index]);
        metrics[index].declared_public = declared_public[index];
    }
    metrics
}

fn collect_tokens(
    root: &SyntaxNode,
    line_index: &LineIndex,
    lines: &mut [LocalLine],
    profile: &CfgProfile,
    features: Option<&PackageFeatures>,
) {
    let mut reachability = Vec::new();
    let mut previous_code_reach = None;
    for event in root.preorder_with_tokens() {
        match event {
            WalkEvent::Enter(NodeOrToken::Node(node)) => {
                let parent = reachability.last().copied().unwrap_or(Reachability::BOTH);
                reachability.push(parent.and(profile.node_gate(&node, features)));
            }
            WalkEvent::Leave(NodeOrToken::Node(_)) => {
                reachability.pop();
            }
            WalkEvent::Enter(NodeOrToken::Token(token)) => {
                let local = reachability.last().copied().unwrap_or(Reachability::BOTH);
                let effective = if token.kind() == SyntaxKind::COMMA {
                    previous_code_reach.unwrap_or(local)
                } else {
                    local
                };
                mark_token(lines, line_index, &token, effective);
                if !matches!(
                    token.kind(),
                    SyntaxKind::WHITESPACE | SyntaxKind::COMMENT | SyntaxKind::COMMA
                ) {
                    previous_code_reach = Some(local);
                }
            }
            WalkEvent::Leave(NodeOrToken::Token(_)) => {}
        }
    }
}

fn mark_token(
    lines: &mut [LocalLine],
    line_index: &LineIndex,
    token: &SyntaxToken,
    reachability: Reachability,
) {
    let (kind, is_doc) = match token.kind() {
        SyntaxKind::WHITESPACE => (ContentKind::Blank, false),
        SyntaxKind::COMMENT => (
            ContentKind::Comment,
            ast::Comment::cast(token.clone()).is_some_and(|comment| comment.is_doc()),
        ),
        _ => (ContentKind::Code, false),
    };
    let Some((first, last)) = line_index.covered_lines(token.text_range()) else {
        return;
    };
    for line in &mut lines[first..=last] {
        line.any_reach = line.any_reach.or(reachability);
        if kind != ContentKind::Blank {
            line.significant_reach = line.significant_reach.or(reachability);
        }
        if kind > line.kind {
            line.kind = kind;
            line.doc = is_doc;
        } else if kind == ContentKind::Comment && line.kind == ContentKind::Comment {
            line.doc |= is_doc;
        }
    }

    if kind == ContentKind::Code && is_complexity_token(token) {
        lines[first].lexical_complexity[reachability.index()] += 1;
    }
}

fn is_complexity_token(token: &SyntaxToken) -> bool {
    match token.text() {
        "for" | "if" | "while" | "loop" | "else" | "match" | "&&" | "||" | "!=" | "==" => true,
        "?" => next_significant_token(token).is_none_or(|next| next.text() != "Sized"),
        _ => false,
    }
}

fn next_significant_token(token: &SyntaxToken) -> Option<SyntaxToken> {
    let mut next = token.next_token();
    while next.as_ref().is_some_and(|token| token.kind().is_trivia()) {
        next = next.and_then(|token| token.next_token());
    }
    next
}

#[derive(Default)]
struct Semantics {
    authored: [ComplexityMetrics; REACHABILITY_STATES],
    declared_public: [u64; REACHABILITY_STATES],
    edges: Vec<SourceEdge>,
    unresolved_edges: Vec<UnresolvedEdge>,
}

struct SemanticVisitor<'a> {
    file: &'a Path,
    profile: &'a CfgProfile,
    features: Option<&'a PackageFeatures>,
    semantics: Semantics,
    bodies: Vec<u64>,
    body_cuts: Vec<usize>,
    matches: Vec<Reachability>,
}

impl SemanticVisitor<'_> {
    fn visit(&mut self, node: &SyntaxNode, parent: Reachability) {
        let local = parent.and(self.profile.node_gate(node, self.features));
        self.record_source(node, local);
        if node.kind() == SyntaxKind::TOKEN_TREE {
            return;
        }

        let body_count = body_count(node);
        for _ in 0..body_count {
            self.semantics.authored[local.index()].cyclomatic_authored += 1;
            self.bodies.push(0);
        }
        let boundary = is_declaration_boundary(node);
        if boundary {
            self.body_cuts.push(self.bodies.len());
        }

        if node.kind() == SyntaxKind::MATCH_ARM {
            let in_body = self.active_body().is_some();
            record_match_alternative(
                node,
                local,
                self.matches.last_mut(),
                in_body,
                &mut self.semantics.authored,
            );
        } else if let Some(nesting) = self.active_body().copied() {
            record_decision(node, local, nesting, &mut self.semantics.authored);
        }

        let match_expression = node.kind() == SyntaxKind::MATCH_EXPR;
        if match_expression {
            self.matches.push(Reachability::NEVER);
        }
        let nested = opens_nesting_region(node);
        if nested && let Some(body) = self.active_body() {
            *body += 1;
        }
        for child in node.children() {
            self.visit(&child, local);
        }
        if nested && let Some(body) = self.active_body() {
            *body -= 1;
        }
        if match_expression {
            self.matches.pop();
        }
        if boundary {
            self.body_cuts.pop();
        }
        self.bodies.truncate(self.bodies.len() - body_count);
    }

    fn active_body(&mut self) -> Option<&mut u64> {
        let active = self.bodies.len() > self.body_cuts.last().copied().unwrap_or(0);
        active.then(|| self.bodies.last_mut()).flatten()
    }
}

fn collect_semantics(
    file: &Path,
    root: &SyntaxNode,
    profile: &CfgProfile,
    features: Option<&PackageFeatures>,
) -> Semantics {
    let mut visitor = SemanticVisitor {
        file,
        profile,
        features,
        semantics: Semantics::default(),
        bodies: Vec::new(),
        body_cuts: Vec::new(),
        matches: Vec::new(),
    };
    visitor.visit(root, Reachability::BOTH);
    visitor.semantics
}

fn is_declaration_boundary(node: &SyntaxNode) -> bool {
    ast::Item::cast(node.clone()).is_some() || ast::ClosureExpr::cast(node.clone()).is_some()
}

fn body_count(node: &SyntaxNode) -> usize {
    usize::from(is_owner_body(node)) + usize::from(is_deferred_block(node))
}

fn is_owner_body(node: &SyntaxNode) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let body = match parent.kind() {
        SyntaxKind::FN => ast::Fn::cast(parent)
            .and_then(|owner| owner.body())
            .map(|body| body.syntax().clone()),
        SyntaxKind::CLOSURE_EXPR => ast::ClosureExpr::cast(parent)
            .and_then(|owner| owner.body())
            .map(|body| body.syntax().clone()),
        SyntaxKind::CONST => ast::Const::cast(parent)
            .and_then(|owner| owner.body())
            .map(|body| body.syntax().clone()),
        SyntaxKind::STATIC => ast::Static::cast(parent)
            .and_then(|owner| owner.body())
            .map(|body| body.syntax().clone()),
        _ => None,
    };
    body.as_ref().is_some_and(|body| body == node)
}

fn is_deferred_block(node: &SyntaxNode) -> bool {
    ast::BlockExpr::cast(node.clone())
        .is_some_and(|block| block.async_token().is_some() || block.const_token().is_some())
}

fn record_decision(
    node: &SyntaxNode,
    reachability: Reachability,
    nesting: u64,
    metrics: &mut [ComplexityMetrics; REACHABILITY_STATES],
) {
    let nested = nesting + 1;
    let contribution = match node.kind() {
        SyntaxKind::IF_EXPR
        | SyntaxKind::WHILE_EXPR
        | SyntaxKind::FOR_EXPR
        | SyntaxKind::LOOP_EXPR => Some((1, nested)),
        SyntaxKind::MATCH_EXPR => Some((0, nested)),
        SyntaxKind::MATCH_GUARD | SyntaxKind::TRY_EXPR => Some((1, 1)),
        SyntaxKind::BIN_EXPR
            if ast::BinExpr::cast(node.clone()).is_some_and(|expression| {
                matches!(
                    expression.op_kind(),
                    Some(ast::BinaryOp::LogicOp(ast::LogicOp::And | ast::LogicOp::Or))
                )
            }) =>
        {
            Some((1, 1))
        }
        SyntaxKind::LET_STMT
            if ast::LetStmt::cast(node.clone()).is_some_and(|statement| {
                statement
                    .let_else()
                    .is_some_and(|let_else| let_else.else_token().is_some())
            }) =>
        {
            Some((1, nested))
        }
        _ => None,
    };
    if let Some((cyclomatic, cognitive)) = contribution {
        let total = &mut metrics[reachability.index()];
        total.cyclomatic_authored += cyclomatic;
        total.cognitive_authored += cognitive;
    }
}

fn record_match_alternative(
    node: &SyntaxNode,
    reachability: Reachability,
    preceding_arms: Option<&mut Reachability>,
    in_body: bool,
    metrics: &mut [ComplexityMetrics; REACHABILITY_STATES],
) {
    let Some(preceding_arms) = preceding_arms else {
        return;
    };
    if ast::MatchArm::cast(node.clone()).is_none_or(|arm| arm.fat_arrow_token().is_none()) {
        return;
    }
    let alternative = reachability.and(*preceding_arms);
    *preceding_arms = (*preceding_arms).or(reachability);
    if in_body && alternative != Reachability::NEVER {
        metrics[alternative.index()].cyclomatic_authored += 1;
    }
}

fn opens_nesting_region(node: &SyntaxNode) -> bool {
    if matches!(node.kind(), SyntaxKind::MATCH_ARM | SyntaxKind::LET_ELSE) {
        return true;
    }
    let Some(block) = ast::BlockExpr::cast(node.clone()) else {
        return false;
    };
    let Some(parent) = node.parent() else {
        return false;
    };
    if let Some(expression) = ast::IfExpr::cast(parent.clone()) {
        return expression
            .then_branch()
            .is_some_and(|branch| branch.syntax() == block.syntax())
            || matches!(
                expression.else_branch(),
                Some(ast::ElseBranch::Block(branch)) if branch.syntax() == block.syntax()
            );
    }
    let loop_body = match parent.kind() {
        SyntaxKind::WHILE_EXPR => ast::WhileExpr::cast(parent).and_then(|owner| owner.loop_body()),
        SyntaxKind::FOR_EXPR => ast::ForExpr::cast(parent).and_then(|owner| owner.loop_body()),
        SyntaxKind::LOOP_EXPR => ast::LoopExpr::cast(parent).and_then(|owner| owner.loop_body()),
        _ => None,
    };
    loop_body.is_some_and(|body| body.syntax() == block.syntax())
}

impl SemanticVisitor<'_> {
    fn record_source(&mut self, node: &SyntaxNode, gate: Reachability) {
        if ast::Visibility::cast(node.clone())
            .is_some_and(|visibility| matches!(visibility.kind(), VisibilityKind::Pub))
        {
            self.semantics.declared_public[gate.index()] += 1;
        }

        if let Some(module) = ast::Module::cast(node.clone())
            && module.semicolon_token().is_some()
        {
            match resolve_module(self.file, &module) {
                Some(target) => self.semantics.edges.push(SourceEdge { target, gate }),
                None => self.semantics.unresolved_edges.push(UnresolvedEdge {
                    gate,
                    message: format!(
                        "cannot resolve out-of-line module {}",
                        module
                            .name()
                            .map_or("<missing name>".to_owned(), |name| name.text().to_string())
                    ),
                }),
            }
        }

        let Some(call) = ast::MacroCall::cast(node.clone()) else {
            return;
        };
        if call
            .path()
            .and_then(|path| path.as_single_name_ref())
            .is_none_or(|name| name.text() != "include")
        {
            return;
        }
        let Some(included) = literal_macro_string(&call) else {
            self.semantics.unresolved_edges.push(UnresolvedEdge {
                gate,
                message: "non-literal include! source is unresolved".to_owned(),
            });
            return;
        };
        let target = containing_directory(self.file).join(included);
        if target.is_file() {
            self.semantics.edges.push(SourceEdge { target, gate });
        } else {
            self.semantics.unresolved_edges.push(UnresolvedEdge {
                gate,
                message: format!("include! source does not exist: {}", target.display()),
            });
        }
    }
}

fn resolve_module(file: &Path, module: &ast::Module) -> Option<PathBuf> {
    let module_name = module.name()?.text().to_string();
    let parent = containing_directory(file);
    let mut base = match file.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => file
            .file_stem()
            .map_or_else(|| parent.to_path_buf(), |stem| parent.join(stem)),
    };
    let inline_ancestors = module
        .syntax()
        .ancestors()
        .skip(1)
        .filter_map(ast::Module::cast)
        .filter(|ancestor| ancestor.item_list().is_some())
        .filter_map(|ancestor| ancestor.name().map(|name| name.text().to_string()))
        .collect::<Vec<_>>();
    for ancestor in inline_ancestors.iter().rev() {
        base.push(ancestor);
    }

    if let Some(path) = attribute_string(module.attrs(), "path") {
        let sibling = parent.join(&path);
        let nested = base.join(path);
        return if inline_ancestors.is_empty() {
            [sibling, nested]
        } else {
            [nested, sibling]
        }
        .into_iter()
        .find(|candidate| candidate.is_file());
    }

    [
        base.join(format!("{module_name}.rs")),
        base.join(&module_name).join("mod.rs"),
        parent.join(format!("{module_name}.rs")),
        parent.join(module_name).join("mod.rs"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn attribute_string(attributes: impl Iterator<Item = ast::Attr>, name: &str) -> Option<String> {
    attributes
        .filter_map(|attribute| attribute.meta())
        .find_map(|meta| {
            let ast::Meta::KeyValueMeta(key_value) = meta else {
                return None;
            };
            if key_value.path()?.as_single_name_ref()?.text() != name {
                return None;
            }
            let ast::Expr::Literal(literal) = key_value.expr()? else {
                return None;
            };
            let ast::LiteralKind::String(literal) = literal.kind() else {
                return None;
            };
            literal.value().ok().map(std::borrow::Cow::into_owned)
        })
}

fn literal_macro_string(call: &ast::MacroCall) -> Option<String> {
    let tree = call.token_tree()?;
    let mut payload = ast::TokenTreeChildren::new(&tree);
    let literal = ast::String::cast(payload.next()?.into_token()?)?;
    payload
        .next()
        .is_none()
        .then(|| literal.value().ok().map(std::borrow::Cow::into_owned))
        .flatten()
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let starts = if source.is_empty() {
            Vec::new()
        } else {
            std::iter::once(0)
                .chain(
                    source
                        .match_indices('\n')
                        .map(|(index, _)| index + 1)
                        .filter(|index| *index < source.len()),
                )
                .collect()
        };
        Self { starts }
    }

    fn covered_lines(&self, range: TextRange) -> Option<(usize, usize)> {
        if self.starts.is_empty() || range.is_empty() {
            return None;
        }
        let start = usize::from(range.start());
        let end = usize::from(range.end()).saturating_sub(1);
        Some((self.line_at(start), self.line_at(end)))
    }

    fn line_at(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    }
}

fn raw_lines(bytes: &[u8]) -> Vec<LocalLine> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .strip_suffix(b"\n")
        .unwrap_or(bytes)
        .split(|byte| *byte == b'\n')
        .map(raw_line)
        .collect()
}

fn raw_line(bytes: &[u8]) -> LocalLine {
    LocalLine {
        kind: if bytes.iter().all(u8::is_ascii_whitespace) {
            ContentKind::Blank
        } else {
            ContentKind::Code
        },
        ..LocalLine::default()
    }
}

pub fn reachability_states() -> impl Iterator<Item = Reachability> {
    (0..REACHABILITY_STATES).map(Reachability::from_index)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn analyze(source: &str) -> LocalFile {
        let directory = std::env::temp_dir().join(format!(
            "rot-source-test-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&directory).expect("create fixture directory");
        let path = directory.join("fixture.rs");
        fs::write(&path, source).expect("write fixture");
        analyze_file(
            path,
            Edition::CURRENT,
            &CfgProfile::new(HashSet::new(), HashSet::new(), HashSet::new(), &[]),
            Some(&PackageFeatures::default()),
        )
        .expect("analyze fixture")
    }

    #[test]
    fn rust_lexer_handles_nested_comments_raw_strings_and_final_line() {
        let file = analyze("/* outer\n/* if */\n*/\nlet raw = r###\"// while ?\"###;\nlast");
        assert_eq!(file.lines.len(), 5);
        assert_eq!(file.lines[0].kind, ContentKind::Comment);
        assert_eq!(file.lines[1].kind, ContentKind::Comment);
        assert_eq!(file.lines[3].kind, ContentKind::Code);
        assert_eq!(file.lines[4].kind, ContentKind::Code);
        assert_eq!(
            file.lines
                .iter()
                .flat_map(|line| line.lexical_complexity)
                .sum::<u32>(),
            0
        );
    }

    #[test]
    fn visually_blank_lines_inside_tokens_keep_their_content_kind() {
        let file = analyze("let text = \"first\n\nthird\";\n/* comment\n\nend */");
        assert_eq!(file.lines.len(), 6);
        assert_eq!(file.lines[1].kind, ContentKind::Code);
        assert_eq!(file.lines[4].kind, ContentKind::Comment);
    }

    #[test]
    fn quote_character_does_not_hide_following_lines() {
        let file = analyze("const QUOTE: char = '\"';\n\n// still comment");
        assert!(file.syntax_errors.is_empty());
        assert_eq!(file.lines[0].kind, ContentKind::Code);
        assert_eq!(file.lines[1].kind, ContentKind::Blank);
        assert_eq!(file.lines[2].kind, ContentKind::Comment);
    }

    #[test]
    fn nested_cfg_attributes_gate_fields_and_statements() {
        let file = analyze(
            "struct Example {\n    #[cfg(test)]\n    test_only: u8,\n}\nfn run() {\n    #[cfg(test)]\n    let _test_only = 1;\n}",
        );
        assert_eq!(file.lines[2].reachability(), Reachability::TEST);
        assert_eq!(file.lines[6].reachability(), Reachability::TEST);
    }

    #[test]
    fn include_requires_one_literal_string() {
        fn included(source: &str) -> Option<String> {
            let parse = SourceFile::parse(source, Edition::CURRENT);
            let call = parse
                .syntax_node()
                .descendants()
                .find_map(ast::MacroCall::cast)
                .unwrap();
            literal_macro_string(&call)
        }

        assert_eq!(
            included("include!(\"generated.rs\");").as_deref(),
            Some("generated.rs")
        );
        assert_eq!(
            included("include!(concat!(env!(\"OUT_DIR\"), \"/bindings.rs\"));"),
            None
        );
    }

    #[test]
    fn scc_style_complexity_uses_rust_tokens() {
        let source = "fn f(x: bool) { if x && x == true { loop {} } else { x?; } }";
        let file = analyze(source);
        assert_eq!(
            file.lines
                .iter()
                .flat_map(|line| line.lexical_complexity)
                .sum::<u32>(),
            6
        );
    }

    fn authored_metrics(file: &LocalFile) -> ComplexityMetrics {
        file.metrics
            .iter()
            .fold(ComplexityMetrics::default(), |mut total, source| {
                total.add(source.metrics);
                total
            })
    }

    #[test]
    fn declarations_without_bodies_do_not_create_cyclomatic_bases() {
        let file = analyze("trait Example { fn required(&self); fn defaulted(&self) {} }");
        let metrics = authored_metrics(&file);
        assert_eq!(metrics.cyclomatic_authored, 1);
        assert_eq!(metrics.cognitive_authored, 0);
    }

    #[test]
    fn declared_public_counts_only_explicit_unrestricted_visibility() {
        let file = analyze(
            r#"
mod hidden {
    pub fn nested() {}
    pub(super) fn parent_only() {}
    pub(self) fn self_only() {}
    pub(in crate) fn crate_only() {}
}
pub trait PublicTrait { fn implicit(); }
pub enum PublicEnum { ImplicitVariant }
pub use self::{PublicEnum as One, PublicTrait as Two};
#[macro_export]
macro_rules! exported { () => {}; }
struct Private;
impl Private { pub fn method() {} }
pub struct Record { pub field: usize, pub(crate) restricted: usize }
#[cfg(test)]
pub fn test_only() {}
"#,
        );
        assert_eq!(file.metrics[Reachability::BOTH.index()].declared_public, 7);
        assert_eq!(file.metrics[Reachability::TEST.index()].declared_public, 1);
        assert_eq!(
            file.metrics
                .into_iter()
                .map(|metrics| metrics.declared_public)
                .sum::<u64>(),
            8
        );
    }

    #[test]
    fn executable_body_ownership_excludes_bare_anonymous_consts_in_signatures() {
        let file = analyze("fn signature(_: [(); { if true { 1 } else { 0 } }]) {}");
        let metrics = authored_metrics(&file);
        assert_eq!(metrics.cyclomatic_authored, 1);
        assert_eq!(metrics.cognitive_authored, 0);

        let explicit = analyze("fn signature(_: [(); const { if true { 1 } else { 0 } }]) {}");
        let metrics = authored_metrics(&explicit);
        assert_eq!(metrics.cyclomatic_authored, 3);
        assert_eq!(metrics.cognitive_authored, 1);

        let nested = analyze("fn outer() { fn inner(_: [(); { if true { 1 } else { 0 } }]) {} }");
        let metrics = authored_metrics(&nested);
        assert_eq!(metrics.cyclomatic_authored, 2);
        assert_eq!(metrics.cognitive_authored, 0);

        let closure_initializer =
            analyze("const F: fn([(); 1]) = |_: [(); { if true { 1 } else { 0 } }]| {};");
        let metrics = authored_metrics(&closure_initializer);
        assert_eq!(metrics.cyclomatic_authored, 2);
        assert_eq!(metrics.cognitive_authored, 0);
    }

    #[test]
    fn authored_decisions_follow_rust_control_flow_instead_of_lexical_tokens() {
        let file = analyze(
            r#"
fn decisions(value: Option<bool>) -> Option<()> {
    let Some(flag) = value else { return None; };
    if flag && true {}
    while false {}
    for _ in 0..1 {}
    loop { break; }
    match flag {
        true if flag || false => {}
        false => {}
    }
    Some(())?;
    Some(())
}
"#,
        );
        let metrics = authored_metrics(&file);
        assert_eq!(metrics.cyclomatic_authored, 11);
        assert_eq!(metrics.cognitive_authored, 10);
    }

    #[test]
    fn cognitive_nesting_tracks_branches_and_resets_for_nested_bodies() {
        let nested = analyze(
            r#"
fn nested(value: bool) {
    if value {
        while value {
            match value {
                true => { if value {} }
                false => {}
            }
        }
    }
}
"#,
        );
        let metrics = authored_metrics(&nested);
        assert_eq!(metrics.cyclomatic_authored, 5);
        assert_eq!(metrics.cognitive_authored, 10);

        let closures = analyze(
            r#"
fn outer() {
    let _first = || {
        if true {
            let _second = || { while true {} };
        }
    };
}
"#,
        );
        let metrics = authored_metrics(&closures);
        assert_eq!(metrics.cyclomatic_authored, 5);
        assert_eq!(metrics.cognitive_authored, 2);

        let else_if = authored_metrics(&analyze(
            "fn chain(a: bool, b: bool) { if a {} else if b {} }",
        ));
        assert_eq!(else_if.cyclomatic_authored, 3);
        assert_eq!(else_if.cognitive_authored, 2);
    }

    #[test]
    fn evaluated_and_deferred_blocks_are_independent_bodies() {
        let file = analyze(
            r#"
const VALUE: usize = if true { 1 } else { 2 };
static FLAG: bool = true;
fn blocks() {
    let _future = async { if true {} };
    let _constant = const { loop { break; } };
}
"#,
        );
        let metrics = authored_metrics(&file);
        assert_eq!(metrics.cyclomatic_authored, 8);
        assert_eq!(metrics.cognitive_authored, 3);
    }

    #[test]
    fn cfg_filtered_match_alternatives_use_the_first_active_arm() {
        let file = analyze(
            r#"
fn classify(value: u8) {
    match value {
        #[cfg(test)]
        0 => {}
        1 => {}
        #[cfg(test)]
        2 => {}
    }
}
"#,
        );
        let test_alternatives = file.metrics[Reachability::TEST.index()].metrics;
        assert_eq!(test_alternatives.cyclomatic_authored, 2);
        assert_eq!(test_alternatives.cognitive_authored, 0);
    }

    #[test]
    fn macro_token_trees_are_lexical_but_not_authored_control_flow() {
        let file = analyze("fn wrapper() { opaque! { if true && true { loop {} } } }");
        let lexical = file
            .lines
            .iter()
            .flat_map(|line| line.lexical_complexity)
            .sum::<u32>();
        let metrics = authored_metrics(&file);
        assert_eq!(lexical, 2);
        assert_eq!(metrics.cyclomatic_authored, 1);
        assert_eq!(metrics.cognitive_authored, 0);
    }

    #[test]
    fn recovered_typed_nodes_keep_authored_metrics() {
        let file = analyze("fn broken() { if true {} let = ; }");
        assert!(!file.syntax_errors.is_empty());
        let metrics = authored_metrics(&file);
        assert_eq!(metrics.cyclomatic_authored, 2);
        assert_eq!(metrics.cognitive_authored, 1);
    }
}
