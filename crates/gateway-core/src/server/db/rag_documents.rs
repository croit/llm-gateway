// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! Document profiles, extracted document fields, and the extraction cache.
//!
//! This is the half of RAG that answers questions about *sets* of documents
//! rather than about passages. Retrieval finds the paragraph that mentions
//! ACME; only a queryable field table can answer "when did we last get an
//! invoice from ACME, and how much" — that is a superlative over a filtered
//! set, and top-k similarity over thousands of near-identical invoices
//! returns five arbitrary ones while giving the model no way to know it.
//!
//! Three pieces live here:
//!
//!   * [`Profile`] — the operator's extraction schema, in the **central** DB
//!     (`rag_document_profiles`, migration 0059). Shared across collections.
//!   * [`DocumentRow`] / [`FieldValue`] — the extracted results, in each
//!     collection's **own store** alongside its chunks, because they are
//!     regenerable per-collection state.
//!   * [`cache`] — completed extractions in the central DB, keyed by document
//!     bytes plus everything that changes the answer, so a rebuild never
//!     re-runs the LLM pass.

use std::collections::{BTreeMap, HashMap};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{DbError, Pool};

// ---- profiles (central DB) ------------------------------------------------

/// The type of one extracted field. Decides which typed column it lands in,
/// and therefore whether it can be range-filtered or sorted meaningfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,
    Number,
    Date,
    Enum,
}

impl FieldType {
    /// Which typed column of `rag_doc_fields` holds a value of this type.
    ///
    /// One place rather than a `match` at each query site: filtering,
    /// sorting and storing must agree on where a value lives, and a fourth
    /// typed column should be one edit, not four.
    pub fn value_column(self) -> &'static str {
        match self {
            FieldType::Number => "value_num",
            FieldType::Date => "value_date",
            FieldType::Text | FieldType::Enum => "value_text",
        }
    }
}

/// One field a profile asks the model to extract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub description: String,
    /// Allowed values for [`FieldType::Enum`]. Given to the model, and used
    /// to describe the field to the query tool.
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub filterable: bool,
    #[serde(default)]
    pub sortable: bool,
}

/// An operator-defined extraction schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub prompt: String,
    pub fields: Vec<ProfileField>,
    /// Part of the extraction cache key. Bumped on any semantic edit so
    /// stored fields that answered a different question are not served.
    pub version: i64,
    /// A profile shipped with the gateway. Editable and copyable like any
    /// other, but not deletable: a collection pointing at a profile that
    /// vanished indexes without fields, which is a puzzle rather than an
    /// error.
    pub builtin: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Profile {
    pub fn field(&self, key: &str) -> Option<&ProfileField> {
        self.fields.iter().find(|f| f.key == key)
    }
}

const PROFILE_COLUMNS: &str = "id, name, description, prompt, fields_json, version, \
     builtin, created_at, updated_at";

