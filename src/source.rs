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
    cfg::{CfgProfile, PackageFeatures, reachability_for_node},
    model::{ComplexityMetrics, Reachability},
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyKind {
    Function,
    Closure,
    Const,
    Static,
    AsyncBlock,
    ConstBlock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecisionKind {
    Conditional,
    Loop,
    Match,
    MatchAlternative,
    Guard,
    ShortCircuit,
    Try,
    LetElse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthoredBody {
    kind: BodyKind,
    range: TextRange,
    reachability: Reachability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuthoredDecision {
    kind: DecisionKind,
    range: TextRange,
    reachability: Reachability,
    nesting: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthoredFact {
    Body(AuthoredBody),
    Decision(AuthoredDecision),
}

impl AuthoredFact {
    pub fn reachability(self) -> Reachability {
        match self {
            Self::Body(body) => body.reachability,
            Self::Decision(decision) => decision.reachability,
        }
    }

    pub fn metrics(self) -> ComplexityMetrics {
        match self {
            Self::Body(body) => {
                debug_assert!(!body.range.is_empty());
                let cyclomatic_authored = match body.kind {
                    BodyKind::Function
                    | BodyKind::Closure
                    | BodyKind::Const
                    | BodyKind::Static
                    | BodyKind::AsyncBlock
                    | BodyKind::ConstBlock => 1,
                };
                ComplexityMetrics {
                    cyclomatic_authored,
                    ..ComplexityMetrics::default()
                }
            }
            Self::Decision(decision) => {
                debug_assert!(!decision.range.is_empty());
                let (cyclomatic_authored, cognitive_authored) = match decision.kind {
                    DecisionKind::Conditional | DecisionKind::Loop | DecisionKind::LetElse => (
                        1,
                        decision
                            .nesting
                            .checked_add(1)
                            .expect("syntax nesting cannot exhaust u64"),
                    ),
                    DecisionKind::Match => (
                        0,
                        decision
                            .nesting
                            .checked_add(1)
                            .expect("syntax nesting cannot exhaust u64"),
                    ),
                    DecisionKind::MatchAlternative => (1, 0),
                    DecisionKind::Guard | DecisionKind::ShortCircuit | DecisionKind::Try => (1, 1),
                };
                ComplexityMetrics {
                    cyclomatic_authored,
                    cognitive_authored,
                    ..ComplexityMetrics::default()
                }
            }
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
    pub lines: Vec<LocalLine>,
    pub syntax_errors: Vec<String>,
    pub edges: Vec<SourceEdge>,
    pub unresolved_edges: Vec<UnresolvedEdge>,
    pub authored_facts: Vec<AuthoredFact>,
    pub declared_public: [u32; REACHABILITY_STATES],
}

impl LocalFile {
    fn invalid_utf8(bytes: Vec<u8>) -> Self {
        let mut lines = raw_lines(&bytes);
        for line in &mut lines {
            line.significant_reach = Reachability::BOTH;
            line.any_reach = Reachability::BOTH;
        }
        Self {
            bytes: bytes.len() as u64,
            lines,
            syntax_errors: vec![
                "source is not valid UTF-8; only raw line kinds are available".to_owned(),
            ],
            edges: Vec::new(),
            unresolved_edges: Vec::new(),
            authored_facts: Vec::new(),
            declared_public: [0; REACHABILITY_STATES],
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
    Ok(analyze_bytes(path, &bytes, edition, profile, features))
}

pub fn analyze_bytes(
    path: PathBuf,
    bytes: &[u8],
    edition: Edition,
    profile: &CfgProfile,
    features: Option<&PackageFeatures>,
) -> LocalFile {
    let source = match std::str::from_utf8(bytes) {
        Ok(source) => source,
        Err(_) => return LocalFile::invalid_utf8(bytes.to_vec()),
    };
    let line_index = LineIndex::new(source);
    let parse = SourceFile::parse(source, edition);
    let syntax_errors = parse
        .errors()
        .into_iter()
        .map(|error| error.to_string())
        .collect();
    let tree = parse.tree();
    let root = tree.syntax();
    let mut lines = vec![LocalLine::default(); line_index.len()];
    collect_tokens(root, &line_index, &mut lines, profile, features);
    let authored_facts = collect_authored_complexity(root, profile, features);
    let declared_public = collect_declared_public(root, profile, features);
    let (edges, unresolved_edges) = collect_edges(&path, root, profile, features);

    LocalFile {
        bytes: bytes.len() as u64,
        lines,
        syntax_errors,
        edges,
        unresolved_edges,
        authored_facts,
        declared_public,
    }
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
    while next
        .as_ref()
        .is_some_and(|token| matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT))
    {
        next = next.and_then(|token| token.next_token());
    }
    next
}

struct BodyState {
    owner: SyntaxNode,
    nesting: u64,
}

struct BodyCut {
    owner: SyntaxNode,
    body_depth: usize,
}

struct MatchState {
    owner: SyntaxNode,
    preceding_arms: Reachability,
}

fn collect_authored_complexity(
    root: &SyntaxNode,
    profile: &CfgProfile,
    features: Option<&PackageFeatures>,
) -> Vec<AuthoredFact> {
    let mut facts = Vec::new();
    let mut reachability = Vec::new();
    let mut bodies: Vec<BodyState> = Vec::new();
    let mut body_cuts: Vec<BodyCut> = Vec::new();
    let mut matches: Vec<MatchState> = Vec::new();
    let mut walk = root.preorder();

    while let Some(event) = walk.next() {
        match event {
            WalkEvent::Enter(node) => {
                let parent = reachability.last().copied().unwrap_or(Reachability::BOTH);
                let local = parent.and(profile.node_gate(&node, features));
                reachability.push(local);

                if node.kind() == SyntaxKind::TOKEN_TREE {
                    walk.skip_subtree();
                    continue;
                }

                for kind in body_kinds(&node).into_iter().flatten() {
                    let range = node.text_range();
                    if !range.is_empty() {
                        facts.push(AuthoredFact::Body(AuthoredBody {
                            kind,
                            range,
                            reachability: local,
                        }));
                        bodies.push(BodyState {
                            owner: node.clone(),
                            nesting: 0,
                        });
                    }
                }

                if active_body(&bodies, &body_cuts).is_some() && is_declaration_boundary(&node) {
                    body_cuts.push(BodyCut {
                        owner: node.clone(),
                        body_depth: bodies.len(),
                    });
                }

                if node.kind() == SyntaxKind::MATCH_ARM {
                    record_match_alternative(
                        &node,
                        local,
                        matches.last_mut(),
                        active_body(&bodies, &body_cuts),
                        &mut facts,
                    );
                } else if let Some(body) = active_body(&bodies, &body_cuts) {
                    record_decision(&node, local, body.nesting, &mut facts);
                }

                if node.kind() == SyntaxKind::MATCH_EXPR {
                    matches.push(MatchState {
                        owner: node.clone(),
                        preceding_arms: Reachability::NEVER,
                    });
                }

                if opens_nesting_region(&node)
                    && let Some(body) = active_body_mut(&mut bodies, &body_cuts)
                {
                    body.nesting = body
                        .nesting
                        .checked_add(1)
                        .expect("syntax nesting cannot exhaust u64");
                }
            }
            WalkEvent::Leave(node) => {
                if opens_nesting_region(&node)
                    && let Some(body) = active_body_mut(&mut bodies, &body_cuts)
                {
                    body.nesting = body
                        .nesting
                        .checked_sub(1)
                        .expect("nesting-region traversal must be balanced");
                }
                if matches.last().is_some_and(|state| state.owner == node) {
                    matches.pop();
                }
                while bodies.last().is_some_and(|body| body.owner == node) {
                    bodies.pop();
                }
                if body_cuts.last().is_some_and(|cut| cut.owner == node) {
                    body_cuts.pop();
                }
                reachability.pop();
            }
        }
    }

    debug_assert!(bodies.is_empty());
    debug_assert!(body_cuts.is_empty());
    debug_assert!(matches.is_empty());
    debug_assert!(reachability.is_empty());
    facts
}

fn active_body<'a>(bodies: &'a [BodyState], cuts: &[BodyCut]) -> Option<&'a BodyState> {
    let cut_depth = cuts.last().map_or(0, |cut| cut.body_depth);
    (bodies.len() > cut_depth).then(|| bodies.last()).flatten()
}

fn active_body_mut<'a>(bodies: &'a mut [BodyState], cuts: &[BodyCut]) -> Option<&'a mut BodyState> {
    let cut_depth = cuts.last().map_or(0, |cut| cut.body_depth);
    (bodies.len() > cut_depth)
        .then(|| bodies.last_mut())
        .flatten()
}

fn is_declaration_boundary(node: &SyntaxNode) -> bool {
    ast::Item::cast(node.clone()).is_some() || ast::ClosureExpr::cast(node.clone()).is_some()
}

fn body_kinds(node: &SyntaxNode) -> [Option<BodyKind>; 2] {
    [owner_body_kind(node), block_body_kind(node)]
}

fn owner_body_kind(node: &SyntaxNode) -> Option<BodyKind> {
    let parent = node.parent()?;
    if ast::Fn::cast(parent.clone())
        .is_some_and(|function| function.body().is_some_and(|body| body.syntax() == node))
    {
        return Some(BodyKind::Function);
    }
    if ast::ClosureExpr::cast(parent.clone())
        .is_some_and(|closure| closure.body().is_some_and(|body| body.syntax() == node))
    {
        return Some(BodyKind::Closure);
    }
    if ast::Const::cast(parent.clone())
        .is_some_and(|constant| constant.body().is_some_and(|body| body.syntax() == node))
    {
        return Some(BodyKind::Const);
    }
    ast::Static::cast(parent)
        .is_some_and(|static_item| static_item.body().is_some_and(|body| body.syntax() == node))
        .then_some(BodyKind::Static)
}

fn block_body_kind(node: &SyntaxNode) -> Option<BodyKind> {
    let block = ast::BlockExpr::cast(node.clone())?;
    if block.async_token().is_some() {
        Some(BodyKind::AsyncBlock)
    } else if block.const_token().is_some() {
        Some(BodyKind::ConstBlock)
    } else {
        None
    }
}

fn record_decision(
    node: &SyntaxNode,
    reachability: Reachability,
    nesting: u64,
    facts: &mut Vec<AuthoredFact>,
) {
    let decision = match node.kind() {
        SyntaxKind::IF_EXPR => ast::IfExpr::cast(node.clone())
            .and_then(|expression| expression.if_token())
            .map(|token| (DecisionKind::Conditional, token)),
        SyntaxKind::WHILE_EXPR => ast::WhileExpr::cast(node.clone())
            .and_then(|expression| expression.while_token())
            .map(|token| (DecisionKind::Loop, token)),
        SyntaxKind::FOR_EXPR => ast::ForExpr::cast(node.clone())
            .and_then(|expression| expression.for_token())
            .map(|token| (DecisionKind::Loop, token)),
        SyntaxKind::LOOP_EXPR => ast::LoopExpr::cast(node.clone())
            .and_then(|expression| expression.loop_token())
            .map(|token| (DecisionKind::Loop, token)),
        SyntaxKind::MATCH_EXPR => ast::MatchExpr::cast(node.clone())
            .and_then(|expression| expression.match_token())
            .map(|token| (DecisionKind::Match, token)),
        SyntaxKind::MATCH_GUARD => ast::MatchGuard::cast(node.clone())
            .and_then(|guard| guard.if_token())
            .map(|token| (DecisionKind::Guard, token)),
        SyntaxKind::BIN_EXPR => ast::BinExpr::cast(node.clone()).and_then(|expression| {
            matches!(
                expression.op_kind(),
                Some(ast::BinaryOp::LogicOp(ast::LogicOp::And | ast::LogicOp::Or))
            )
            .then(|| expression.op_token())
            .flatten()
            .map(|token| (DecisionKind::ShortCircuit, token))
        }),
        SyntaxKind::TRY_EXPR => ast::TryExpr::cast(node.clone())
            .and_then(|expression| expression.question_mark_token())
            .map(|token| (DecisionKind::Try, token)),
        SyntaxKind::LET_STMT => ast::LetStmt::cast(node.clone())
            .and_then(|statement| statement.let_else())
            .and_then(|let_else| let_else.else_token())
            .map(|token| (DecisionKind::LetElse, token)),
        _ => None,
    };

    let Some((kind, token)) = decision else {
        return;
    };
    let range = token.text_range();
    if !range.is_empty() {
        facts.push(AuthoredFact::Decision(AuthoredDecision {
            kind,
            range,
            reachability,
            nesting,
        }));
    }
}

fn record_match_alternative(
    node: &SyntaxNode,
    reachability: Reachability,
    state: Option<&mut MatchState>,
    body: Option<&BodyState>,
    facts: &mut Vec<AuthoredFact>,
) {
    let Some(state) = state else {
        return;
    };
    let Some(arrow) = ast::MatchArm::cast(node.clone()).and_then(|arm| arm.fat_arrow_token())
    else {
        return;
    };
    let alternative = reachability.and(state.preceding_arms);
    state.preceding_arms = state.preceding_arms.or(reachability);
    let Some(body) = body else {
        return;
    };
    if alternative == Reachability::NEVER {
        return;
    }
    facts.push(AuthoredFact::Decision(AuthoredDecision {
        kind: DecisionKind::MatchAlternative,
        range: arrow.text_range(),
        reachability: alternative,
        nesting: body.nesting,
    }));
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
    if let Some(expression) = ast::WhileExpr::cast(parent.clone()) {
        return expression
            .loop_body()
            .is_some_and(|body| body.syntax() == block.syntax());
    }
    if let Some(expression) = ast::ForExpr::cast(parent.clone()) {
        return expression
            .loop_body()
            .is_some_and(|body| body.syntax() == block.syntax());
    }
    ast::LoopExpr::cast(parent)
        .and_then(|expression| expression.loop_body())
        .is_some_and(|body| body.syntax() == block.syntax())
}

fn collect_declared_public(
    root: &SyntaxNode,
    profile: &CfgProfile,
    features: Option<&PackageFeatures>,
) -> [u32; REACHABILITY_STATES] {
    let mut counts = [0; REACHABILITY_STATES];
    for visibility in root.descendants().filter_map(ast::Visibility::cast) {
        if !matches!(visibility.kind(), VisibilityKind::Pub) {
            continue;
        }
        let reachability = reachability_for_node(profile, visibility.syntax(), features);
        counts[reachability.index()] += 1;
    }
    counts
}

fn collect_edges(
    file: &Path,
    root: &SyntaxNode,
    profile: &CfgProfile,
    features: Option<&PackageFeatures>,
) -> (Vec<SourceEdge>, Vec<UnresolvedEdge>) {
    let mut edges = Vec::new();
    let mut unresolved = Vec::new();
    for module in root.descendants().filter_map(ast::Module::cast) {
        if module.semicolon_token().is_none() {
            continue;
        }
        let gate = reachability_for_node(profile, module.syntax(), features);
        match resolve_module(file, &module) {
            Some(target) => edges.push(SourceEdge { target, gate }),
            None => unresolved.push(UnresolvedEdge {
                gate,
                message: format!(
                    "cannot resolve out-of-line module {}",
                    module.name().map_or_else(
                        || "<missing name>".to_owned(),
                        |name| name.text().to_string()
                    )
                ),
            }),
        }
    }

    for call in root.descendants().filter_map(ast::MacroCall::cast) {
        let Some(path) = call.path() else {
            continue;
        };
        if compact_text(path.syntax()) != "include" {
            continue;
        }
        let gate = reachability_for_node(profile, call.syntax(), features);
        let Some(included) = literal_macro_string(&call) else {
            unresolved.push(UnresolvedEdge {
                gate,
                message: "non-literal include! source is unresolved".to_owned(),
            });
            continue;
        };
        let target = file.parent().unwrap_or(Path::new(".")).join(included);
        if target.is_file() {
            edges.push(SourceEdge { target, gate });
        } else {
            unresolved.push(UnresolvedEdge {
                gate,
                message: format!("include! source does not exist: {}", target.display()),
            });
        }
    }
    (edges, unresolved)
}

fn resolve_module(file: &Path, module: &ast::Module) -> Option<PathBuf> {
    let module_name = module.name()?.text().to_string();
    let mut base = conventional_module_base(file);
    let mut inline_ancestors = module
        .syntax()
        .ancestors()
        .skip(1)
        .filter_map(ast::Module::cast)
        .filter(|ancestor| ancestor.item_list().is_some())
        .filter_map(|ancestor| ancestor.name().map(|name| name.text().to_string()))
        .collect::<Vec<_>>();
    inline_ancestors.reverse();
    let nested_inline = !inline_ancestors.is_empty();
    for ancestor in inline_ancestors {
        base.push(ancestor);
    }

    if let Some(path) = attribute_string(module.attrs(), "path") {
        let parent = file.parent().unwrap_or(Path::new("."));
        let sibling = parent.join(&path);
        let nested = base.join(path);
        let candidates = if nested_inline {
            [nested, sibling]
        } else {
            [sibling, nested]
        };
        return candidates.into_iter().find(|candidate| candidate.is_file());
    }

    let candidates = [
        base.join(format!("{module_name}.rs")),
        base.join(&module_name).join("mod.rs"),
    ];
    if let Some(found) = candidates.into_iter().find(|candidate| candidate.is_file()) {
        return Some(found);
    }

    let parent = file.parent().unwrap_or(Path::new("."));
    if base != parent {
        [
            parent.join(format!("{module_name}.rs")),
            parent.join(module_name).join("mod.rs"),
        ]
        .into_iter()
        .find(|candidate| candidate.is_file())
    } else {
        None
    }
}

fn conventional_module_base(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or(Path::new("."));
    match file.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => file
            .file_stem()
            .map_or_else(|| parent.to_path_buf(), |stem| parent.join(stem)),
    }
}

fn attribute_string(attributes: impl Iterator<Item = ast::Attr>, name: &str) -> Option<String> {
    attributes
        .filter_map(|attribute| attribute.meta())
        .find_map(|meta| {
            let ast::Meta::KeyValueMeta(key_value) = meta else {
                return None;
            };
            if key_value
                .path()
                .is_none_or(|path| compact_text(path.syntax()) != name)
            {
                return None;
            }
            key_value
                .syntax()
                .descendants_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .find_map(ast::String::cast)
                .and_then(|literal| literal.value().ok().map(|value| value.into_owned()))
        })
}

fn literal_macro_string(call: &ast::MacroCall) -> Option<String> {
    let tree = call.token_tree()?;
    let mut payload = tree
        .syntax()
        .children_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|token| {
            !matches!(
                token.kind(),
                SyntaxKind::WHITESPACE
                    | SyntaxKind::COMMENT
                    | SyntaxKind::L_PAREN
                    | SyntaxKind::R_PAREN
                    | SyntaxKind::L_BRACK
                    | SyntaxKind::R_BRACK
                    | SyntaxKind::L_CURLY
                    | SyntaxKind::R_CURLY
            )
        });
    let literal = ast::String::cast(payload.next()?)?;
    if payload.next().is_some() {
        return None;
    }
    literal.value().ok().map(|value| value.into_owned())
}

fn compact_text(node: &SyntaxNode) -> String {
    node.text()
        .to_string()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

struct LineIndex {
    starts: Vec<usize>,
    source_len: usize,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        if source.is_empty() {
            return Self {
                starts: Vec::new(),
                source_len: 0,
            };
        }
        let mut starts = Vec::with_capacity(source.len() / 32 + 1);
        starts.push(0);
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' && index + 1 < source.len() {
                starts.push(index + 1);
            }
        }
        Self {
            starts,
            source_len: source.len(),
        }
    }

    fn len(&self) -> usize {
        self.starts.len()
    }

    fn covered_lines(&self, range: TextRange) -> Option<(usize, usize)> {
        if self.starts.is_empty() || range.is_empty() {
            return None;
        }
        let start = usize::from(range.start()).min(self.source_len.saturating_sub(1));
        let end = usize::from(range.end())
            .saturating_sub(1)
            .min(self.source_len.saturating_sub(1));
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
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(raw_line(&bytes[start..index]));
            start = index + 1;
        }
    }
    if start < bytes.len() {
        lines.push(raw_line(&bytes[start..]));
    }
    lines
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
        let mut metrics = ComplexityMetrics::default();
        for &fact in &file.authored_facts {
            metrics.add(fact.metrics());
        }
        metrics
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
        assert_eq!(file.declared_public[Reachability::BOTH.index()], 7);
        assert_eq!(file.declared_public[Reachability::TEST.index()], 1);
        assert_eq!(file.declared_public.into_iter().sum::<u32>(), 8);
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
        let test_alternatives = file
            .authored_facts
            .iter()
            .copied()
            .filter(|fact| fact.reachability() == Reachability::TEST)
            .fold(ComplexityMetrics::default(), |mut metrics, fact| {
                metrics.add(fact.metrics());
                metrics
            });
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
