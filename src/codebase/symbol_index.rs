//! In-memory symbol index for fast cross-file resolution.

use std::collections::HashMap;

use crate::types::symbol::{CodeSymbol, SymbolRef};

/// Context for symbol resolution with priority scoring.
#[derive(Debug, Clone)]
pub struct ResolutionContext {
    pub caller_file: String,
}

impl ResolutionContext {
    pub fn new(caller_file: String) -> Self {
        Self { caller_file }
    }
}

/// In-memory index for fast symbol lookup with priority-based resolution.
#[derive(Debug, Default)]
pub struct SymbolIndex {
    by_name: HashMap<String, Vec<SymbolRef>>,
    total_symbols: usize,
}

/// Hard safety limits for in-memory symbol accumulation during project indexing.
/// These caps prevent unbounded attacker-controlled growth from exhausting RAM.
const MAX_TOTAL_SYMBOLS: usize = 200_000;
const MAX_SYMBOLS_PER_NAME: usize = 256;

impl SymbolIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a symbol to the index.
    pub fn add(&mut self, symbol: &CodeSymbol) {
        if self.total_symbols >= MAX_TOTAL_SYMBOLS {
            return;
        }

        let sym_ref = SymbolRef::from_symbol(symbol);
        let entry = self.by_name.entry(symbol.name.clone()).or_default();
        if entry.len() >= MAX_SYMBOLS_PER_NAME {
            return;
        }

        entry.push(sym_ref);
        self.total_symbols += 1;
    }

    /// Add multiple symbols to the index.
    pub fn add_batch(&mut self, symbols: &[CodeSymbol]) {
        for symbol in symbols {
            self.add(symbol);
        }
    }

    /// Resolve a symbol name with priority scoring.
    /// Priority: same file (100) > same directory (50) > any (0)
    pub fn resolve(&self, name: &str, ctx: &ResolutionContext) -> Option<SymbolRef> {
        let candidates = self.by_name.get(name)?;

        candidates
            .iter()
            .map(|s| (self.score(s, ctx), s))
            .max_by_key(|(score, _)| *score)
            .map(|(_, s)| s.clone())
    }

    /// Get all symbols with a given name (for debugging).
    pub fn get_all(&self, name: &str) -> Option<&Vec<SymbolRef>> {
        self.by_name.get(name)
    }

    /// Total number of unique names in the index.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Total number of symbol refs retained in memory.
    pub fn total_symbols(&self) -> usize {
        self.total_symbols
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    fn score(&self, symbol: &SymbolRef, ctx: &ResolutionContext) -> i32 {
        let mut score = 0;

        // Same file gets highest priority
        if symbol.file_path == ctx.caller_file {
            score += 100;
        }
        // Same directory gets medium priority
        else if same_directory(&symbol.file_path, &ctx.caller_file) {
            score += 50;
        }

        score
    }
}

/// Check if two file paths are in the same directory.
fn same_directory(path1: &str, path2: &str) -> bool {
    let parent1 = std::path::Path::new(path1).parent();
    let parent2 = std::path::Path::new(path2).parent();
    parent1.is_some() && parent1 == parent2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::symbol::{CodeSymbol, SymbolType};

    fn make_symbol(name: &str, file: &str, line: u32) -> CodeSymbol {
        CodeSymbol::new(
            name.to_string(),
            SymbolType::Function,
            file.to_string(),
            line,
            line + 10,
            "test_project".to_string(),
        )
    }

    #[test]
    fn test_resolve_same_file_priority() {
        let mut index = SymbolIndex::new();
        index.add(&make_symbol("foo", "/src/a.rs", 10));
        index.add(&make_symbol("foo", "/src/b.rs", 20));

        let ctx = ResolutionContext::new("/src/a.rs".to_string());
        let resolved = index.resolve("foo", &ctx).unwrap();

        assert_eq!(resolved.file_path, "/src/a.rs");
        assert_eq!(resolved.line, 10);
    }

    #[test]
    fn test_resolve_same_directory_priority() {
        let mut index = SymbolIndex::new();
        index.add(&make_symbol("bar", "/src/utils/a.rs", 10));
        index.add(&make_symbol("bar", "/other/b.rs", 20));

        let ctx = ResolutionContext::new("/src/utils/caller.rs".to_string());
        let resolved = index.resolve("bar", &ctx).unwrap();

        assert_eq!(resolved.file_path, "/src/utils/a.rs");
    }

    #[test]
    fn test_resolve_not_found() {
        let index = SymbolIndex::new();
        let ctx = ResolutionContext::new("/src/a.rs".to_string());
        assert!(index.resolve("nonexistent", &ctx).is_none());
    }

    #[test]
    fn test_per_name_cap() {
        let mut index = SymbolIndex::new();
        for i in 0..(MAX_SYMBOLS_PER_NAME as u32 + 10) {
            index.add(&make_symbol("dup", &format!("/src/{i}.rs"), i));
        }

        let retained = index.get_all("dup").map_or(0, Vec::len);
        assert_eq!(retained, MAX_SYMBOLS_PER_NAME);
    }

    #[test]
    fn test_total_cap() {
        let mut index = SymbolIndex::new();
        for i in 0..(MAX_TOTAL_SYMBOLS as u32 + 10) {
            index.add(&make_symbol(&format!("name_{i}"), &format!("/src/{i}.rs"), i));
        }

        assert_eq!(index.total_symbols(), MAX_TOTAL_SYMBOLS);
    }
}