fn map_profile(row: &SqliteRow) -> Result<Profile, DbError> {
    let fields_json: String = row.try_get("fields_json")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;
    Ok(Profile {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        prompt: row.try_get("prompt")?,
        fields: serde_json::from_str(&fields_json).map_err(|e| DbError::Decode {
            column: "fields_json",
            source: anyhow::Error::from(e),
        })?,
        version: row.try_get("version")?,
        builtin: row.try_get::<i64, _>("builtin")? != 0,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

/// The seed migration writes `datetime('now')`, which is SQLite's
/// space-separated form rather than RFC 3339. Accept both so a seeded row
/// does not fail to decode.
fn parse_ts(s: &str) -> Result<Timestamp, DbError> {
    s.parse::<Timestamp>()
        .or_else(|_| format!("{}Z", s.replace(' ', "T")).parse::<Timestamp>())
        .map_err(|e: jiff::Error| DbError::Decode {
            column: "timestamp",
            source: e.into(),
        })
}

pub async fn list_profiles(pool: &Pool) -> Result<Vec<Profile>, DbError> {
    let q = format!("SELECT {PROFILE_COLUMNS} FROM rag_document_profiles ORDER BY name");
    let rows = sqlx::query(&q).fetch_all(pool).await?;
    rows.iter().map(map_profile).collect()
}

pub async fn find_profile(pool: &Pool, id: i64) -> Result<Option<Profile>, DbError> {
    let q = format!("SELECT {PROFILE_COLUMNS} FROM rag_document_profiles WHERE id = ?");
    let row = sqlx::query(&q).bind(id).fetch_optional(pool).await?;
    row.as_ref().map(map_profile).transpose()
}

pub async fn find_profile_by_name(pool: &Pool, name: &str) -> Result<Option<Profile>, DbError> {
    let q = format!("SELECT {PROFILE_COLUMNS} FROM rag_document_profiles WHERE name = ?");
    let row = sqlx::query(&q).bind(name).fetch_optional(pool).await?;
    row.as_ref().map(map_profile).transpose()
}

/// What an operator submitted for a new or edited profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInput {
    pub name: String,
    pub description: Option<String>,
    pub prompt: String,
    pub fields: Vec<ProfileField>,
}

pub async fn create_profile(pool: &Pool, input: &ProfileInput) -> Result<i64, DbError> {
    let fields_json = serde_json::to_string(&input.fields).map_err(|e| DbError::Decode {
        column: "fields_json",
        source: anyhow::Error::from(e),
    })?;
    let now = Timestamp::now().to_string();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO rag_document_profiles \
           (name, description, prompt, fields_json, version, builtin, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 1, 0, ?, ?) RETURNING id",
    )
    .bind(&input.name)
    .bind(&input.description)
    .bind(&input.prompt)
    .bind(&fields_json)
    .bind(&now)
    .bind(&now)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Save an edit, bumping the version.
///
/// The bump is not optional: `version` is part of the extraction cache key,
/// so without it every document already processed would keep serving fields
/// extracted under the *old* prompt — an edit that appears to do nothing,
/// which is the worst possible outcome for an operator trying to fix a bad
/// extraction.
pub async fn update_profile(pool: &Pool, id: i64, input: &ProfileInput) -> Result<(), DbError> {
    let fields_json = serde_json::to_string(&input.fields).map_err(|e| DbError::Decode {
        column: "fields_json",
        source: anyhow::Error::from(e),
    })?;
    sqlx::query(
        "UPDATE rag_document_profiles \
         SET name = ?, description = ?, prompt = ?, fields_json = ?, \
             version = version + 1, updated_at = ? \
         WHERE id = ?",
    )
    .bind(&input.name)
    .bind(&input.description)
    .bind(&input.prompt)
    .bind(&fields_json)
    .bind(Timestamp::now().to_string())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a custom profile. Returns false when it does not exist or is a
/// built-in.
pub async fn delete_profile(pool: &Pool, id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query("DELETE FROM rag_document_profiles WHERE id = ? AND builtin = 0")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected == 1)
}

/// Names of the collections still pointing at a profile.
///
/// Checked before a delete: removing a profile out from under a live
/// collection would leave it indexing without fields and the operator with
/// no indication why.
pub async fn collections_using_profile(
    pool: &Pool,
    profile_id: i64,
) -> Result<Vec<String>, DbError> {
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT name FROM rag_collections WHERE profile_id = ? ORDER BY name")
            .bind(profile_id)
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

// ---- extraction cache (central DB) ---------------------------------------

/// What the cache holds for one document under one profile version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedExtraction {
    pub fields: Option<BTreeMap<String, String>>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

impl CachedExtraction {
    /// A failed row is kept for the operator but reads as a miss, so a
    /// transient backend failure retries on the next pass instead of
    /// poisoning the document forever.
    pub fn hit(&self) -> Option<(&BTreeMap<String, String>, Option<&str>)> {
        match (&self.fields, &self.error) {
            (Some(fields), None) => Some((fields, self.summary.as_deref())),
            _ => None,
        }
    }
}

/// Cache identity: the document's bytes plus everything that changes what
/// comes back. Mirrors `ocr_derivatives`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionKey {
    pub doc_sha256: String,
    pub profile_id: i64,
    pub profile_version: i64,
    pub model: String,
}

pub async fn get_extraction(
    pool: &Pool,
    key: &ExtractionKey,
) -> Result<Option<CachedExtraction>, DbError> {
    let row = sqlx::query(
        "SELECT fields_json, summary, error FROM rag_extractions \
         WHERE doc_sha256 = ? AND profile_id = ? AND profile_version = ? AND model = ?",
    )
    .bind(&key.doc_sha256)
    .bind(key.profile_id)
    .bind(key.profile_version)
    .bind(&key.model)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let fields_json: Option<String> = row.try_get("fields_json")?;
    Ok(Some(CachedExtraction {
        fields: fields_json.and_then(|j| serde_json::from_str(&j).ok()),
        summary: row.try_get("summary")?,
        error: row.try_get("error")?,
    }))
}

pub async fn put_extraction(
    pool: &Pool,
    key: &ExtractionKey,
    value: &CachedExtraction,
) -> Result<(), DbError> {
    let fields_json = value
        .fields
        .as_ref()
        .and_then(|f| serde_json::to_string(f).ok());
    sqlx::query(
        "INSERT INTO rag_extractions \
           (doc_sha256, profile_id, profile_version, model, fields_json, summary, error, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (doc_sha256, profile_id, profile_version, model) DO UPDATE SET \
           fields_json = excluded.fields_json, summary = excluded.summary, \
           error = excluded.error, created_at = excluded.created_at",
    )
    .bind(&key.doc_sha256)
    .bind(key.profile_id)
    .bind(key.profile_version)
    .bind(&key.model)
    .bind(&fields_json)
    .bind(&value.summary)
    .bind(&value.error)
    .bind(Timestamp::now().to_string())
    .execute(pool)
    .await?;
    Ok(())
}

// ---- documents + fields (per-collection store) ---------------------------

/// One extracted document, joined with its file's provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentRow {
    pub id: i64,
    pub file_id: i64,
    pub path: String,
    pub web_url: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub extractor: String,
    pub pages_total: Option<i64>,
    pub pages_processed: Option<i64>,
    /// Extracted fields, rendered back to strings for display. Dates and
    /// numbers keep their normalised form.
    pub fields: BTreeMap<String, String>,
}

/// One field value on its way into the store, already normalised by the
/// extraction pass.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldValue {
    pub key: String,
    pub text: Option<String>,
    pub num: Option<f64>,
    pub date: Option<String>,
}

/// Insert (or replace) a document's extraction results.
/// Everything about one document that is not a field value. Grouped rather
/// than passed as seven positional arguments, where a caller can silently
/// swap `pages_total` and `pages_processed`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentMeta<'a> {
    pub title: Option<&'a str>,
    pub summary: Option<&'a str>,
    /// Which rung of the extraction ladder produced the text.
    pub extractor: &'a str,
    pub pages_total: Option<i64>,
    pub pages_processed: Option<i64>,
}

