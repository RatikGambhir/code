//! A small, typed SQL builder.
//!
//! Tables are enums of their columns (declared with [`table!`]), so queries
//! name columns as `AgentThreads::Title` rather than as strings, and a typo
//! is a compile error. Values that are stored as text but mean one of a few
//! things are enums too (declared with [`sql_enum!`]).
//!
//! ```ignore
//! AgentThreads::insert()
//!     .value(AgentThreads::ThreadId, &thread.thread_id)
//!     .value(AgentThreads::Title, thread.title.as_deref())
//!     .value(AgentThreads::UpdatedAt, thread.updated_at)
//!     .on_conflict([AgentThreads::ThreadId])
//!     .keep_existing_if_null(AgentThreads::Title)
//!     .take_new(AgentThreads::UpdatedAt)
//!     .execute(w)?;
//!
//! let recent = Workspaces::select(Workspaces::ALL)
//!     .order_by(Workspaces::LastOpened, Order::Desc)
//!     .limit(10)
//!     .fetch_all(r, WorkspaceRecord::from_row)?;
//! ```
//!
//! Every value is bound as a parameter (`?1`, `?2`, …); nothing a caller
//! passes is ever spliced into the SQL text.
//!
//! Writes take a [`Writer`](crate::Writer), which only exists inside
//! [`Database::write`](crate::Database::write)'s transaction, so a write can
//! never run outside an atomic transaction.

use std::fmt::Write as _;

use anyhow::{bail, Result};
pub use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, types::FromSql, Row};

use crate::{Reader, Writer};

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// Anything that can be bound as a parameter.
pub trait IntoSql {
    fn into_sql(self) -> SqlValue;
}

macro_rules! into_sql_integer {
    ($($t:ty),*) => {$(
        impl IntoSql for $t {
            fn into_sql(self) -> SqlValue {
                SqlValue::Integer(self as i64)
            }
        }
    )*};
}
into_sql_integer!(i64, i32, i16, i8, u32, u16, u8, usize);

impl IntoSql for bool {
    fn into_sql(self) -> SqlValue {
        SqlValue::Integer(self as i64)
    }
}

impl IntoSql for f64 {
    fn into_sql(self) -> SqlValue {
        SqlValue::Real(self)
    }
}

impl IntoSql for String {
    fn into_sql(self) -> SqlValue {
        SqlValue::Text(self)
    }
}

impl IntoSql for &str {
    fn into_sql(self) -> SqlValue {
        SqlValue::Text(self.to_string())
    }
}

impl IntoSql for &String {
    fn into_sql(self) -> SqlValue {
        SqlValue::Text(self.clone())
    }
}

impl IntoSql for Vec<u8> {
    fn into_sql(self) -> SqlValue {
        SqlValue::Blob(self)
    }
}

impl IntoSql for SqlValue {
    fn into_sql(self) -> SqlValue {
        self
    }
}

impl<T: IntoSql> IntoSql for Option<T> {
    fn into_sql(self) -> SqlValue {
        self.map_or(SqlValue::Null, IntoSql::into_sql)
    }
}

// ---------------------------------------------------------------------------
// Tables and columns
// ---------------------------------------------------------------------------

