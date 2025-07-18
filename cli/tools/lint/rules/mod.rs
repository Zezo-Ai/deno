// Copyright 2018-2025 the Deno authors. MIT license.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

use deno_ast::ModuleSpecifier;
use deno_config::deno_json::ConfigFile;
use deno_config::deno_json::LintRulesConfig;
use deno_core::anyhow::bail;
use deno_core::error::AnyError;
use deno_graph::ModuleGraph;
use deno_lint::diagnostic::LintDiagnostic;
use deno_lint::rules::LintRule;
use deno_lint::tags;
use deno_resolver::workspace::WorkspaceResolver;

use crate::sys::CliSys;

mod no_sloppy_imports;
mod no_slow_types;

// used for publishing
pub use no_slow_types::collect_no_slow_type_diagnostics;

pub trait PackageLintRule: std::fmt::Debug + Send + Sync {
  fn code(&self) -> &'static str;

  fn tags(&self) -> tags::Tags {
    &[]
  }

  fn help_docs_url(&self) -> Cow<'static, str>;

  fn lint_package(
    &self,
    graph: &ModuleGraph,
    entrypoints: &[ModuleSpecifier],
  ) -> Vec<LintDiagnostic>;
}

pub(super) trait ExtendedLintRule: LintRule {
  /// If the rule supports the incremental cache.
  fn supports_incremental_cache(&self) -> bool;

  fn help_docs_url(&self) -> Cow<'static, str>;

  fn into_base(self: Box<Self>) -> Box<dyn LintRule>;
}

pub enum FileOrPackageLintRule {
  File(Box<dyn LintRule>),
  Package(Box<dyn PackageLintRule>),
}

#[derive(Debug)]
enum CliLintRuleKind {
  DenoLint(Box<dyn LintRule>),
  Extended(Box<dyn ExtendedLintRule>),
  Package(Box<dyn PackageLintRule>),
}

#[derive(Debug)]
pub struct CliLintRule(CliLintRuleKind);

impl PartialEq for CliLintRule {
  fn eq(&self, other: &Self) -> bool {
    self.code() == other.code()
  }
}

impl CliLintRule {
  pub fn code(&self) -> &'static str {
    use CliLintRuleKind::*;
    match &self.0 {
      DenoLint(rule) => rule.code(),
      Extended(rule) => rule.code(),
      Package(rule) => rule.code(),
    }
  }

  pub fn tags(&self) -> tags::Tags {
    use CliLintRuleKind::*;
    match &self.0 {
      DenoLint(rule) => rule.tags(),
      Extended(rule) => rule.tags(),
      Package(rule) => rule.tags(),
    }
  }

  pub fn help_docs_url(&self) -> Cow<'static, str> {
    use CliLintRuleKind::*;
    match &self.0 {
      DenoLint(rule) => {
        Cow::Owned(format!("https://docs.deno.com/lint/rules/{}", rule.code()))
      }
      Extended(rule) => rule.help_docs_url(),
      Package(rule) => rule.help_docs_url(),
    }
  }

  pub fn supports_incremental_cache(&self) -> bool {
    use CliLintRuleKind::*;
    match &self.0 {
      DenoLint(_) => true,
      Extended(rule) => rule.supports_incremental_cache(),
      // graph rules don't go through the incremental cache, so allow it
      Package(_) => true,
    }
  }

  pub fn into_file_or_pkg_rule(self) -> FileOrPackageLintRule {
    use CliLintRuleKind::*;
    match self.0 {
      DenoLint(rule) => FileOrPackageLintRule::File(rule),
      Extended(rule) => FileOrPackageLintRule::File(rule.into_base()),
      Package(rule) => FileOrPackageLintRule::Package(rule),
    }
  }
}

#[derive(Debug)]
pub struct ConfiguredRules {
  pub all_rule_codes: HashSet<Cow<'static, str>>,
  pub rules: Vec<CliLintRule>,
}

impl ConfiguredRules {
  pub fn supports_incremental_cache(&self) -> bool {
    self.rules.iter().all(|r| r.supports_incremental_cache())
  }

  pub fn incremental_cache_state(&self) -> impl std::hash::Hash + use<> {
    // use a hash of the rule names in order to bust the cache
    let mut codes = self.rules.iter().map(|r| r.code()).collect::<Vec<_>>();
    // ensure this is stable by sorting it
    codes.sort_unstable();
    Some(codes)
  }
}

pub struct LintRuleProvider {
  workspace_resolver: Option<Arc<WorkspaceResolver<CliSys>>>,
}