pub async fn upsert_document(
    pool: &Pool,
    file_id: i64,
    meta: &DocumentMeta<'_>,
    fields: &[FieldValue],
) -> Result<i64, DbError> {
    let DocumentMeta {
        title,
        summary,
        extractor,
        pages_total,
        pages_processed,
    } = *meta;
    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO rag_documents \
           (file_id, title, summary, extractor, pages_total, pages_processed, extracted_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (file_id) DO UPDATE SET \
           title = excluded.title, summary = excluded.summary, \
           extractor = excluded.extractor, pages_total = excluded.pages_total, \
           pages_processed = excluded.pages_processed, extracted_at = excluded.extracted_at \
         RETURNING id",
    )
    .bind(file_id)
    .bind(title)
    .bind(summary)
    .bind(extractor)
    .bind(pages_total)
    .bind(pages_processed)
    .bind(Timestamp::now().to_string())
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM rag_doc_fields WHERE doc_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    for f in fields {
        sqlx::query(
            "INSERT INTO rag_doc_fields (doc_id, key, value_text, value_num, value_date) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(&f.key)
        .bind(&f.text)
        .bind(f.num)
        .bind(&f.date)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// How one filter compares a field against a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// Case-insensitive substring. The default for text, because entity
    /// names never match exactly ("ACME" vs "ACME GmbH").
    Matches,
    Eq,
    Gte,
    Lte,
}

impl FilterOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "matches" | "contains" => Some(FilterOp::Matches),
            "eq" | "=" => Some(FilterOp::Eq),
            "gte" | ">=" => Some(FilterOp::Gte),
            "lte" | "<=" => Some(FilterOp::Lte),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub key: String,
    pub op: FilterOp,
    pub value: String,
    /// Which typed column to compare, from the profile's field type.
    pub field_type: FieldType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// A structured query over extracted documents.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentQuery {
    pub filters: Vec<Filter>,
    /// Restrict to documents whose path starts with this prefix.
    pub folder: Option<String>,
    pub order_by: Option<(String, FieldType, SortDir)>,
    pub limit: i64,
}

/// Build the shared WHERE fragment + bindings for a query's filters and
/// folder.
///
/// Extracted because `query_documents` and `sum_field` must select the *same*
/// set: a total that sums a different set than the listing returned is a
/// wrong number in an answer, not a crash.
fn where_clause(query: &DocumentQuery) -> (String, Vec<Bind>) {
    let mut where_parts: Vec<String> = Vec::new();
    let mut bindings: Vec<Bind> = Vec::new();
    for f in &query.filters {
        let (sql, binds) = filter_sql(f);
        where_parts.push(sql);
        bindings.extend(binds);
    }
    if let Some(folder) = &query.folder {
        // Anchored with a trailing separator, and the LIKE wildcards escaped:
        // without either, folder `Legal` also matches `LegalHold/...` and
        // `Q1_2025` matches `Q1x2025/...`, inflating the very count the tool
        // presents as authoritative.
        where_parts.push("f.path LIKE ? ESCAPE '\\'".to_string());
        bindings.push(Bind::Text(format!(
            "{}/%",
            escape_like(folder.trim_matches('/'))
        )));
    }
    let where_sql = if where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_parts.join(" AND "))
    };
    (where_sql, bindings)
}

/// Build the WHERE fragment + bindings for one filter, as an EXISTS
/// sub-select against the EAV table.
///
/// EXISTS rather than a JOIN per filter: two filters on the same document
/// must both hold for that document, and joining the EAV table twice makes
/// that easy to get subtly wrong.
fn filter_sql(f: &Filter) -> (String, Vec<Bind>) {
    let column = f.field_type.value_column();
    let numeric = matches!(f.field_type, FieldType::Number);
    let cmp = match f.op {
        FilterOp::Matches if numeric => "=",
        FilterOp::Eq => "=",
        FilterOp::Matches => "LIKE",
        FilterOp::Gte => ">=",
        FilterOp::Lte => "<=",
    };
    // A number is bound as a number. `value_num` is REAL, so a TEXT operand
    // is converted only if it happens to parse — `"1.000"` silently becomes
    // 1.0, and `"1000 EUR"` never converts at all, which in SQLite's type
    // ordering puts it above every REAL: `gte` then matches nothing and
    // `lte` matches everything. On "what did we spend", that is a confident
    // wrong total with no error anywhere.
    let value = if numeric {
        match parse_number(&f.value) {
            Some(n) => Bind::Num(n),
            // Unparseable: match nothing rather than guess. `Filter::new`
            // rejects these up front, so reaching here means a caller built
            // a `Filter` by hand.
            None => return ("0 = 1".to_string(), Vec::new()),
        }
    } else if f.op == FilterOp::Matches {
        Bind::Text(format!("%{}%", f.value.to_lowercase()))
    } else {
        // Lowercased on both sides: a vendor typed as "acme" must match a
        // document that spells it "ACME GmbH". Ordering comparisons too —
        // otherwise `vendor gte "M"` compares 'acme gmbh' >= 'M', and every
        // lowercase ASCII letter sorts above every uppercase one, so the
        // filter matches everything.
        Bind::Text(f.value.to_lowercase())
    };
    let lhs = if numeric {
        column.to_string()
    } else {
        format!("LOWER({column})")
    };
    (
        format!(
            "EXISTS (SELECT 1 FROM rag_doc_fields df WHERE df.doc_id = d.id \
             AND df.key = ? AND {lhs} {cmp} ?)"
        ),
        vec![Bind::Text(f.key.clone()), value],
    )
}

