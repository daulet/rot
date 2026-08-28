use std::{
    fs,
    path::{Path, PathBuf},
};

use ra_ap_syntax::{
    AstNode, AstToken, Edition, NodeOrToken, SourceFile, SyntaxKind, SyntaxNode, SyntaxToken,
    TextRange, WalkEvent,
    ast::{self, HasAttrs, HasModuleItem, HasName, HasVisibility, VisibilityKind},
};

use crate::{
    cfg::{CfgProfile, PackageFeatures, reachability_for_node},
    model::Reachability,
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
    pub complexity: [u32; REACHABILITY_STATES],
    pub exported_relative: Reachability,
    pub exported_absolute: Reachability,
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
    pub public: bool,
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
    pub declared_items: [u32; REACHABILITY_STATES],
    pub exported_relative_items: [u32; REACHABILITY_STATES],
    pub exported_absolute_items: [u32; REACHABILITY_STATES],
    pub unresolved_public_uses: [u32; REACHABILITY_STATES],
    pub unresolved_globs: [u32; REACHABILITY_STATES],
    pub opaque_macro_calls: [u32; REACHABILITY_STATES],
    pub unresolved_inherent_public_items: [u32; REACHABILITY_STATES],
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
            declared_items: [0; REACHABILITY_STATES],
            exported_relative_items: [0; REACHABILITY_STATES],
            exported_absolute_items: [0; REACHABILITY_STATES],
            unresolved_public_uses: [0; REACHABILITY_STATES],
            unresolved_globs: [0; REACHABILITY_STATES],
            opaque_macro_calls: [0; REACHABILITY_STATES],
            unresolved_inherent_public_items: [0; REACHABILITY_STATES],
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
    let source = match std::str::from_utf8(&bytes) {
        Ok(source) => source,
        Err(_) => return Ok(LocalFile::invalid_utf8(bytes)),
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

    let mut api = ApiCollector::new(profile, features);
    let root_reach = profile.node_gate(root, features);
    api.visit_items(tree.items(), root_reach, true);
    api.apply_lines(&line_index, &mut lines);
    let (edges, unresolved_edges) = collect_edges(&path, root, profile, features);

    Ok(LocalFile {
        bytes: bytes.len() as u64,
        lines,
        syntax_errors,
        edges,
        unresolved_edges,
        declared_items: api.declared_items,
        exported_relative_items: api.exported_relative_items,
        exported_absolute_items: api.exported_absolute_items,
        unresolved_public_uses: api.unresolved_public_uses,
        unresolved_globs: api.unresolved_globs,
        opaque_macro_calls: api.opaque_macro_calls,
        unresolved_inherent_public_items: api.unresolved_inherent_public_items,
    })
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
        lines[first].complexity[reachability.index()] += 1;
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

#[derive(Clone, Debug)]
struct ApiFact {
    range: TextRange,
    reachability: Reachability,
    absolute: bool,
}

struct ApiCollector<'a> {
    profile: &'a CfgProfile,
    features: Option<&'a PackageFeatures>,
    facts: Vec<ApiFact>,
    declared_items: [u32; REACHABILITY_STATES],
    exported_relative_items: [u32; REACHABILITY_STATES],
    exported_absolute_items: [u32; REACHABILITY_STATES],
    unresolved_public_uses: [u32; REACHABILITY_STATES],
    unresolved_globs: [u32; REACHABILITY_STATES],
    opaque_macro_calls: [u32; REACHABILITY_STATES],
    unresolved_inherent_public_items: [u32; REACHABILITY_STATES],
}

impl<'a> ApiCollector<'a> {
    fn new(profile: &'a CfgProfile, features: Option<&'a PackageFeatures>) -> Self {
        Self {
            profile,
            features,
            facts: Vec::new(),
            declared_items: [0; REACHABILITY_STATES],
            exported_relative_items: [0; REACHABILITY_STATES],
            exported_absolute_items: [0; REACHABILITY_STATES],
            unresolved_public_uses: [0; REACHABILITY_STATES],
            unresolved_globs: [0; REACHABILITY_STATES],
            opaque_macro_calls: [0; REACHABILITY_STATES],
            unresolved_inherent_public_items: [0; REACHABILITY_STATES],
        }
    }

    fn visit_items(
        &mut self,
        items: impl Iterator<Item = ast::Item>,
        parent_reach: Reachability,
        public_path: bool,
    ) {
        for item in items {
            let reach = parent_reach.and(self.profile.node_gate(item.syntax(), self.features));
            match item {
                ast::Item::Module(module) => {
                    let declared = is_public(&module);
                    let exported = public_path && declared;
                    if declared {
                        self.add(
                            signature_before_child(
                                module.syntax(),
                                module.item_list().map(|list| list.syntax().clone()),
                            ),
                            reach,
                            true,
                            exported,
                            false,
                        );
                    }
                    if let Some(list) = module.item_list() {
                        let child_reach =
                            reach.and(self.profile.node_gate(list.syntax(), self.features));
                        self.visit_items(list.items(), child_reach, exported);
                    }
                }
                ast::Item::Fn(function) => {
                    self.add_visible(
                        &function,
                        signature_before_child(
                            function.syntax(),
                            function.body().map(|body| body.syntax().clone()),
                        ),
                        reach,
                        public_path,
                    );
                }
                ast::Item::Struct(item) => self.visit_struct(item, reach, public_path),
                ast::Item::Enum(item) => self.visit_enum(item, reach, public_path),
                ast::Item::Union(item) => self.visit_union(item, reach, public_path),
                ast::Item::Trait(item) => self.visit_trait(item, reach, public_path),
                ast::Item::Impl(item) => self.visit_impl(item, reach, public_path),
                ast::Item::ExternBlock(item) => self.visit_extern(item, reach, public_path),
                ast::Item::Use(item) => {
                    let declared = is_public(&item);
                    let exported = public_path && declared;
                    if declared {
                        self.add(item.syntax().text_range(), reach, true, exported, false);
                        if exported {
                            self.unresolved_public_uses[reach.index()] += 1;
                        }
                        if exported
                            && item
                                .syntax()
                                .descendants_with_tokens()
                                .filter_map(NodeOrToken::into_token)
                                .any(|token| token.text() == "*")
                        {
                            self.unresolved_globs[reach.index()] += 1;
                        }
                    }
                }
                ast::Item::MacroRules(item) => {
                    let macro_export = has_attribute(item.attrs(), "macro_export");
                    let declared = is_public(&item);
                    if declared || macro_export {
                        self.add(
                            item.syntax().text_range(),
                            reach,
                            declared,
                            public_path && declared,
                            macro_export,
                        );
                    }
                }
                ast::Item::MacroDef(item) => {
                    self.add_visible(&item, item.syntax().text_range(), reach, public_path);
                }
                ast::Item::MacroCall(item) => {
                    if public_path && !is_include_macro(&item) {
                        self.opaque_macro_calls[reach.index()] += 1;
                    }
                }
                ast::Item::Const(item) => {
                    self.add_visible(
                        &item,
                        signature_before_child(
                            item.syntax(),
                            item.body().map(|body| body.syntax().clone()),
                        ),
                        reach,
                        public_path,
                    );
                }
                ast::Item::Static(item) => {
                    self.add_visible(
                        &item,
                        signature_before_child(
                            item.syntax(),
                            item.body().map(|body| body.syntax().clone()),
                        ),
                        reach,
                        public_path,
                    );
                }
                ast::Item::TypeAlias(item) => {
                    self.add_visible(&item, item.syntax().text_range(), reach, public_path);
                }
                ast::Item::ExternCrate(item) => {
                    self.add_visible(&item, item.syntax().text_range(), reach, public_path);
                }
                ast::Item::AsmExpr(_) => {}
            }
        }
    }

    fn visit_struct(&mut self, item: ast::Struct, reach: Reachability, public_path: bool) {
        let declared = is_public(&item);
        let exported = public_path && declared;
        if declared {
            self.add(
                signature_before_child(
                    item.syntax(),
                    item.field_list().map(|fields| fields.syntax().clone()),
                ),
                reach,
                true,
                exported,
                false,
            );
        }
        if let Some(fields) = item.field_list() {
            match fields {
                ast::FieldList::RecordFieldList(fields) => {
                    for field in fields.fields() {
                        let field_reach =
                            reach.and(self.profile.node_gate(field.syntax(), self.features));
                        self.add_visible(
                            &field,
                            field.syntax().text_range(),
                            field_reach,
                            exported,
                        );
                    }
                }
                ast::FieldList::TupleFieldList(fields) => {
                    for field in fields.fields() {
                        let field_reach =
                            reach.and(self.profile.node_gate(field.syntax(), self.features));
                        self.add_visible(
                            &field,
                            field.syntax().text_range(),
                            field_reach,
                            exported,
                        );
                    }
                }
            }
        }
    }

    fn visit_union(&mut self, item: ast::Union, reach: Reachability, public_path: bool) {
        let declared = is_public(&item);
        let exported = public_path && declared;
        if declared {
            self.add(
                signature_before_child(
                    item.syntax(),
                    item.record_field_list()
                        .map(|fields| fields.syntax().clone()),
                ),
                reach,
                true,
                exported,
                false,
            );
        }
        if let Some(fields) = item.record_field_list() {
            for field in fields.fields() {
                let field_reach = reach.and(self.profile.node_gate(field.syntax(), self.features));
                self.add_visible(&field, field.syntax().text_range(), field_reach, exported);
            }
        }
    }

    fn visit_enum(&mut self, item: ast::Enum, reach: Reachability, public_path: bool) {
        let declared = is_public(&item);
        let exported = public_path && declared;
        if declared {
            self.add(
                signature_before_child(
                    item.syntax(),
                    item.variant_list()
                        .map(|variants| variants.syntax().clone()),
                ),
                reach,
                true,
                exported,
                false,
            );
        }
        if exported && let Some(variants) = item.variant_list() {
            for variant in variants.variants() {
                let variant_reach =
                    reach.and(self.profile.node_gate(variant.syntax(), self.features));
                self.add(
                    variant.syntax().text_range(),
                    variant_reach,
                    false,
                    true,
                    false,
                );
            }
        }
    }

    fn visit_trait(&mut self, item: ast::Trait, reach: Reachability, public_path: bool) {
        let declared = is_public(&item);
        let exported = public_path && declared;
        if declared {
            self.add(
                signature_before_child(
                    item.syntax(),
                    item.assoc_item_list().map(|items| items.syntax().clone()),
                ),
                reach,
                true,
                exported,
                false,
            );
        }
        if exported && let Some(items) = item.assoc_item_list() {
            let child_reach = reach.and(self.profile.node_gate(items.syntax(), self.features));
            for item in items.assoc_items() {
                self.visit_assoc(item, child_reach, true, true);
            }
        }
    }

    fn visit_impl(&mut self, item: ast::Impl, reach: Reachability, _public_path: bool) {
        if let Some(items) = item.assoc_item_list() {
            let child_reach = reach.and(self.profile.node_gate(items.syntax(), self.features));
            for item in items.assoc_items() {
                let item_reach =
                    child_reach.and(self.profile.node_gate(item.syntax(), self.features));
                if is_public_assoc(&item) {
                    self.unresolved_inherent_public_items[item_reach.index()] += 1;
                }
                self.visit_assoc(item, child_reach, false, false);
            }
        }
    }

    fn visit_assoc(
        &mut self,
        item: ast::AssocItem,
        parent_reach: Reachability,
        public_path: bool,
        implicit_public: bool,
    ) {
        let reach = parent_reach.and(self.profile.node_gate(item.syntax(), self.features));
        match item {
            ast::AssocItem::Fn(item) => {
                let declared = is_public(&item);
                let exported = public_path && (implicit_public || declared);
                if declared || implicit_public {
                    self.add(
                        signature_before_child(
                            item.syntax(),
                            item.body().map(|body| body.syntax().clone()),
                        ),
                        reach,
                        declared,
                        exported,
                        false,
                    );
                }
            }
            ast::AssocItem::Const(item) => {
                let declared = is_public(&item);
                let exported = public_path && (implicit_public || declared);
                if declared || implicit_public {
                    self.add(
                        signature_before_child(
                            item.syntax(),
                            item.body().map(|body| body.syntax().clone()),
                        ),
                        reach,
                        declared,
                        exported,
                        false,
                    );
                }
            }
            ast::AssocItem::TypeAlias(item) => {
                let declared = is_public(&item);
                let exported = public_path && (implicit_public || declared);
                if declared || implicit_public {
                    self.add(item.syntax().text_range(), reach, declared, exported, false);
                }
            }
            ast::AssocItem::MacroCall(_) => {
                if public_path {
                    self.opaque_macro_calls[reach.index()] += 1;
                }
            }
        }
    }

    fn visit_extern(&mut self, item: ast::ExternBlock, reach: Reachability, public_path: bool) {
        let Some(items) = item.extern_item_list() else {
            return;
        };
        let child_reach = reach.and(self.profile.node_gate(items.syntax(), self.features));
        for item in items.extern_items() {
            let item_reach = child_reach.and(self.profile.node_gate(item.syntax(), self.features));
            match item {
                ast::ExternItem::Fn(item) => {
                    self.add_visible(&item, item.syntax().text_range(), item_reach, public_path)
                }
                ast::ExternItem::Static(item) => {
                    self.add_visible(&item, item.syntax().text_range(), item_reach, public_path)
                }
                ast::ExternItem::TypeAlias(item) => {
                    self.add_visible(&item, item.syntax().text_range(), item_reach, public_path)
                }
                ast::ExternItem::MacroCall(_) => {
                    if public_path {
                        self.opaque_macro_calls[item_reach.index()] += 1;
                    }
                }
            }
        }
    }

    fn add_visible<T: HasVisibility>(
        &mut self,
        item: &T,
        range: TextRange,
        reachability: Reachability,
        public_path: bool,
    ) {
        if is_public(item) {
            self.add(range, reachability, true, public_path, false);
        }
    }

    fn add(
        &mut self,
        range: TextRange,
        reachability: Reachability,
        declared: bool,
        exported: bool,
        absolute: bool,
    ) {
        if declared {
            self.declared_items[reachability.index()] += 1;
        }
        if exported {
            self.exported_relative_items[reachability.index()] += 1;
            self.facts.push(ApiFact {
                range,
                reachability,
                absolute: false,
            });
        }
        if absolute {
            self.exported_absolute_items[reachability.index()] += 1;
            self.facts.push(ApiFact {
                range,
                reachability,
                absolute: true,
            });
        }
    }

    fn apply_lines(&self, line_index: &LineIndex, lines: &mut [LocalLine]) {
        for fact in &self.facts {
            let Some((first, last)) = line_index.covered_lines(fact.range) else {
                continue;
            };
            for line in &mut lines[first..=last] {
                if fact.absolute {
                    line.exported_absolute = line.exported_absolute.or(fact.reachability);
                } else {
                    line.exported_relative = line.exported_relative.or(fact.reachability);
                }
            }
        }
    }
}

fn is_public_assoc(item: &ast::AssocItem) -> bool {
    match item {
        ast::AssocItem::Fn(item) => is_public(item),
        ast::AssocItem::Const(item) => is_public(item),
        ast::AssocItem::TypeAlias(item) => is_public(item),
        ast::AssocItem::MacroCall(_) => false,
    }
}

fn is_public(item: &impl HasVisibility) -> bool {
    item.visibility()
        .is_some_and(|visibility| matches!(visibility.kind(), VisibilityKind::Pub))
}

fn has_attribute(mut attributes: impl Iterator<Item = ast::Attr>, name: &str) -> bool {
    attributes.any(|attribute| {
        attribute
            .simple_name()
            .is_some_and(|attribute| attribute == name)
    })
}

fn signature_before_child(node: &SyntaxNode, child: Option<SyntaxNode>) -> TextRange {
    child.map_or_else(
        || node.text_range(),
        |child| TextRange::new(node.text_range().start(), child.text_range().start()),
    )
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
        let public = public_module_path(&module);
        match resolve_module(file, &module) {
            Some(target) => edges.push(SourceEdge {
                target,
                gate,
                public,
            }),
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
            edges.push(SourceEdge {
                target,
                gate,
                public: public_syntax_path(call.syntax()),
            });
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

fn public_module_path(module: &ast::Module) -> bool {
    is_public(module) && public_syntax_path(module.syntax())
}

fn public_syntax_path(node: &SyntaxNode) -> bool {
    for ancestor in node.ancestors().skip(1) {
        if let Some(module) = ast::Module::cast(ancestor.clone())
            && !is_public(&module)
        {
            return false;
        }
        if matches!(
            ancestor.kind(),
            SyntaxKind::FN | SyntaxKind::BLOCK_EXPR | SyntaxKind::CLOSURE_EXPR
        ) {
            return false;
        }
    }
    true
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

fn is_include_macro(call: &ast::MacroCall) -> bool {
    call.path()
        .is_some_and(|path| compact_text(path.syntax()) == "include")
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
                .flat_map(|line| line.complexity)
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
                .flat_map(|line| line.complexity)
                .sum::<u32>(),
            6
        );
    }
}