impl LintRuleProvider {
  pub fn new(
    workspace_resolver: Option<Arc<WorkspaceResolver<CliSys>>>,
  ) -> Self {
    Self { workspace_resolver }
  }

  pub fn resolve_lint_rules_err_empty(
    &self,
    rules: LintRulesConfig,
    maybe_config_file: Option<&ConfigFile>,
  ) -> Result<ConfiguredRules, AnyError> {
    let lint_rules = self.resolve_lint_rules(rules, maybe_config_file);
    if lint_rules.rules.is_empty() {
      bail!("No rules have been configured")
    }
    Ok(lint_rules)
  }

  pub fn all_rules(&self) -> Vec<CliLintRule> {
    let deno_lint_rules = deno_lint::rules::get_all_rules();
    let cli_lint_rules = vec![CliLintRule(CliLintRuleKind::Extended(
      Box::new(no_sloppy_imports::NoSloppyImportsRule::new(
        self.workspace_resolver.clone(),
      )),
    ))];
    let cli_graph_rules = vec![CliLintRule(CliLintRuleKind::Package(
      Box::new(no_slow_types::NoSlowTypesRule),
    ))];
    deno_lint_rules
      .into_iter()
      .map(|rule| CliLintRule(CliLintRuleKind::DenoLint(rule)))
      .chain(cli_lint_rules)
      .chain(cli_graph_rules)
      .collect()
  }

  pub fn resolve_lint_rules(
    &self,
    rules: LintRulesConfig,
    maybe_config_file: Option<&ConfigFile>,
  ) -> ConfiguredRules {
    let all_rules = self.all_rules();
    let mut all_rule_names = HashSet::with_capacity(all_rules.len());
    for rule in &all_rules {
      all_rule_names.insert(rule.code().into());
    }
    let rules = filtered_rules(
      all_rules.into_iter(),
      rules
        .tags
        .or_else(|| Some(get_default_tags(maybe_config_file))),
      rules.exclude,
      rules.include,
    );
    ConfiguredRules {
      rules,
      all_rule_codes: all_rule_names,
    }
  }
}

fn get_default_tags(maybe_config_file: Option<&ConfigFile>) -> Vec<String> {
  let mut tags = Vec::with_capacity(2);
  tags.push("recommended".to_string());
  if maybe_config_file.map(|c| c.is_package()).unwrap_or(false) {
    tags.push("jsr".to_string());
  }
  tags
}

fn filtered_rules(
  all_rules: impl Iterator<Item = CliLintRule>,
  maybe_tags: Option<Vec<String>>,
  maybe_exclude: Option<Vec<String>>,
  maybe_include: Option<Vec<String>>,
) -> Vec<CliLintRule> {
  let tags_set =
    maybe_tags.map(|tags| tags.into_iter().collect::<HashSet<_>>());

  let mut rules = all_rules
    .filter(|rule| {
      let mut passes = if let Some(tags_set) = &tags_set {
        rule
          .tags()
          .iter()
          .any(|t| tags_set.contains(&t.to_string()))
      } else {
        true
      };

      if let Some(includes) = &maybe_include {
        if includes.contains(&rule.code().to_owned()) {
          passes |= true;
        }
      }

      if let Some(excludes) = &maybe_exclude {
        if excludes.contains(&rule.code().to_owned()) {
          passes &= false;
        }
      }

      passes
    })
    .collect::<Vec<_>>();

  rules.sort_by_key(|r| r.code());

  rules
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::args::LintRulesConfig;

  #[test]
  fn recommended_rules_when_no_tags_in_config() {
    let rules_config = LintRulesConfig {
      exclude: Some(vec!["no-debugger".to_string()]),
      include: None,
      tags: None,
    };
    let rules_provider = LintRuleProvider::new(None);
    let rules = rules_provider.resolve_lint_rules(rules_config, None);
    let mut rule_names = rules
      .rules
      .into_iter()
      .map(|r| r.code().to_string())
      .collect::<Vec<_>>();
    rule_names.sort();
    let mut recommended_rule_names = rules_provider
      .resolve_lint_rules(Default::default(), None)
      .rules
      .into_iter()
      .filter(|r| r.tags().contains(&tags::RECOMMENDED))
      .map(|r| r.code().to_string())
      .filter(|n| n != "no-debugger")
      .collect::<Vec<_>>();
    recommended_rule_names.sort();
    assert_eq!(rule_names, recommended_rule_names);
  }
}