/// Escape the LIKE wildcards in a literal, for use with `ESCAPE '\\'`.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// One bound query parameter, carrying its SQL type.
///
/// The typed columns of `rag_doc_fields` have real affinities; binding
/// everything as text makes SQLite's conversion rules decide the answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Bind {
    Text(String),
    Num(f64),
}

/// Parse a number a person or a model might have written.
///
/// Accepts a plain decimal and thousands-separated forms (`1,234.56`), and
/// refuses anything with trailing units or an ambiguous separator — a filter
/// that cannot be read as a number must be an error, never a silent match.
pub fn parse_number(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(n) = raw.parse::<f64>() {
        return n.is_finite().then_some(n);
    }
    if !raw.contains(',') {
        return None;
    }
    let (int_part, frac_part) = match raw.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (raw, None),
    };
    if frac_part.is_some_and(|f| f.contains(',')) {
        return None;
    }
    let groups: Vec<&str> = int_part.split(',').collect();
    if groups.len() < 2 {
        return None;
    }
    let head = groups[0].strip_prefix('-').unwrap_or(groups[0]);
    let head_ok = !head.is_empty() && head.len() <= 3 && head.chars().all(|c| c.is_ascii_digit());
    let rest_ok = groups[1..]
        .iter()
        .all(|g| g.len() == 3 && g.chars().all(|c| c.is_ascii_digit()));
    if !head_ok || !rest_ok {
        return None;
    }
    let joined = match frac_part {
        Some(f) => format!("{}.{f}", int_part.replace(',', "")),
        None => int_part.replace(',', ""),
    };
    joined.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// Run a structured query. Returns the matching documents plus the total
/// number that matched before `limit` — the caller needs that so a model
/// cannot conclude "we received 10 invoices" when it was handed the first 10
/// of 47.
pub async fn query_documents(
    pool: &Pool,
    query: &DocumentQuery,
) -> Result<(Vec<DocumentRow>, i64), DbError> {
    let (where_sql, mut bindings) = where_clause(query);

    let count_sql = format!(
        "SELECT COUNT(*) FROM rag_documents d JOIN rag_files f ON f.id = d.file_id {where_sql}"
    );
    let mut count_q = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &bindings {
        count_q = match b {
            Bind::Text(t) => count_q.bind(t),
            Bind::Num(n) => count_q.bind(n),
        };
    }
    let total: i64 = count_q.fetch_one(pool).await?;

    let order_sql = match &query.order_by {
        Some((key, ty, dir)) => {
            let column = ty.value_column();
            let dir = match dir {
                SortDir::Asc => "ASC",
                SortDir::Desc => "DESC",
            };
            // NULLs last in both directions: a document missing the sort key
            // is not "the oldest", it is unknown, and surfacing it at the top
            // of a "most recent" answer would be actively misleading.
            bindings.push(Bind::Text(key.clone()));
            format!(
                "ORDER BY (SELECT {column} FROM rag_doc_fields df \
                 WHERE df.doc_id = d.id AND df.key = ?) IS NULL ASC, \
                 (SELECT {column} FROM rag_doc_fields df2 \
                 WHERE df2.doc_id = d.id AND df2.key = ?) {dir}"
            )
        }
        None => "ORDER BY f.path ASC".to_string(),
    };
    // The ORDER BY sub-selects bind the key twice.
    if let Some((key, _, _)) = &query.order_by {
        bindings.push(Bind::Text(key.clone()));
    }

    let sql = format!(
        "SELECT d.id, d.file_id, f.path, f.web_url, d.title, d.summary, d.extractor, \
                d.pages_total, d.pages_processed \
         FROM rag_documents d JOIN rag_files f ON f.id = d.file_id \
         {where_sql} {order_sql} LIMIT ?"
    );
    let mut q = sqlx::query(&sql);
    for b in &bindings {
        q = match b {
            Bind::Text(t) => q.bind(t),
            Bind::Num(n) => q.bind(n),
        };
    }
    q = q.bind(query.limit.clamp(1, 200));
    let rows = q.fetch_all(pool).await?;

    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<_, _>>()?;
    let mut fields = fields_for_many(pool, &ids).await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.try_get("id")?;
        out.push(DocumentRow {
            id,
            file_id: row.try_get("file_id")?,
            path: row.try_get("path")?,
            web_url: row.try_get("web_url")?,
            title: row.try_get("title")?,
            summary: row.try_get("summary")?,
            extractor: row.try_get("extractor")?,
            pages_total: row.try_get("pages_total")?,
            pages_processed: row.try_get("pages_processed")?,
            fields: fields.remove(&id).unwrap_or_default(),
        });
    }
    Ok((out, total))
}

/// Fields for a whole page of documents in one round trip.
///
/// One query per row would be up to 200 of them per `rag_query_documents`
/// call, and that call is on the model's request path — it runs again every
/// turn.
async fn fields_for_many(
    pool: &Pool,
    doc_ids: &[i64],
) -> Result<HashMap<i64, BTreeMap<String, String>>, DbError> {
    let mut out: HashMap<i64, BTreeMap<String, String>> = HashMap::new();
    if doc_ids.is_empty() {
        return Ok(out);
    }
    let placeholders = std::iter::repeat_n("?", doc_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT doc_id, key, value_text, value_num, value_date          FROM rag_doc_fields WHERE doc_id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql);
    for id in doc_ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await?;
    for row in &rows {
        let doc_id: i64 = row.try_get("doc_id")?;
        let key: String = row.try_get("key")?;
        let text: Option<String> = row.try_get("value_text")?;
        let num: Option<f64> = row.try_get("value_num")?;
        let date: Option<String> = row.try_get("value_date")?;
        let value = text.or(date).or_else(|| num.map(format_number));
        if let Some(v) = value {
            out.entry(doc_id).or_default().insert(key, v);
        }
    }
    Ok(out)
}

