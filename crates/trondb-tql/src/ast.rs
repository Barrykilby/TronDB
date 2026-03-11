#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateCollection(CreateCollectionStmt),
    Insert(InsertStmt),
    Fetch(FetchStmt),
    Search(SearchStmt),
    Explain(Box<Statement>),
    CreateEdgeType(CreateEdgeTypeStmt),
    InsertEdge(InsertEdgeStmt),
    DeleteEdge(DeleteEdgeStmt),
    Traverse(TraverseStmt),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateCollectionStmt {
    pub name: String,
    pub dimensions: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub collection: String,
    pub fields: Vec<String>,
    pub values: Vec<Literal>,
    pub vector: Option<Vec<f64>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub filter: Option<WhereClause>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchStmt {
    pub collection: String,
    pub fields: FieldList,
    pub near: Vec<f64>,
    pub confidence: Option<f64>,
    pub limit: Option<usize>,
}

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

#[derive(Debug, Clone, PartialEq)]
pub struct CreateEdgeTypeStmt {
    pub name: String,
    pub from_collection: String,
    pub to_collection: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertEdgeStmt {
    pub edge_type: String,
    pub from_id: String,
    pub to_id: String,
    pub metadata: Vec<(String, Literal)>,
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
