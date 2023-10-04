use std::fmt::Debug;

use index_vec::IndexVec;
use oxc::{
  ast::VisitMut,
  semantic::{ReferenceId, ScopeTree, SymbolId},
  span::Atom,
};
use rolldown_common::{
  ImportRecord, ImportRecordId, LocalOrReExport, ModuleId, NamedImport, ResolvedExport, ResourceId,
  StmtInfo, StmtInfoId, SymbolRef,
};
use rolldown_oxc::OxcProgram;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{module::ModuleFinalizeContext, module_id::ModuleVec, source_mutation::SourceMutation};
use crate::bundler::{
  graph::symbols::Symbols,
  module::module::Module,
  visitors::{FinalizeContext, Finalizer},
};

#[derive(Debug)]
pub struct NormalModule {
  pub exec_order: u32,
  pub id: ModuleId,
  pub resource_id: ResourceId,
  pub ast: OxcProgram,
  pub source_mutations: Vec<SourceMutation>,
  pub named_imports: FxHashMap<SymbolId, NamedImport>,
  pub named_exports: FxHashMap<Atom, LocalOrReExport>,
  pub stmt_infos: IndexVec<StmtInfoId, StmtInfo>,
  pub import_records: IndexVec<ImportRecordId, ImportRecord>,
  // [[StarExportEntries]] in https://tc39.es/ecma262/#sec-source-text-module-records
  pub star_exports: Vec<ImportRecordId>,
  // resolved
  pub resolved_exports: FxHashMap<Atom, ResolvedExport>,
  pub resolved_star_exports: Vec<ModuleId>,
  pub scope: ScopeTree,
  pub default_export_symbol: Option<SymbolId>,
  pub namespace_symbol: (SymbolRef, ReferenceId),
  pub is_symbol_for_namespace_referenced: bool,
}

pub enum Resolution {
  None,
  Ambiguous,
  Found(SymbolRef),
}

impl NormalModule {
  pub fn finalize(&mut self, ctx: ModuleFinalizeContext) {
    let program = self.ast.program_mut();
    let mut finalizer = Finalizer::new(FinalizeContext {
      symbols: ctx.symbols,
      id: self.id,
      final_names: ctx.canonical_names,
      source_mutations: &mut self.source_mutations,
    });
    finalizer.visit_program(program);
    if self.is_symbol_for_namespace_referenced {
      self
        .source_mutations
        .push(SourceMutation::AddNamespaceExport());
    }
  }

  pub fn initialize_namespace(&mut self) {
    self.stmt_infos.push(StmtInfo {
      stmt_idx: self.ast.program().body.len(),
      declared_symbols: vec![self.namespace_symbol.0.symbol],
    });
  }

  // https://tc39.es/ecma262/#sec-getexportednames
  pub fn get_exported_names<'modules>(
    &'modules self,
    stack: &mut Vec<ModuleId>,
    modules: &'modules ModuleVec,
  ) -> FxHashSet<&'modules Atom> {
    if stack.contains(&self.id) {
      // cycle
      return Default::default();
    }

    stack.push(self.id);

    let ret: FxHashSet<&'modules Atom> = {
      self
        .star_exports
        .iter()
        .copied()
        .map(|id| &self.import_records[id])
        .flat_map(|rec| {
          debug_assert!(rec.resolved_module.is_valid());
          let importee = &modules[rec.resolved_module];
          match importee {
            Module::Normal(importee) => importee
              .get_exported_names(stack, modules)
              .into_iter()
              .filter(|name| name.as_str() != "default")
              .collect::<Vec<_>>(),
            Module::External(importee) => importee
              .symbols_imported_by_others
              .keys()
              .filter(|name| name.as_str() != "default")
              .collect(),
          }
        })
        .chain(self.named_exports.keys())
        .collect()
    };

    stack.pop();
    ret
  }

  // https://tc39.es/ecma262/#sec-resolveexport
  pub fn resolve_export<'modules>(
    &'modules self,
    export_name: &'modules Atom,
    resolve_set: &mut Vec<(ModuleId, &'modules Atom)>,
    modules: &'modules ModuleVec,
    symbols: &mut Symbols,
  ) -> Resolution {
    let record = (self.id, export_name);
    if resolve_set.iter().rev().any(|prev| prev == &record) {
      unimplemented!("handle cycle")
    }
    resolve_set.push(record);

    let ret = if let Some(info) = self.named_exports.get(export_name) {
      match info {
        LocalOrReExport::Local(local) => {
          if let Some(named_import) = self.named_imports.get(&local.referenced.symbol) {
            let record = &self.import_records[named_import.record_id];
            let importee = &modules[record.resolved_module];
            match importee {
              Module::Normal(importee) => {
                let resolved = if named_import.is_imported_star {
                  Resolution::Found(importee.namespace_symbol.0)
                } else {
                  importee.resolve_export(&named_import.imported, resolve_set, modules, symbols)
                };
                if let Resolution::Found(exist) = &resolved {
                  symbols.union(local.referenced, *exist)
                }
                resolved
              }
              Module::External(importee) => {
                let resolve =
                  importee.resolve_export(&named_import.imported, named_import.is_imported_star);
                return Resolution::Found(resolve);
              }
            }
          } else {
            Resolution::Found(local.referenced)
          }
        }
        LocalOrReExport::Re(re) => {
          let record = &self.import_records[re.record_id];
          let importee = &modules[record.resolved_module];
          match importee {
            Module::Normal(importee) => {
              if re.is_imported_star {
                return Resolution::Found(importee.namespace_symbol.0);
              } else {
                importee.resolve_export(&re.imported, resolve_set, modules, symbols)
              }
            }
            Module::External(importee) => {
              let resolve = importee.resolve_export(&re.imported, re.is_imported_star);
              return Resolution::Found(resolve);
            }
          }
        }
      }
    } else {
      if export_name.as_str() == "default" {
        return Resolution::None;
      }
      let mut star_resolution: Option<SymbolRef> = None;
      for e in &self.star_exports {
        let rec = &self.import_records[*e];
        let importee = &modules[rec.resolved_module];
        match importee {
          Module::Normal(importee) => {
            match importee.resolve_export(export_name, resolve_set, modules, symbols) {
              Resolution::None => continue,
              Resolution::Ambiguous => return Resolution::Ambiguous,
              Resolution::Found(exist) => {
                if let Some(star_resolution) = star_resolution {
                  if star_resolution == exist {
                    continue;
                  } else {
                    return Resolution::Ambiguous;
                  }
                } else {
                  star_resolution = Some(exist)
                }
              }
            }
          }
          Module::External(_) => {
            // unimplemented!("handle external module")
          }
        }
      }

      star_resolution
        .map(Resolution::Found)
        .unwrap_or(Resolution::None)
    };

    resolve_set.pop();

    ret
  }

  pub fn resolve_star_exports(&self, modules: &ModuleVec) -> Vec<ModuleId> {
    let mut visited = FxHashSet::default();
    let mut resolved = vec![];
    let mut queue = self
      .star_exports
      .iter()
      .map(|rec_id| {
        let rec = &self.import_records[*rec_id];
        rec.resolved_module
      })
      .collect::<Vec<_>>();

    while let Some(importee_id) = queue.pop() {
      if !visited.contains(&importee_id) {
        visited.insert(importee_id);
        resolved.push(importee_id);
        let importee = &modules[importee_id];
        match importee {
          Module::Normal(importee) => queue.extend(
            importee
              .star_exports
              .iter()
              .map(|rec_id| importee.import_records[*rec_id].resolved_module),
          ),
          Module::External(_) => {}
        }
      }
    }

    resolved
  }
}