/// Render a stored number without a trailing `.0` on whole values — an
/// invoice total of `1200` should not read as `1200.0` in an answer.
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// The distinct values a text field takes among documents matching a filter.
///
/// This is what lets a model notice that "ACME" matched both `ACME GmbH` and
/// `ACME Deutschland AG` and ask which was meant, instead of silently
/// answering about one of them. Entity resolution is genuinely hard and a
/// legal-suffix list would be a language-specific word list, which this
/// product does not do.
pub async fn distinct_values(
    pool: &Pool,
    query: &DocumentQuery,
    key: &str,
    like: &str,
) -> Result<Vec<String>, DbError> {
    // Scoped to the same documents the query matched. Without the join and
    // the shared WHERE this answered over the whole corpus, so a question
    // already narrowed to `doc_type=invoice, folder=Finance` still reported
    // the near-miss vendor from an unrelated contract in `Legal/` — and the
    // tool tells the model to stop and ask which one was meant. A spurious
    // clarification loop on an unambiguous question.
    let (where_sql, bindings) = where_clause(query);
    let sql = format!(
        "SELECT DISTINCT df.value_text FROM rag_doc_fields df \
         JOIN rag_documents d ON d.id = df.doc_id \
         JOIN rag_files f ON f.id = d.file_id \
         {where_sql} \
         {and_or_where} df.key = ? AND df.value_text IS NOT NULL \
           AND LOWER(df.value_text) LIKE ? \
         ORDER BY df.value_text LIMIT 25",
        and_or_where = if where_sql.is_empty() { "WHERE" } else { "AND" }
    );
    let mut q = sqlx::query(&sql);
    for b in &bindings {
        q = match b {
            Bind::Text(t) => q.bind(t),
            Bind::Num(n) => q.bind(n),
        };
    }
    let rows = q
        .bind(key)
        .bind(format!("%{}%", like.to_lowercase()))
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| r.try_get::<String, _>("value_text").map_err(DbError::from))
        .collect()
}

/// Aggregate one numeric field over the documents a query matches.
pub async fn sum_field(
    pool: &Pool,
    query: &DocumentQuery,
    key: &str,
) -> Result<Option<f64>, DbError> {
    let (where_sql, bindings) = where_clause(query);
    let sql = format!(
        "SELECT SUM((SELECT value_num FROM rag_doc_fields df WHERE df.doc_id = d.id \
                     AND df.key = ?)) \
         FROM rag_documents d JOIN rag_files f ON f.id = d.file_id {where_sql}"
    );
    let mut q = sqlx::query_scalar::<_, Option<f64>>(&sql).bind(key);
    for b in &bindings {
        q = match b {
            Bind::Text(t) => q.bind(t),
            Bind::Num(n) => q.bind(n),
        };
    }
    Ok(q.fetch_one(pool).await?)
}

