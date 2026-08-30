use std::collections::{BTreeSet, HashSet};

use ra_ap_cfg::{CfgAtom, CfgExpr};
use ra_ap_syntax::{AstNode, SyntaxNode, ast};

use crate::model::{Activation, Reachability};

#[derive(Clone, Debug, Default)]
pub struct PackageFeatures {
    pub enabled: BTreeSet<String>,
    pub excluded: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct CfgProfile {
    known_true: HashSet<String>,
    known_false: HashSet<String>,
    builtin_names: HashSet<String>,
    test_attributes: HashSet<String>,
}

impl CfgProfile {
    pub fn new(
        known_true: HashSet<String>,
        known_false: HashSet<String>,
        mut builtin_names: HashSet<String>,
        custom_test_attributes: &[String],
    ) -> Self {
        const BUILTIN_NAMES: &str = "target_arch target_endian target_env target_family target_feature target_has_atomic target_has_atomic_equal_alignment target_has_atomic_load_store target_has_atomic_primitive_alignment target_os target_pointer_width target_vendor panic debug_assertions unix windows";
        const TEST_ATTRIBUTES: &str = "test bench tokio::test async_std::test actix_rt::test rstest test_case wasm_bindgen_test";
        builtin_names.extend(BUILTIN_NAMES.split_whitespace().map(str::to_owned));
        let mut test_attributes = TEST_ATTRIBUTES
            .split_whitespace()
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        test_attributes.extend(custom_test_attributes.iter().cloned());

        Self {
            known_true,
            known_false,
            builtin_names,
            test_attributes,
        }
    }

    pub fn node_gate(&self, node: &SyntaxNode, features: Option<&PackageFeatures>) -> Reachability {
        node.children()
            .filter_map(ast::Attr::cast)
            .fold(Reachability::BOTH, |reachability, attr| {
                reachability.and(self.meta_gate(attr.meta(), features))
            })
    }

    fn meta_gate(
        &self,
        meta: Option<ast::Meta>,
        features: Option<&PackageFeatures>,
    ) -> Reachability {
        let Some(meta) = meta else {
            return Reachability::BOTH;
        };

        match meta {
            ast::Meta::CfgMeta(cfg) => {
                cfg.cfg_predicate().map_or(Reachability::BOTH, |predicate| {
                    self.cfg_reachability(CfgExpr::parse_from_ast(predicate), features)
                })
            }
            ast::Meta::CfgAttrMeta(cfg_attr) => {
                let condition = cfg_attr
                    .cfg_predicate()
                    .map_or(Reachability::BOTH, |predicate| {
                        self.cfg_reachability(CfgExpr::parse_from_ast(predicate), features)
                    });
                cfg_attr.metas().fold(Reachability::BOTH, |gate, nested| {
                    let nested_gate = self.meta_gate(Some(nested), features);
                    gate.and(Self::conditional_gate(condition, nested_gate))
                })
            }
            _ if self.is_test_meta(&meta) => Reachability::TEST,
            _ => Reachability::BOTH,
        }
    }

    fn conditional_gate(condition: Reachability, gate: Reachability) -> Reachability {
        Reachability {
            production: condition.production.not().or(gate.production),
            test: condition.test.not().or(gate.test),
        }
    }

    fn is_test_meta(&self, meta: &ast::Meta) -> bool {
        let Some(path) = meta.path() else {
            return false;
        };
        let compact = path
            .syntax()
            .text()
            .to_string()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        self.test_attributes.contains(&compact)
    }

    fn cfg_reachability(
        &self,
        expression: CfgExpr,
        features: Option<&PackageFeatures>,
    ) -> Reachability {
        Reachability {
            production: self.eval(&expression, false, features),
            test: self.eval(&expression, true, features),
        }
    }