/// A column of one table. Implemented by the enums [`table!`] generates.
pub trait Column: Copy + 'static {
    /// The table's name.
    const TABLE: &'static str;
    /// Every column, in declaration order.
    const ALL: &'static [Self];

    fn name(self) -> &'static str;

    // --- statements on this table ---------------------------------------

    fn select<I, E>(columns: I) -> Select
    where
        I: IntoIterator<Item = E>,
        E: Into<Expr>,
    {
        Select::new(Self::TABLE, columns.into_iter().map(Into::into).collect())
    }

    fn insert() -> Insert {
        Insert::new(Self::TABLE)
    }

    fn update() -> Update {
        Update::new(Self::TABLE)
    }

    fn delete() -> Delete {
        Delete::new(Self::TABLE)
    }

    // --- expressions -----------------------------------------------------

    /// `table.column`, for when a statement involves more than one source.
    fn qualified(self) -> Expr {
        Expr::Qualified(Self::TABLE, self.name())
    }

    /// `excluded.column`: the value an upsert tried to insert.
    fn excluded(self) -> Expr {
        Expr::Qualified("excluded", self.name())
    }

    /// `SUM(column)`, which is `NULL` over zero rows.
    fn sum(self) -> Expr {
        Expr::Function("SUM", vec![Expr::Column(self.name())])
    }

    /// `COALESCE(SUM(column), 0)`.
    fn sum_or_zero(self) -> Expr {
        Expr::Function("COALESCE", vec![self.sum(), Expr::Literal("0")])
    }

    fn max(self) -> Expr {
        Expr::Function("MAX", vec![Expr::Column(self.name())])
    }

    fn count(self) -> Expr {
        Expr::Function("COUNT", vec![Expr::Column(self.name())])
    }

    // --- conditions ------------------------------------------------------

    fn equals(self, value: impl IntoSql) -> Condition {
        Condition::compare(self, "=", value)
    }

    fn not_equals(self, value: impl IntoSql) -> Condition {
        Condition::compare(self, "<>", value)
    }

    /// Null-safe equality (`IS`): `NULL IS NULL` is true.
    fn is(self, value: impl IntoSql) -> Condition {
        Condition::compare(self, "IS", value)
    }

    fn greater_than(self, value: impl IntoSql) -> Condition {
        Condition::compare(self, ">", value)
    }

    fn at_least(self, value: impl IntoSql) -> Condition {
        Condition::compare(self, ">=", value)
    }

    fn less_than(self, value: impl IntoSql) -> Condition {
        Condition::compare(self, "<", value)
    }

    fn at_most(self, value: impl IntoSql) -> Condition {
        Condition::compare(self, "<=", value)
    }

    fn is_null(self) -> Condition {
        Condition::IsNull(Expr::Column(self.name()), true)
    }

    fn is_not_null(self) -> Condition {
        Condition::IsNull(Expr::Column(self.name()), false)
    }

    /// Case-insensitive substring match. `%` and `_` in `text` match
    /// themselves, not "anything".
    fn contains(self, text: &str) -> Condition {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        Condition::Like(Expr::Column(self.name()), format!("%{escaped}%"))
    }

    fn is_in<I, V>(self, values: I) -> Condition
    where
        I: IntoIterator<Item = V>,
        V: IntoSql,
    {
        Condition::In(
            Expr::Column(self.name()),
            values.into_iter().map(IntoSql::into_sql).collect(),
        )
    }
}

impl<C: Column> From<C> for Expr {
    fn from(column: C) -> Self {
        Expr::Column(column.name())
    }
}