/// Full text of one document, reassembled from its chunks in order.
pub async fn document_text(pool: &Pool, doc_id: i64) -> Result<String, DbError> {
    let rows = sqlx::query(
        "SELECT c.content FROM rag_chunks c \
         JOIN rag_documents d ON d.file_id = c.file_id \
         WHERE d.id = ? ORDER BY c.chunk_index",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await?;
    let mut out = String::new();
    for row in &rows {
        let content: String = row.try_get("content")?;
        append_without_overlap(&mut out, &content);
    }
    Ok(out)
}

/// Longest run of characters, up to [`MAX_OVERLAP_CHARS`], that `next` repeats
/// from the end of `out`.
const MAX_OVERLAP_CHARS: usize = 2048;

/// Append `next`, dropping the part it repeats from the end of `out`.
///
/// The chunker overlaps consecutive chunks by `chunk_overlap` so a passage
/// straddling a boundary is retrievable from either side. That is right for
/// retrieval and wrong for reassembly: concatenating the chunks verbatim
/// repeats every boundary, and `rag_fetch_document` presents the result as
/// "the full extracted text". A model asked to total invoice line items then
/// counts anything near a boundary twice.
///
/// The overlap length is not recorded per chunk, so it is measured: the
/// longest suffix of what we have that is also a prefix of what comes next.
/// Bounded, because an unbounded search is quadratic in the document.
fn append_without_overlap(out: &mut String, next: &str) {
    if out.is_empty() {
        out.push_str(next);
        return;
    }
    let cap = next
        .char_indices()
        .map(|(i, _)| i)
        .take(MAX_OVERLAP_CHARS + 1)
        .last()
        .unwrap_or(0);
    let mut overlap = 0usize;
    for (i, _) in next[..cap].char_indices().skip(1) {
        if out.ends_with(&next[..i]) {
            overlap = i;
        }
    }
    if out.ends_with(&next[..cap]) {
        overlap = cap;
    }
    let rest = &next[overlap..];
    if !rest.is_empty() {
        if overlap == 0 && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(rest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db;

    async fn store() -> Pool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rag.sqlite");
        let pool = db::open_collection_store(&path).await.unwrap();
        // Keep the tempdir alive for the process: the pool holds the file
        // open and the test only needs it until it drops.
        std::mem::forget(dir);
        pool
    }

    async fn seed_file(pool: &Pool, path: &str, web_url: Option<&str>) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO rag_files (collection_id, path, content_hash, indexed_at, web_url) \
             VALUES (1, ?, 'h', '2026-01-01T00:00:00Z', ?) RETURNING id",
        )
        .bind(path)
        .bind(web_url)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn text(key: &str, v: &str) -> FieldValue {
        FieldValue {
            key: key.into(),
            text: Some(v.into()),
            num: None,
            date: None,
        }
    }

    fn date(key: &str, v: &str) -> FieldValue {
        FieldValue {
            key: key.into(),
            text: None,
            num: None,
            date: Some(v.into()),
        }
    }

    fn num(key: &str, v: f64) -> FieldValue {
        FieldValue {
            key: key.into(),
            text: None,
            num: Some(v),
            date: None,
        }
    }

    async fn seed_invoice(pool: &Pool, path: &str, vendor: &str, day: &str, total: f64) -> i64 {
        let file_id = seed_file(pool, path, Some("https://cloud.example.com/f/1")).await;
        upsert_document(
            pool,
            file_id,
            &DocumentMeta {
                title: Some(path),
                summary: Some("an invoice"),
                extractor: "ocr",
                pages_total: Some(1),
                pages_processed: Some(1),
            },
            &[
                text("vendor", vendor),
                text("doc_type", "invoice"),
                date("doc_date", day),
                num("total_gross", total),
            ],
        )
        .await
        .unwrap()
    }

    fn query(filters: Vec<Filter>) -> DocumentQuery {
        DocumentQuery {
            filters,
            folder: None,
            order_by: None,
            limit: 50,
        }
    }

    fn vendor_filter(v: &str) -> Filter {
        Filter {
            key: "vendor".into(),
            op: FilterOp::Matches,
            value: v.into(),
            field_type: FieldType::Text,
        }
    }

    #[tokio::test]
    async fn the_latest_invoice_from_a_vendor_is_a_sort_not_a_similarity_search() {
        let pool = store().await;
        seed_invoice(&pool, "a.pdf", "ACME GmbH", "2025-03-01", 100.0).await;
        seed_invoice(&pool, "b.pdf", "ACME GmbH", "2025-11-04", 1234.56).await;
        seed_invoice(&pool, "c.pdf", "Globex Ltd", "2025-12-01", 999.0).await;

        let mut q = query(vec![vendor_filter("acme")]);
        q.order_by = Some(("doc_date".into(), FieldType::Date, SortDir::Desc));
        q.limit = 1;
        let (docs, total) = query_documents(&pool, &q).await.unwrap();

        assert_eq!(total, 2, "the other vendor's invoice is excluded");
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "b.pdf", "the most recent ACME invoice");
        assert_eq!(docs[0].fields.get("doc_date").unwrap(), "2025-11-04");
        assert_eq!(
            docs[0].fields.get("total_gross").unwrap(),
            "1234.56",
            "the amount comes back exactly as extracted"
        );
    }

    #[tokio::test]
    async fn the_total_count_survives_the_limit() {
        let pool = store().await;
        for i in 0..5 {
            seed_invoice(&pool, &format!("{i}.pdf"), "ACME GmbH", "2025-01-01", 10.0).await;
        }
        let mut q = query(vec![vendor_filter("acme")]);
        q.limit = 2;
        let (docs, total) = query_documents(&pool, &q).await.unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(
            total, 5,
            "a model handed 2 of 5 must be able to tell that it was 5"
        );
    }

    #[tokio::test]
    async fn a_vendor_filter_is_case_insensitive_and_partial() {
        let pool = store().await;
        seed_invoice(&pool, "a.pdf", "ACME GmbH", "2025-01-01", 10.0).await;
        let (docs, _) = query_documents(&pool, &query(vec![vendor_filter("acme gmbh")]))
            .await
            .unwrap();
        assert_eq!(docs.len(), 1);
        let (docs, _) = query_documents(&pool, &query(vec![vendor_filter("ACME")]))
            .await
            .unwrap();
        assert_eq!(docs.len(), 1);
    }

    #[tokio::test]
    async fn filters_combine_with_and_not_or() {
        let pool = store().await;
        seed_invoice(&pool, "a.pdf", "ACME GmbH", "2025-03-01", 10.0).await;
        seed_invoice(&pool, "b.pdf", "Globex Ltd", "2025-11-01", 20.0).await;
        let q = query(vec![
            vendor_filter("acme"),
            Filter {
                key: "doc_date".into(),
                op: FilterOp::Gte,
                value: "2025-06-01".into(),
                field_type: FieldType::Date,
            },
        ]);
        let (docs, total) = query_documents(&pool, &q).await.unwrap();
        assert_eq!(total, 0, "ACME's invoice is too old and Globex is not ACME");
        assert!(docs.is_empty());
    }

    #[tokio::test]
    async fn a_date_range_filter_compares_chronologically() {
        let pool = store().await;
        seed_invoice(&pool, "a.pdf", "ACME", "2025-03-01", 10.0).await;
        seed_invoice(&pool, "b.pdf", "ACME", "2025-11-04", 20.0).await;
        let q = query(vec![Filter {
            key: "doc_date".into(),
            op: FilterOp::Gte,
            value: "2025-06-01".into(),
            field_type: FieldType::Date,
        }]);
        let (docs, _) = query_documents(&pool, &q).await.unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "b.pdf");
    }

    #[tokio::test]
    async fn summing_a_numeric_field_answers_how_much_in_total() {
        let pool = store().await;
        seed_invoice(&pool, "a.pdf", "ACME", "2025-03-01", 100.5).await;
        seed_invoice(&pool, "b.pdf", "ACME", "2025-11-04", 200.25).await;
        seed_invoice(&pool, "c.pdf", "Globex", "2025-11-04", 999.0).await;
        let sum = sum_field(&pool, &query(vec![vendor_filter("acme")]), "total_gross")
            .await
            .unwrap();
        assert_eq!(sum, Some(300.75));
    }

    #[tokio::test]
    async fn documents_missing_the_sort_key_never_lead_a_most_recent_answer() {
        let pool = store().await;
        seed_invoice(&pool, "dated.pdf", "ACME", "2025-01-01", 10.0).await;
        // A document whose date the extractor could not find.
        let file_id = seed_file(&pool, "undated.pdf", None).await;
        upsert_document(
            &pool,
            file_id,
            &DocumentMeta {
                extractor: "ocr",
                ..Default::default()
            },
            &[text("vendor", "ACME")],
        )
        .await
        .unwrap();

        let mut q = query(vec![vendor_filter("acme")]);
        q.order_by = Some(("doc_date".into(), FieldType::Date, SortDir::Desc));
        let (docs, _) = query_documents(&pool, &q).await.unwrap();
        assert_eq!(
            docs[0].path, "dated.pdf",
            "an unknown date is not the newest date"
        );
    }

    /// Reassembled text must not repeat the chunker's overlap.
    ///
    /// Regression: chunks overlap by `chunk_overlap` so a passage straddling
    /// a boundary stays retrievable, and `document_text` concatenated them
    /// verbatim — so every boundary appeared twice in what
    /// `rag_fetch_document` calls the full text. A model totalling line items
    /// double-counted anything near one.
    #[test]
    fn reassembly_drops_the_chunk_overlap() {
        let mut out = String::new();
        append_without_overlap(&mut out, "alpha beta gamma");
        append_without_overlap(&mut out, "beta gamma delta");
        assert_eq!(out, "alpha beta gamma delta");

        // Chunks that genuinely share nothing are still separated.
        let mut out = String::new();
        append_without_overlap(&mut out, "first");
        append_without_overlap(&mut out, "second");
        assert_eq!(out, "first\nsecond");
    }

    /// The overlap scan must never split a character.
    #[test]
    fn reassembly_is_safe_across_multibyte_boundaries() {
        let mut out = String::new();
        append_without_overlap(&mut out, "Rechnungsprüfung über 100 €");
        append_without_overlap(&mut out, "über 100 € netto");
        assert_eq!(out, "Rechnungsprüfung über 100 € netto");
    }

    #[tokio::test]
    async fn distinct_values_expose_the_ambiguity_rather_than_hiding_it() {
        let pool = store().await;
        seed_invoice(&pool, "a.pdf", "ACME GmbH", "2025-01-01", 10.0).await;
        seed_invoice(&pool, "b.pdf", "ACME Deutschland AG", "2025-02-01", 20.0).await;
        let all = DocumentQuery {
            filters: vec![],
            folder: None,
            order_by: None,
            limit: 10,
        };
        let values = distinct_values(&pool, &all, "vendor", "acme")
            .await
            .unwrap();
        assert_eq!(values.len(), 2, "the model gets to notice these are two");
        assert!(values.contains(&"ACME GmbH".to_string()));
    }

    /// ...but only among the documents the query actually matched.
    ///
    /// Regression: this ignored the query entirely and answered over the whole
    /// corpus, so a question already scoped to one folder still surfaced a
    /// near-miss name from another — and the tool tells the model to stop and
    /// ask which was meant. A clarification loop on an unambiguous question.
    #[tokio::test]
    async fn distinct_values_are_scoped_to_the_query() {
        let pool = store().await;
        seed_invoice(&pool, "Finance/a.pdf", "ACME GmbH", "2025-01-01", 10.0).await;
        seed_invoice(
            &pool,
            "Legal/b.pdf",
            "ACME Deutschland AG",
            "2025-02-01",
            20.0,
        )
        .await;

        let scoped = DocumentQuery {
            filters: vec![],
            folder: Some("Finance".into()),
            order_by: None,
            limit: 10,
        };
        let values = distinct_values(&pool, &scoped, "vendor", "acme")
            .await
            .unwrap();
        assert_eq!(
            values,
            vec!["ACME GmbH".to_string()],
            "the contract in Legal/ is not an ambiguity for a question about Finance/"
        );
    }

    #[tokio::test]
    async fn a_folder_scope_restricts_by_path_prefix() {
        let pool = store().await;
        seed_invoice(&pool, "Finance/2025/a.pdf", "ACME", "2025-01-01", 10.0).await;
        seed_invoice(&pool, "Projects/orion/b.pdf", "ACME", "2025-02-01", 20.0).await;
        let mut q = query(vec![]);
        q.folder = Some("/Finance".into());
        let (docs, total) = query_documents(&pool, &q).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(docs[0].path, "Finance/2025/a.pdf");
    }

    #[tokio::test]
    async fn re_extracting_replaces_fields_rather_than_accumulating_them() {
        let pool = store().await;
        let file_id = seed_file(&pool, "a.pdf", None).await;
        upsert_document(
            &pool,
            file_id,
            &DocumentMeta {
                extractor: "ocr",
                ..Default::default()
            },
            &[text("vendor", "Old Name")],
        )
        .await
        .unwrap();
        upsert_document(
            &pool,
            file_id,
            &DocumentMeta {
                extractor: "ocr",
                ..Default::default()
            },
            &[text("vendor", "New Name")],
        )
        .await
        .unwrap();
        let (docs, total) = query_documents(&pool, &query(vec![])).await.unwrap();
        assert_eq!(total, 1, "one document, not two");
        assert_eq!(docs[0].fields.get("vendor").unwrap(), "New Name");
    }

    #[tokio::test]
    async fn whole_numbers_render_without_a_decimal_tail() {
        let pool = store().await;
        seed_invoice(&pool, "a.pdf", "ACME", "2025-01-01", 1200.0).await;
        let (docs, _) = query_documents(&pool, &query(vec![])).await.unwrap();
        assert_eq!(docs[0].fields.get("total_gross").unwrap(), "1200");
    }

    #[test]
    fn a_failed_cached_extraction_reads_as_a_miss() {
        let failed = CachedExtraction {
            fields: None,
            summary: None,
            error: Some("upstream 503".into()),
        };
        assert!(
            failed.hit().is_none(),
            "a transient failure must retry, not be served forever"
        );
        let ok = CachedExtraction {
            fields: Some(BTreeMap::new()),
            summary: Some("s".into()),
            error: None,
        };
        assert!(ok.hit().is_some());
    }

    #[test]
    fn filter_ops_accept_both_spellings() {
        assert_eq!(FilterOp::parse("gte"), Some(FilterOp::Gte));
        assert_eq!(FilterOp::parse(">="), Some(FilterOp::Gte));
        assert_eq!(FilterOp::parse("nonsense"), None);
    }

    /// A thousands-separated amount must compare as a number.
    ///
    /// Regression: every filter value was bound as TEXT while `value_num` is
    /// REAL, so SQLite converted only what happened to parse. `"1.000"`
    /// became 1.0 and matched every invoice over one euro; `"1000 EUR"` never
    /// converted at all and, since REAL sorts below TEXT unconditionally,
    /// `gte` matched nothing while `lte` matched everything. The tool this
    /// backs answers "how much did we spend".
    #[tokio::test]
    async fn a_numeric_filter_compares_numerically() {
        assert_eq!(parse_number("1000"), Some(1000.0));
        assert_eq!(parse_number("1,234.56"), Some(1234.56));
        assert_eq!(
            parse_number("1.000"),
            Some(1.0),
            "a dot is a decimal point here; the ambiguity is why the model is \
             told to send plain decimals"
        );
        assert_eq!(
            parse_number("1000 EUR"),
            None,
            "a unit-suffixed amount is refused, never silently compared as text"
        );
        assert_eq!(parse_number("1'000"), None);
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("abc"), None);

        // And the binding itself carries the type.
        let f = Filter {
            key: "total_gross".into(),
            op: FilterOp::Gte,
            value: "1,000".into(),
            field_type: FieldType::Number,
        };
        let (_, binds) = filter_sql(&f);
        assert_eq!(
            binds[1],
            Bind::Num(1000.0),
            "bound as a number, so SQLite does not have to guess"
        );

        // An unreadable amount matches nothing rather than everything.
        let bad = Filter {
            key: "total_gross".into(),
            op: FilterOp::Gte,
            value: "1000 EUR".into(),
            field_type: FieldType::Number,
        };
        let (sql, binds) = filter_sql(&bad);
        assert_eq!(sql, "0 = 1");
        assert!(binds.is_empty());
    }

    /// A folder scope must not leak into a sibling whose name starts the same.
    ///
    /// Regression: `f.path LIKE 'Legal%'` with no trailing separator also
    /// matched `LegalHold/confidential/...`, and `_` is a LIKE wildcard so
    /// `Q1_2025` matched `Q1x2025/...`. The folder is model-supplied and the
    /// over-match inflates the total the tool presents as authoritative.
    #[test]
    fn a_folder_scope_is_anchored_and_its_wildcards_escaped() {
        let q = DocumentQuery {
            filters: vec![],
            folder: Some("Legal".into()),
            order_by: None,
            limit: 10,
        };
        let (sql, binds) = where_clause(&q);
        assert!(sql.contains("ESCAPE"), "wildcards are escaped: {sql}");
        assert_eq!(
            binds[0],
            Bind::Text("Legal/%".into()),
            "anchored with a separator, so LegalHold is not inside Legal"
        );

        let q = DocumentQuery {
            filters: vec![],
            folder: Some("Q1_2025".into()),
            order_by: None,
            limit: 10,
        };
        let (_, binds) = where_clause(&q);
        assert_eq!(
            binds[0],
            Bind::Text("Q1\\_2025/%".into()),
            "`_` is a LIKE wildcard and must not match any character"
        );
    }

    /// Ordering comparisons on text lowercase both sides, like equality does.
    ///
    /// Regression: `gte`/`lte` kept the caller's value verbatim against a
    /// `LOWER(value_text)` left-hand side, so `vendor gte "M"` compared
    /// `'acme gmbh' >= 'M'` — every lowercase ASCII letter sorts above every
    /// uppercase one, so the filter matched everything.
    #[test]
    fn a_text_range_filter_lowercases_both_sides() {
        let f = Filter {
            key: "vendor".into(),
            op: FilterOp::Gte,
            value: "M".into(),
            field_type: FieldType::Text,
        };
        let (sql, binds) = filter_sql(&f);
        assert!(sql.contains("LOWER(value_text)"));
        assert_eq!(binds[1], Bind::Text("m".into()));
    }
}
