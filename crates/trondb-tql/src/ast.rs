#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateCollection(CreateCollectionStmt),
    Insert(InsertStmt),
    Fetch(FetchStmt),
    Search(SearchStmt),
    Explain(Box<Statement>),
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