/// Declares a table as an enum of its columns.
///
/// ```ignore
/// table! {
///     /// Projects the user has opened.
///     pub enum Workspaces = "workspaces" {
///         Id = "id",
///         Path = "path",
///     }
/// }
/// ```
#[macro_export]
macro_rules! table {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident = $table:literal {
            $($(#[$col_meta:meta])* $column:ident = $column_name:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
        $vis enum $name {
            $($(#[$col_meta])* $column),+
        }

        impl $crate::sql::Column for $name {
            const TABLE: &'static str = $table;
            const ALL: &'static [Self] = &[$(Self::$column),+];

            fn name(self) -> &'static str {
                match self {
                    $(Self::$column => $column_name),+
                }
            }
        }
    };
}

/// Declares an enum stored as text, with `ToSql`/`FromSql` and serde
/// support. Unknown stored values fail to decode rather than guessing.
///
/// ```ignore
/// sql_enum! {
///     pub enum TabKind { File = "file", Browser = "browser" }
/// }
/// ```
#[macro_export]
macro_rules! sql_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident = $text:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        $vis enum $name {
            $($(#[$variant_meta])* #[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl $crate::sql::IntoSql for $name {
            fn into_sql(self) -> $crate::sql::SqlValue {
                $crate::sql::SqlValue::Text(self.as_str().to_string())
            }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                let text = value.as_str()?;
                Self::parse(text).ok_or_else(|| {
                    rusqlite::types::FromSqlError::Other(
                        format!("unknown {} value {text:?}", stringify!($name)).into(),
                    )
                })
            }
        }
    };
}

/// Reads a column of a result row by its enum.
pub trait RowExt {
    fn col<T: FromSql, C: Column>(&self, column: C) -> rusqlite::Result<T>;
}

impl RowExt for Row<'_> {
    fn col<T: FromSql, C: Column>(&self, column: C) -> rusqlite::Result<T> {
        self.get(column.name())
    }
}

// ---------------------------------------------------------------------------
// Expressions and conditions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Column(&'static str),
    Qualified(&'static str, &'static str),
    Value(SqlValue),
    /// A constant written into the SQL text. `'static` so it can only come
    /// from the program, never from user input.
    Literal(&'static str),
    Function(&'static str, Vec<Expr>),
    /// `expr AS alias`; only meaningful in a select list.
    Alias(Box<Expr>, &'static str),
}

impl Expr {
    pub fn value(value: impl IntoSql) -> Self {
        Expr::Value(value.into_sql())
    }

    /// `COALESCE(a, b, …)`.
    pub fn coalesce(exprs: impl IntoIterator<Item = Expr>) -> Self {
        Expr::Function("COALESCE", exprs.into_iter().collect())
    }

    /// Names a computed column after `column`, so the row can be read back
    /// with `row.col(column)`.
    pub fn aliased_as<C: Column>(self, column: C) -> Self {
        Expr::Alias(Box::new(self), column.name())
    }

    fn render(&self, sql: &mut SqlText) {
        match self {
            Expr::Column(name) => sql.push(name),
            Expr::Qualified(table, name) => {
                let _ = write!(sql.text, "{table}.{name}");
            }
            Expr::Value(value) => sql.bind(value.clone()),
            Expr::Literal(text) => sql.push(text),
            Expr::Function(name, args) => {
                sql.push(name);
                sql.push("(");
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        sql.push(", ");
                    }
                    arg.render(sql);
                }
                sql.push(")");
            }
            Expr::Alias(expr, alias) => {
                expr.render(sql);
                let _ = write!(sql.text, " AS {alias}");
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Condition {
    Compare(Expr, &'static str, Expr),
    /// `true` for `IS NULL`, `false` for `IS NOT NULL`.
    IsNull(Expr, bool),
    Like(Expr, String),
    In(Expr, Vec<SqlValue>),
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    fn compare<C: Column>(column: C, op: &'static str, value: impl IntoSql) -> Self {
        Condition::Compare(Expr::Column(column.name()), op, Expr::Value(value.into_sql()))
    }

    pub fn and(self, other: Condition) -> Self {
        match self {
            Condition::All(mut all) => {
                all.push(other);
                Condition::All(all)
            }
            first => Condition::All(vec![first, other]),
        }
    }

    pub fn or(self, other: Condition) -> Self {
        match self {
            Condition::Any(mut any) => {
                any.push(other);
                Condition::Any(any)
            }
            first => Condition::Any(vec![first, other]),
        }
    }

    pub fn any(conditions: impl IntoIterator<Item = Condition>) -> Self {
        Condition::Any(conditions.into_iter().collect())
    }

    pub fn negate(self) -> Self {
        Condition::Not(Box::new(self))
    }

    fn render(&self, sql: &mut SqlText) {
        match self {
            Condition::Compare(left, op, right) => {
                left.render(sql);
                let _ = write!(sql.text, " {op} ");
                right.render(sql);
            }
            Condition::IsNull(expr, is_null) => {
                expr.render(sql);
                sql.push(if *is_null { " IS NULL" } else { " IS NOT NULL" });
            }
            Condition::Like(expr, pattern) => {
                expr.render(sql);
                sql.push(" LIKE ");
                sql.bind(SqlValue::Text(pattern.clone()));
                sql.push(" ESCAPE '\\'");
            }
            Condition::In(expr, values) => {
                if values.is_empty() {
                    // `x IN ()` is a syntax error; an empty set matches nothing.
                    sql.push("0");
                    return;
                }
                expr.render(sql);
                sql.push(" IN (");
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        sql.push(", ");
                    }
                    sql.bind(value.clone());
                }
                sql.push(")");
            }
            Condition::All(conditions) => render_joined(sql, conditions, " AND ", "1"),
            Condition::Any(conditions) => render_joined(sql, conditions, " OR ", "0"),
            Condition::Not(condition) => {
                sql.push("NOT (");
                condition.render(sql);
                sql.push(")");
            }
        }
    }
}

fn render_joined(sql: &mut SqlText, conditions: &[Condition], separator: &str, empty: &str) {
    match conditions {
        [] => sql.push(empty),
        [only] => only.render(sql),
        many => {
            for (i, condition) in many.iter().enumerate() {
                if i > 0 {
                    sql.push(separator);
                }
                let nested = matches!(condition, Condition::All(_) | Condition::Any(_));
                if nested {
                    sql.push("(");
                }
                condition.render(sql);
                if nested {
                    sql.push(")");
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// SQL text with its bound parameters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SqlText {
    pub text: String,
    pub params: Vec<SqlValue>,
}

impl SqlText {
    fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn bind(&mut self, value: SqlValue) {
        self.params.push(value);
        let _ = write!(self.text, "?{}", self.params.len());
    }

    fn render_where(&mut self, filters: &[Condition]) {
        if !filters.is_empty() {
            self.push(" WHERE ");
            render_joined(self, filters, " AND ", "1");
        }
    }

    fn execute(&self, writer: &Writer<'_>) -> Result<usize> {
        let mut statement = writer.connection().prepare_cached(&self.text)?;
        Ok(statement.execute(params_from_iter(self.params.iter()))?)
    }
}

// ---------------------------------------------------------------------------
// SELECT
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
#[must_use = "a query does nothing until it is fetched"]
pub struct Select {
    table: &'static str,
    columns: Vec<Expr>,
    filters: Vec<Condition>,
    group_by: Vec<Expr>,
    order_by: Vec<(Expr, Order)>,
    limit: Option<i64>,
}

impl Select {
    fn new(table: &'static str, columns: Vec<Expr>) -> Self {
        Self {
            table,
            columns,
            filters: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            limit: None,
        }
    }

    /// Adds a column or computed expression to the result.
    pub fn and_select(mut self, expr: impl Into<Expr>) -> Self {
        self.columns.push(expr.into());
        self
    }

    /// Adds a `WHERE` condition; several are combined with `AND`.
    pub fn filter(mut self, condition: Condition) -> Self {
        self.filters.push(condition);
        self
    }

    /// Adds the condition only when there is one — for optional filters.
    pub fn filter_if(mut self, condition: Option<Condition>) -> Self {
        self.filters.extend(condition);
        self
    }

    pub fn group_by(mut self, expr: impl Into<Expr>) -> Self {
        self.group_by.push(expr.into());
        self
    }

    pub fn order_by(mut self, expr: impl Into<Expr>, order: Order) -> Self {
        self.order_by.push((expr.into(), order));
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit as i64);
        self
    }

    pub fn to_sql(&self) -> SqlText {
        let mut sql = SqlText::default();
        sql.push("SELECT ");
        for (i, column) in self.columns.iter().enumerate() {
            if i > 0 {
                sql.push(", ");
            }
            column.render(&mut sql);
        }
        let _ = write!(sql.text, " FROM {}", self.table);
        sql.render_where(&self.filters);
        if !self.group_by.is_empty() {
            sql.push(" GROUP BY ");
            for (i, expr) in self.group_by.iter().enumerate() {
                if i > 0 {
                    sql.push(", ");
                }
                expr.render(&mut sql);
            }
        }
        if !self.order_by.is_empty() {
            sql.push(" ORDER BY ");
            for (i, (expr, order)) in self.order_by.iter().enumerate() {
                if i > 0 {
                    sql.push(", ");
                }
                expr.render(&mut sql);
                sql.push(match order {
                    Order::Asc => " ASC",
                    Order::Desc => " DESC",
                });
            }
        }
        if let Some(limit) = self.limit {
            sql.push(" LIMIT ");
            sql.bind(SqlValue::Integer(limit));
        }
        sql
    }

    pub fn fetch_all<T>(
        &self,
        reader: &Reader<'_>,
        mut map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let sql = self.to_sql();
        let mut statement = reader.connection().prepare_cached(&sql.text)?;
        let rows = statement.query_map(params_from_iter(sql.params.iter()), |row| map(row))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn fetch_optional<T>(
        &self,
        reader: &Reader<'_>,
        map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>> {
        Ok(self.clone().limit(1).fetch_all(reader, map)?.into_iter().next())
    }

    pub fn fetch_one<T>(
        &self,
        reader: &Reader<'_>,
        map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<T> {
        match self.fetch_optional(reader, map)? {
            Some(row) => Ok(row),
            None => bail!("expected a row from {}", self.table),
        }
    }
}

// ---------------------------------------------------------------------------
// INSERT … ON CONFLICT
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Conflict {
    DoNothing(Vec<&'static str>),
    DoUpdate(Vec<&'static str>, Vec<(&'static str, Expr)>),
}

#[derive(Clone, Debug)]
#[must_use = "an insert does nothing until it is executed"]
pub struct Insert {
    table: &'static str,
    columns: Vec<&'static str>,
    values: Vec<SqlValue>,
    conflict: Option<Conflict>,
}

impl Insert {
    fn new(table: &'static str) -> Self {
        Self {
            table,
            columns: Vec::new(),
            values: Vec::new(),
            conflict: None,
        }
    }

    pub fn value<C: Column>(mut self, column: C, value: impl IntoSql) -> Self {
        self.columns.push(column.name());
        self.values.push(value.into_sql());
        self
    }

    /// `ON CONFLICT (…) DO NOTHING`: keep the existing row.
    pub fn on_conflict_do_nothing<C: Column>(mut self, target: impl IntoIterator<Item = C>) -> Self {
        self.conflict = Some(Conflict::DoNothing(target.into_iter().map(Column::name).collect()));
        self
    }

    /// Starts an upsert: `ON CONFLICT (…) DO UPDATE SET …`. Say what happens
    /// to each column with [`Upsert`]'s methods.
    pub fn on_conflict<C: Column>(self, target: impl IntoIterator<Item = C>) -> Upsert {
        Upsert {
            insert: self,
            target: target.into_iter().map(Column::name).collect(),
            sets: Vec::new(),
        }
    }

    pub fn to_sql(&self) -> SqlText {
        let mut sql = SqlText::default();
        let _ = write!(
            sql.text,
            "INSERT INTO {} ({}) VALUES (",
            self.table,
            self.columns.join(", ")
        );
        for (i, value) in self.values.iter().enumerate() {
            if i > 0 {
                sql.push(", ");
            }
            sql.bind(value.clone());
        }
        sql.push(")");
        match &self.conflict {
            None => {}
            Some(Conflict::DoNothing(target)) => {
                let _ = write!(sql.text, " ON CONFLICT ({}) DO NOTHING", target.join(", "));
            }
            Some(Conflict::DoUpdate(target, sets)) => {
                let _ = write!(sql.text, " ON CONFLICT ({}) DO UPDATE SET ", target.join(", "));
                for (i, (column, expr)) in sets.iter().enumerate() {
                    if i > 0 {
                        sql.push(", ");
                    }
                    let _ = write!(sql.text, "{column} = ");
                    expr.render(&mut sql);
                }
            }
        }
        sql
    }

    /// Runs the insert and returns the number of rows written.
    pub fn execute(&self, writer: &Writer<'_>) -> Result<usize> {
        self.to_sql().execute(writer)
    }

    /// Runs the insert and returns the new row's id.
    pub fn execute_returning_id(&self, writer: &Writer<'_>) -> Result<i64> {
        self.execute(writer)?;
        Ok(writer.connection().last_insert_rowid())
    }
}

/// The `DO UPDATE SET` half of an upsert.
#[derive(Clone, Debug)]
#[must_use = "an upsert does nothing until it is executed"]
pub struct Upsert {
    insert: Insert,
    target: Vec<&'static str>,
    sets: Vec<(&'static str, Expr)>,
}

impl Upsert {
    /// `column = excluded.column`: overwrite with the new value.
    pub fn take_new<C: Column>(mut self, column: C) -> Self {
        self.sets.push((column.name(), column.excluded()));
        self
    }

    /// `column = COALESCE(excluded.column, table.column)`: overwrite unless
    /// the new value is `NULL`.
    pub fn keep_existing_if_null<C: Column>(mut self, column: C) -> Self {
        self.sets.push((
            column.name(),
            Expr::coalesce([column.excluded(), column.qualified()]),
        ));
        self
    }

    /// `column = <expr>`, for anything else.
    pub fn set<C: Column>(mut self, column: C, expr: Expr) -> Self {
        self.sets.push((column.name(), expr));
        self
    }

    fn finish(&self) -> Insert {
        let mut insert = self.insert.clone();
        insert.conflict = Some(Conflict::DoUpdate(self.target.clone(), self.sets.clone()));
        insert
    }

    pub fn to_sql(&self) -> SqlText {
        self.finish().to_sql()
    }

    pub fn execute(&self, writer: &Writer<'_>) -> Result<usize> {
        if self.sets.is_empty() {
            bail!("upsert into {} updates no columns", self.insert.table);
        }
        self.finish().execute(writer)
    }
}

// ---------------------------------------------------------------------------
// UPDATE / DELETE
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
#[must_use = "an update does nothing until it is executed"]
pub struct Update {
    table: &'static str,
    sets: Vec<(&'static str, Expr)>,
    filters: Vec<Condition>,
    all_rows: bool,
}

impl Update {
    fn new(table: &'static str) -> Self {
        Self {
            table,
            sets: Vec::new(),
            filters: Vec::new(),
            all_rows: false,
        }
    }

    pub fn set<C: Column>(mut self, column: C, value: impl IntoSql) -> Self {
        self.sets.push((column.name(), Expr::value(value)));
        self
    }

    pub fn set_expr<C: Column>(mut self, column: C, expr: Expr) -> Self {
        self.sets.push((column.name(), expr));
        self
    }

    pub fn filter(mut self, condition: Condition) -> Self {
        self.filters.push(condition);
        self
    }

    /// Without a filter an update refuses to run; this says touching every
    /// row really is intended.
    pub fn all_rows(mut self) -> Self {
        self.all_rows = true;
        self
    }

    pub fn to_sql(&self) -> SqlText {
        let mut sql = SqlText::default();
        let _ = write!(sql.text, "UPDATE {} SET ", self.table);
        for (i, (column, expr)) in self.sets.iter().enumerate() {
            if i > 0 {
                sql.push(", ");
            }
            let _ = write!(sql.text, "{column} = ");
            expr.render(&mut sql);
        }
        sql.render_where(&self.filters);
        sql
    }

    pub fn execute(&self, writer: &Writer<'_>) -> Result<usize> {
        if self.sets.is_empty() {
            bail!("update of {} sets no columns", self.table);
        }
        if self.filters.is_empty() && !self.all_rows {
            bail!("update of {} has no filter; call .all_rows() if that is intended", self.table);
        }
        self.to_sql().execute(writer)
    }
}

#[derive(Clone, Debug)]
#[must_use = "a delete does nothing until it is executed"]
pub struct Delete {
    table: &'static str,
    filters: Vec<Condition>,
    all_rows: bool,
}

impl Delete {
    fn new(table: &'static str) -> Self {
        Self {
            table,
            filters: Vec::new(),
            all_rows: false,
        }
    }

    pub fn filter(mut self, condition: Condition) -> Self {
        self.filters.push(condition);
        self
    }

    /// Without a filter a delete refuses to run; this says emptying the
    /// table really is intended.
    pub fn all_rows(mut self) -> Self {
        self.all_rows = true;
        self
    }

    pub fn to_sql(&self) -> SqlText {
        let mut sql = SqlText::default();
        let _ = write!(sql.text, "DELETE FROM {}", self.table);
        sql.render_where(&self.filters);
        sql
    }

    pub fn execute(&self, writer: &Writer<'_>) -> Result<usize> {
        if self.filters.is_empty() && !self.all_rows {
            bail!("delete from {} has no filter; call .all_rows() if that is intended", self.table);
        }
        self.to_sql().execute(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::table! {
        enum Things = "things" {
            Id = "id",
            Name = "name",
            Size = "size",
            Owner = "owner",
        }
    }

    crate::sql_enum! {
        enum Color {
            Red = "red",
            DarkBlue = "dark-blue",
        }
    }

    #[test]
    fn select_reads_like_sql() {
        let sql = Things::select([Things::Id, Things::Name])
            .filter(Things::Size.at_least(10))
            .filter_if(None)
            .filter(Things::Owner.is_null().or(Things::Owner.equals("me")))
            .order_by(Things::Name, Order::Asc)
            .limit(5)
            .to_sql();
        assert_eq!(
            sql.text,
            "SELECT id, name FROM things WHERE size >= ?1 AND (owner IS NULL OR owner = ?2) \
             ORDER BY name ASC LIMIT ?3"
        );
        assert_eq!(
            sql.params,
            vec![SqlValue::Integer(10), SqlValue::Text("me".into()), SqlValue::Integer(5)]
        );
    }

    #[test]
    fn aggregates_and_grouping() {
        let sql = Things::select([Expr::from(Things::Owner), Things::Size.sum_or_zero().aliased_as(Things::Size)])
            .group_by(Things::Owner)
            .order_by(Things::Size.sum(), Order::Desc)
            .to_sql();
        assert_eq!(
            sql.text,
            "SELECT owner, COALESCE(SUM(size), 0) AS size FROM things GROUP BY owner ORDER BY SUM(size) DESC"
        );
    }

    #[test]
    fn upserts_spell_out_each_column() {
        let sql = Things::insert()
            .value(Things::Id, 1)
            .value(Things::Name, "a")
            .value(Things::Owner, None::<String>)
            .on_conflict([Things::Id])
            .keep_existing_if_null(Things::Owner)
            .take_new(Things::Name)
            .to_sql();
        assert_eq!(
            sql.text,
            "INSERT INTO things (id, name, owner) VALUES (?1, ?2, ?3) ON CONFLICT (id) DO UPDATE SET \
             owner = COALESCE(excluded.owner, things.owner), name = excluded.name"
        );
        assert_eq!(sql.params[2], SqlValue::Null);
    }

    #[test]
    fn like_patterns_are_escaped_and_bound() {
        let sql = Things::select([Things::Id]).filter(Things::Name.contains("50%_off")).to_sql();
        assert_eq!(sql.text, "SELECT id FROM things WHERE name LIKE ?1 ESCAPE '\\'");
        assert_eq!(sql.params, vec![SqlValue::Text("%50\\%\\_off%".into())]);
    }

    #[test]
    fn empty_in_lists_match_nothing() {
        let sql = Things::delete().filter(Things::Id.is_in(Vec::<i64>::new())).to_sql();
        assert_eq!(sql.text, "DELETE FROM things WHERE 0");
        let sql = Things::delete().filter(Things::Id.is_in([1, 2])).to_sql();
        assert_eq!(sql.text, "DELETE FROM things WHERE id IN (?1, ?2)");
    }

    #[test]
    fn sql_enums_round_trip() {
        assert_eq!(Color::DarkBlue.as_str(), "dark-blue");
        assert_eq!(Color::parse("red"), Some(Color::Red));
        assert_eq!(Color::parse("green"), None);
        assert_eq!(Color::Red.into_sql(), SqlValue::Text("red".into()));
        assert_eq!(serde_json::to_string(&Color::DarkBlue).unwrap(), "\"dark-blue\"");
        assert_eq!(Color::ALL.len(), 2);
    }

    #[test]
    fn values_are_never_spliced_into_the_text() {
        let hostile = "x'); DROP TABLE things; --";
        let sql = Things::update()
            .set(Things::Name, hostile)
            .filter(Things::Name.equals(hostile))
            .to_sql();
        assert!(!sql.text.contains("DROP"));
        assert_eq!(sql.text, "UPDATE things SET name = ?1 WHERE name = ?2");
    }
}
