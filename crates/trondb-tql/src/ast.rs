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
    Update(UpdateStmt),
    Infer(InferStmt),
    ConfirmEdge(ConfirmEdgeStmt),
    ExplainHistory(ExplainHistoryStmt),
    DropCollection(DropCollectionStmt),
    DropEdgeType(DropEdgeTypeStmt),
    FetchJoin(FetchJoinStmt),
}

// --- CREATE COLLECTION (expanded) ---

#[derive(Debug, Clone, PartialEq)]
pub struct CreateCollectionStmt {
    pub name: String,
    pub representations: Vec<RepresentationDecl>,
    pub fields: Vec<FieldDecl>,
    pub indexes: Vec<IndexDecl>,
    pub vectoriser_config: Option<VectoriserConfigDecl>,
}

/// Collection-level vectoriser configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct VectoriserConfigDecl {
    pub model: Option<String>,
    pub model_path: Option<String>,
    pub device: Option<String>,
    pub vectoriser_type: Option<String>,
    pub endpoint: Option<String>,
    pub auth: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepresentationDecl {
    pub name: String,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
    pub metric: Metric,
    pub sparse: bool,
    pub fields: Vec<String>,  // empty means passthrough
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

// --- ORDER BY ---

#[derive(Debug, Clone, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByClause {
    pub field: String,
    pub direction: SortDirection,
}

// --- Query Hints ---

#[derive(Debug, Clone, PartialEq)]
pub enum QueryHint {
    NoPromote,          // /*+ NO_PROMOTE */
    NoPrefilter,        // /*+ NO_PREFILTER */
    ForceFullScan,      // /*+ FORCE_FULL_SCAN */
    MaxAcu(f64),        // /*+ MAX_ACU(200) */
    Timeout(u64),       // /*+ TIMEOUT(5000) */
}

// --- FETCH ---

#[derive(Debug, Clone, PartialEq)]
pub struct FetchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub hints: Vec<QueryHint>,
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
    pub query_text: Option<String>,
    pub using_repr: Option<String>,
    pub hints: Vec<QueryHint>,
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
    Neq(String, Literal),
    Gt(String, Literal),
    Lt(String, Literal),
    Gte(String, Literal),
    Lte(String, Literal),
    And(Box<WhereClause>, Box<WhereClause>),
    Or(Box<WhereClause>, Box<WhereClause>),
    Not(Box<WhereClause>),
    IsNull(String),
    IsNotNull(String),
    In(String, Vec<Literal>),
    Like(String, String),
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
    pub inference_config: Option<InferenceConfigDecl>,
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

// --- UPDATE ---

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub entity_id: String,
    pub collection: String,
    pub assignments: Vec<(String, Literal)>,
}

// --- Inference ---

#[derive(Debug, Clone, PartialEq)]
pub struct InferStmt {
    pub from_id: String,
    pub edge_types: Vec<String>,       // empty = all applicable
    pub limit: Option<usize>,          // None = ALL
    pub confidence_floor: Option<f32>, // CONFIDENCE > threshold
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmEdgeStmt {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainHistoryStmt {
    pub entity_id: String,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropCollectionStmt {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropEdgeTypeStmt {
    pub name: String,
}

// --- JOINs ---

/// A qualified field reference: alias.field (e.g., `e.name`)
#[derive(Debug, Clone, PartialEq)]
pub struct QualifiedField {
    pub alias: String,
    pub field: String,
}

/// Join type
#[derive(Debug, Clone, PartialEq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
}

/// A single JOIN clause
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub join_type: JoinType,
    pub collection: String,
    pub alias: String,
    pub on_left: QualifiedField,
    pub on_right: QualifiedField,
    /// If Some, this is a probabilistic join with a confidence threshold
    pub confidence_threshold: Option<f64>,
}

/// FETCH ... FROM ... AS ... JOIN ... statement
#[derive(Debug, Clone, PartialEq)]
pub struct FetchJoinStmt {
    pub fields: JoinFieldList,
    pub from_collection: String,
    pub from_alias: String,
    pub joins: Vec<JoinClause>,
    pub filter: Option<WhereClause>,
    pub order_by: Vec<OrderByClause>,
    pub limit: Option<usize>,
    pub hints: Vec<QueryHint>,
}

/// Field list for JOINs — supports qualified (alias.field) and star
#[derive(Debug, Clone, PartialEq)]
pub enum JoinFieldList {
    All,
    Named(Vec<QualifiedField>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InferenceConfigDecl {
    pub auto: bool,
    pub confidence_floor: Option<f32>,
    pub limit: Option<usize>,
}
