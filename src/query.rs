//! PostgREST query parameters shared by read and write requests.

/// Sort direction for an `order=` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Ascending order (`asc`).
    Ascending,
    /// Descending order (`desc`).
    Descending,
}

impl Order {
    fn as_suffix(self) -> &'static str {
        match self {
            Order::Ascending => "asc",
            Order::Descending => "desc",
        }
    }
}

/// TableQuery parameters appended to a PostgREST table URL.
///
/// Parts are written into the URL verbatim, so they use PostgREST syntax:
/// column lists are comma separated and a filter condition carries its
/// operator, as in `eq.42` or `in.(sword,shield)`.
///
/// # Example
///
/// ```rust
/// use msg_supabase::prelude::*;
///
/// let query = TableQuery::new()
///     .select("id,score")
///     .filter("score", "gt.100")
///     .order("score", Order::Descending)
///     .limit(10);
///
/// assert_eq!(
///     query.to_query_string(),
///     "select=id,score&score=gt.100&order=score.desc&limit=10"
/// );
/// ```
#[derive(Debug, Clone, Default)]
pub struct TableQuery {
    select: Option<String>,
    on_conflict: Vec<String>,
    filters: Vec<(String, String)>,
    order: Vec<(String, Order)>,
    limit: Option<usize>,
}

impl TableQuery {
    /// Create an empty query.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the columns to return, as a PostgREST `select` list (`*` for all).
    #[must_use]
    pub fn select(mut self, columns: impl Into<String>) -> Self {
        self.select = Some(columns.into());
        self
    }

    /// Set the columns forming the `ON CONFLICT` target of an upsert.
    #[must_use]
    pub fn on_conflict<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.on_conflict = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Add a filter on `column`, where `condition` carries the PostgREST
    /// operator, as in `eq.42`.
    #[must_use]
    pub fn filter(mut self, column: impl Into<String>, condition: impl Into<String>) -> Self {
        self.filters.push((column.into(), condition.into()));
        self
    }

    /// Add an equality filter on `column`.
    #[must_use]
    pub fn eq(self, column: impl Into<String>, value: impl std::fmt::Display) -> Self {
        self.filter(column, format!("eq.{value}"))
    }

    /// Add an ordering clause. Repeated calls order by each column in turn.
    #[must_use]
    pub fn order(mut self, column: impl Into<String>, direction: Order) -> Self {
        self.order.push((column.into(), direction));
        self
    }

    /// Limit the number of returned rows.
    #[must_use]
    pub fn limit(mut self, rows: usize) -> Self {
        self.limit = Some(rows);
        self
    }

    /// Whether the query would add no parameters to the URL.
    pub fn is_empty(&self) -> bool {
        self.select.is_none()
            && self.on_conflict.is_empty()
            && self.filters.is_empty()
            && self.order.is_empty()
            && self.limit.is_none()
    }

    /// Render the parameters as a URL query string, without a leading `?`.
    pub fn to_query_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        if !self.on_conflict.is_empty() {
            parts.push(format!("on_conflict={}", self.on_conflict.join(",")));
        }
        if let Some(ref select) = self.select {
            parts.push(format!("select={select}"));
        }
        for (column, condition) in &self.filters {
            parts.push(format!("{column}={condition}"));
        }
        if !self.order.is_empty() {
            let clauses: Vec<String> = self
                .order
                .iter()
                .map(|(column, direction)| format!("{column}.{}", direction.as_suffix()))
                .collect();
            parts.push(format!("order={}", clauses.join(",")));
        }
        if let Some(limit) = self.limit {
            parts.push(format!("limit={limit}"));
        }

        parts.join("&")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_renders_nothing() {
        let query = TableQuery::new();
        assert!(query.is_empty());
        assert_eq!(query.to_query_string(), "");
    }

    #[test]
    fn on_conflict_joins_columns() {
        let query = TableQuery::new().on_conflict(["session_pk", "run_index"]);
        assert_eq!(query.to_query_string(), "on_conflict=session_pk,run_index");
    }

    #[test]
    fn on_conflict_precedes_select() {
        let query = TableQuery::new()
            .select("id,run_index")
            .on_conflict(["run_uuid"]);
        assert_eq!(
            query.to_query_string(),
            "on_conflict=run_uuid&select=id,run_index"
        );
    }

    #[test]
    fn filters_use_postgrest_operators() {
        let query = TableQuery::new().filter("kills", "gte.10").eq("won", true);
        assert_eq!(query.to_query_string(), "kills=gte.10&won=eq.true");
    }

    #[test]
    fn order_clauses_are_comma_separated() {
        let query = TableQuery::new()
            .order("kills", Order::Descending)
            .order("player_id", Order::Ascending);
        assert_eq!(query.to_query_string(), "order=kills.desc,player_id.asc");
    }

    #[test]
    fn select_all_with_limit() {
        let query = TableQuery::new().select("*").limit(1000);
        assert_eq!(query.to_query_string(), "select=*&limit=1000");
        assert!(!query.is_empty());
    }
}