    fn eval(
        &self,
        expression: &CfgExpr,
        test: bool,
        features: Option<&PackageFeatures>,
    ) -> Activation {
        match expression {
            CfgExpr::Invalid => Activation::Maybe,
            CfgExpr::Atom(atom) => self.eval_atom(atom, test, features),
            CfgExpr::All(expressions) => {
                expressions.iter().fold(Activation::Always, |value, expr| {
                    value.and(self.eval(expr, test, features))
                })
            }
            CfgExpr::Any(expressions) => {
                expressions.iter().fold(Activation::Never, |value, expr| {
                    value.or(self.eval(expr, test, features))
                })
            }
            CfgExpr::Not(expression) => self.eval(expression, test, features).not(),
        }
    }

    fn eval_atom(
        &self,
        atom: &CfgAtom,
        test: bool,
        features: Option<&PackageFeatures>,
    ) -> Activation {
        let (name, rendered) = match atom {
            CfgAtom::Flag(name) => (name.as_str(), name.as_str().to_owned()),
            CfgAtom::KeyValue { key, value } => {
                (key.as_str(), format!("{}={}", key.as_str(), value.as_str()))
            }
        };

        if name == "test" || name == "doctest" {
            return if test {
                Activation::Always
            } else {
                Activation::Never
            };
        }
        if name == "true" {
            return Activation::Always;
        }
        if name == "false" {
            return Activation::Never;
        }
        if name == "feature" {
            let CfgAtom::KeyValue { value, .. } = atom else {
                return Activation::Maybe;
            };
            let feature = value.as_str();
            return match features {
                Some(features) if features.excluded.contains(feature) => Activation::Never,
                Some(features) if features.enabled.contains(feature) => Activation::Always,
                Some(_) => Activation::Never,
                None => Activation::Maybe,
            };
        }
        if self.known_true.contains(&rendered) {
            return Activation::Always;
        }
        if self.known_false.contains(&rendered) {
            return Activation::Never;
        }
        if self.builtin_names.contains(name) {
            return Activation::Never;
        }
        Activation::Maybe
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ra_ap_syntax::{Edition, SourceFile, ast::HasModuleItem};

    use super::*;

    fn first_item_gate(source: &str, features: PackageFeatures) -> Reachability {
        let parse = SourceFile::parse(source, Edition::CURRENT);
        let item = parse.tree().items().next().expect("fixture item");
        CfgProfile::new(HashSet::new(), HashSet::new(), HashSet::new(), &[])
            .node_gate(item.syntax(), Some(&features))
    }

    #[test]
    fn compound_test_cfg_is_test_only_without_feature() {
        let gate = first_item_gate(
            "#[cfg(any(test, feature = \"testability\"))]\nfn helper() {}",
            PackageFeatures::default(),
        );
        assert_eq!(gate, Reachability::TEST);
    }

    #[test]
    fn compound_test_cfg_is_production_when_feature_is_enabled() {
        let mut features = PackageFeatures::default();
        features.enabled.insert("testability".to_owned());
        let gate = first_item_gate(
            "#[cfg(any(test, feature = \"testability\"))]\nfn helper() {}",
            features,
        );
        assert_eq!(gate, Reachability::BOTH);
    }

    #[test]
    fn cfg_attr_without_a_gating_attribute_keeps_the_item() {
        let gate = first_item_gate(
            "#[cfg_attr(test, derive(Debug))]\nstruct Value;",
            PackageFeatures::default(),
        );
        assert_eq!(gate, Reachability::BOTH);
    }

    #[test]
    fn forced_key_value_does_not_close_a_custom_cfg_namespace() {
        let mut known_true = HashSet::new();
        known_true.insert("mode=one".to_owned());
        let parse = SourceFile::parse("#[cfg(mode = \"two\")] fn other() {}", Edition::CURRENT);
        let item = parse.tree().items().next().unwrap();
        let gate = CfgProfile::new(known_true, HashSet::new(), HashSet::new(), &[])
            .node_gate(item.syntax(), Some(&PackageFeatures::default()));
        assert_eq!(
            gate,
            Reachability {
                production: Activation::Maybe,
                test: Activation::Maybe,
            }
        );
    }
}
