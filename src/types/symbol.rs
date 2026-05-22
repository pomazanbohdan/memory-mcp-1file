use super::{Datetime, SurrealValue, Thing};
use serde::{Deserialize, Serialize};

fn default_datetime() -> Datetime {
    Datetime::default()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SymbolType {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Module,
    Trait,
    Import,
}

impl std::fmt::Display for SymbolType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SymbolType::Function => write!(f, "function"),
            SymbolType::Method => write!(f, "method"),
            SymbolType::Class => write!(f, "class"),
            SymbolType::Struct => write!(f, "struct"),
            SymbolType::Enum => write!(f, "enum"),
            SymbolType::Interface => write!(f, "interface"),
            SymbolType::Module => write!(f, "module"),
            SymbolType::Trait => write!(f, "trait"),
            SymbolType::Import => write!(f, "import"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodeRelationType {
    Calls,
    Imports,
    Contains,
    Implements,
    Extends,
}

impl std::fmt::Display for CodeRelationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodeRelationType::Calls => write!(f, "calls"),
            CodeRelationType::Imports => write!(f, "imports"),
            CodeRelationType::Contains => write!(f, "contains"),
            CodeRelationType::Implements => write!(f, "implements"),
            CodeRelationType::Extends => write!(f, "extends"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct CodeSymbol {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Thing>,

    pub name: String,
    pub symbol_type: SymbolType,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub project_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,

    #[serde(default = "default_datetime")]
    pub indexed_at: Datetime,
}

impl CodeSymbol {
    pub fn new(
        name: String,
        symbol_type: SymbolType,
        file_path: String,
        start_line: u32,
        end_line: u32,
        project_id: String,
    ) -> Self {
        Self {
            id: None,
            name,
            symbol_type,
            file_path,
            start_line,
            end_line,
            project_id,
            signature: None,
            embedding: None,
            indexed_at: Datetime::default(),
        }
    }

    pub fn with_signature(mut self, signature: String) -> Self {
        self.signature = Some(signature);
        self
    }

    pub fn unique_key(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.project_id.hash(&mut hasher);
        self.file_path.hash(&mut hasher);
        self.name.hash(&mut hasher);
        self.start_line.hash(&mut hasher);

        format!("{:016x}", hasher.finish())
    }
}

/// Reference to a symbol with full context for Thing creation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRef {
    pub name: String,
    pub file_path: String,
    pub line: u32,
}

impl SymbolRef {
    pub fn new(name: String, file_path: String, line: u32) -> Self {
        Self {
            name,
            file_path,
            line,
        }
    }

    /// Create SymbolRef from an existing CodeSymbol
    pub fn from_symbol(symbol: &CodeSymbol) -> Self {
        Self {
            name: symbol.name.clone(),
            file_path: symbol.file_path.clone(),
            line: symbol.start_line,
        }
    }

    /// Convert to SurrealDB Thing for relation creation
    pub fn to_thing(&self, project_id: &str) -> Thing {
        crate::types::safe_thing::symbol_thing(project_id, &self.file_path, &self.name, self.line)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeReference {
    pub name: String,
    pub from_symbol: String,
    pub from_symbol_line: u32,
    pub to_symbol: String,
    pub relation_type: CodeRelationType,
    pub file_path: String,
    pub line: u32,
    pub column: u32,
}

impl CodeReference {
    pub fn builder() -> CodeReferenceBuilder {
        CodeReferenceBuilder::default()
    }
}

#[derive(Default)]
pub struct CodeReferenceBuilder {
    name: Option<String>,
    from_symbol: Option<String>,
    from_symbol_line: Option<u32>,
    to_symbol: Option<String>,
    relation_type: Option<CodeRelationType>,
    file_path: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}

impl CodeReferenceBuilder {
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn from_symbol(mut self, from_symbol: impl Into<String>) -> Self {
        self.from_symbol = Some(from_symbol.into());
        self
    }

    pub fn from_symbol_line(mut self, line: u32) -> Self {
        self.from_symbol_line = Some(line);
        self
    }

    pub fn to_symbol(mut self, to_symbol: impl Into<String>) -> Self {
        self.to_symbol = Some(to_symbol.into());
        self
    }

    pub fn relation_type(mut self, relation_type: CodeRelationType) -> Self {
        self.relation_type = Some(relation_type);
        self
    }

    pub fn file_path(mut self, file_path: impl Into<String>) -> Self {
        self.file_path = Some(file_path.into());
        self
    }

    pub fn line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    pub fn column(mut self, column: u32) -> Self {
        self.column = Some(column);
        self
    }

    pub fn build(self) -> CodeReference {
        CodeReference {
            name: self.name.expect("name is required"),
            from_symbol: self.from_symbol.expect("from_symbol is required"),
            from_symbol_line: self.from_symbol_line.expect("from_symbol_line is required"),
            to_symbol: self.to_symbol.expect("to_symbol is required"),
            relation_type: self.relation_type.expect("relation_type is required"),
            file_path: self.file_path.expect("file_path is required"),
            line: self.line.expect("line is required"),
            column: self.column.expect("column is required"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct SymbolRelation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Thing>,

    #[serde(rename = "in")]
    pub from_symbol: Thing,

    #[serde(rename = "out")]
    pub to_symbol: Thing,

    pub relation_type: CodeRelationType,

    pub file_path: String,
    pub line_number: u32,
    pub project_id: String,

    #[serde(default = "default_datetime")]
    pub created_at: Datetime,
}

impl SymbolRelation {
    pub fn new(
        from_symbol: Thing,
        to_symbol: Thing,
        relation_type: CodeRelationType,
        file_path: String,
        line_number: u32,
        project_id: String,
    ) -> Self {
        Self {
            id: None,
            from_symbol,
            to_symbol,
            relation_type,
            file_path,
            line_number,
            project_id,
            created_at: Datetime::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
pub struct ScoredSymbol {
    #[serde(flatten)]
    pub symbol: CodeSymbol,
    pub score: f32,
}
