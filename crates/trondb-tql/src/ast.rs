#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateCollection(CreateCollectionStmt),
    Insert(InsertStmt),
    Fetch(FetchStmt),
    Search(SearchStmt),
    Explain(Box<Statement>),
    CreateEdgeType(CreateEdgeTypeStmt),
    InsertEdge(InsertEdgeStmt),
    Delete(DeleteStmt),
    DeleteEdge(DeleteEdgeStmt),
    Traverse(TraverseStmt),
    CreateAffinityGroup(CreateAffinityGroupStmt),
    AlterEntityDropAffinity(AlterEntityDropAffinityStmt),
    Demote(DemoteStmt),
    Promote(PromoteStmt),
    ExplainTiers(ExplainTiersStmt),
}

// --- CREATE COLLECTION (expanded) ---

#[derive(Debug, Clone, PartialEq)]
pub struct CreateCollectionStmt {
    pub name: String,
    pub representations: Vec<RepresentationDecl>,
    pub fields: Vec<FieldDecl>,
    pub indexes: Vec<IndexDecl>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepresentationDecl {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Metric {
    #[default]
    Cosine,
    InnerProduct,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    pub name: String,
    pub field_type: FieldType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Text,
    DateTime,
    Bool,
    Int,
    Float,
    EntityRef(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexDecl {
    pub name: String,
    pub fields: Vec<String>,
    pub partial_condition: Option<WhereClause>,
}

// --- INSERT (expanded with named representations) ---

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vectors: Vec<(String, VectorLiteral)>,
    pub collocate_with: Option<Vec<String>>,
    pub affinity_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VectorLiteral {
    Dense(Vec<f64>),
    Sparse(Vec<(u32, f32)>),
}

// --- FETCH (unchanged) ---

#[derive(Debug, Clone, PartialEq)]
pub struct FetchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub limit: Option<usize>,
}

// --- SEARCH (expanded) ---

#[derive(Debug, Clone, PartialEq)]
pub struct SearchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub dense_vector: Option<Vec<f64>>,
    pub sparse_vector: Option<Vec<(u32, f32)>>,
    pub filter: Option<WhereClause>,
    pub confidence: Option<f64>,
    pub limit: Option<usize>,
}

// --- Common types ---

#[derive(Debug, Clone, PartialEq)]
pub enum FieldList {
    All,
    Named(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WhereClause {
    Eq(String, Literal),
    Gt(String, Literal),
    Lt(String, Literal),
    Gte(String, Literal),
    Lte(String, Literal),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

// --- Decay declarations (parser-side, no core dependency) ---

#[derive(Debug, Clone, PartialEq)]
pub enum DecayFnDecl {
    Exponential,
    Linear,
    Step,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecayConfigDecl {
    pub decay_fn: Option<DecayFnDecl>,
    pub decay_rate: Option<f64>,
    pub floor: Option<f64>,
    pub promote_threshold: Option<f64>,
    pub prune_threshold: Option<f64>,
}

// --- Edge types ---

#[derive(Debug, Clone, PartialEq)]
pub struct CreateEdgeTypeStmt {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
    pub decay_config: Option<DecayConfigDecl>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, Literal)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub entity_id: String,
    pub collection: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraverseStmt {
    pub edge_type: String,
    pub from_id: String,
    pub depth: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateAffinityGroupStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterEntityDropAffinityStmt {
    pub entity_id: String,
}

// --- Tier management ---

#[derive(Debug, Clone, PartialEq)]
pub enum TierTarget {
    Warm,
    Archive,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DemoteStmt {
    pub entity_id: String,
    pub collection: String,
    pub target_tier: TierTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromoteStmt {
    pub entity_id: String,
    pub collection: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainTiersStmt {
    pub collection: String,
}
