use anyhow::Result;
use sqlx::{Column, Row, SqlitePool, TypeInfo, ValueRef};
use std::io::{self, Write};

use crate::output::Format;

pub async fn query_raw(pool: &SqlitePool, sql: &str, format: Format) -> Result<()> {
    let rows: Vec<sqlx::sqlite::SqliteRow> = sqlx::query(sql)
        .fetch_all(pool)
        .await?;

    let col_names: Vec<String> = match rows.first() {
        Some(row) => row.columns().iter().map(|c| c.name().to_owned()).collect(),
        None => return Ok(()),
    };

    let json_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut map = serde_json::Map::new();
            for (i, col) in col_names.iter().enumerate() {
                let (is_null, type_name) = {
                    let raw = row.try_get_raw(i).unwrap();
                    (raw.is_null(), raw.type_info().name().to_owned())
                };
                let val = if is_null {
                    serde_json::Value::Null
                } else {
                    match type_name.as_str() {
                        "INTEGER" => row
                            .try_get::<i64, _>(i)
                            .map(|v| serde_json::Value::Number(v.into()))
                            .unwrap_or(serde_json::Value::Null),
                        "REAL" => row
                            .try_get::<f64, _>(i)
                            .ok()
                            .and_then(|f| serde_json::Number::from_f64(f).map(serde_json::Value::Number))
                            .unwrap_or(serde_json::Value::Null),
                        "BLOB" => row
                            .try_get::<Vec<u8>, _>(i)
                            .map(|b| serde_json::Value::String(format!("<blob {} bytes>", b.len())))
                            .unwrap_or(serde_json::Value::Null),
                        _ => row
                            .try_get::<String, _>(i)
                            .map(serde_json::Value::String)
                            .unwrap_or(serde_json::Value::Null),
                    }
                };
                map.insert(col.clone(), val);
            }
            serde_json::Value::Object(map)
        })
        .collect();

    let col_refs: Vec<&str> = col_names.iter().map(|s| s.as_str()).collect();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    match format {
        Format::Json => {
            for row in &json_rows {
                serde_json::to_writer(&mut out, row)?;
                writeln!(out)?;
            }
        }
        Format::JsonPretty => {
            for row in &json_rows {
                writeln!(out, "{}", serde_json::to_string_pretty(row)?)?;
            }
        }
        Format::Tsv => {
            writeln!(out, "{}", col_refs.join("\t"))?;
            for row in &json_rows {
                let fields: Vec<String> = col_refs
                    .iter()
                    .map(|col| json_cell(row.get(*col)))
                    .collect();
                writeln!(out, "{}", fields.join("\t"))?;
            }
        }
        Format::Table => {
            print_table_raw(&mut out, &json_rows, &col_refs)?;
        }
    }

    Ok(())
}

pub async fn query_view(pool: &SqlitePool, view: &str, format: Format) -> Result<()> {
    query_raw(pool, &format!("SELECT * FROM {view}"), format).await
}

fn json_cell(v: Option<&serde_json::Value>) -> String {
    match v {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn print_table_raw(out: &mut impl Write, rows: &[serde_json::Value], columns: &[&str]) -> Result<()> {
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, col) in columns.iter().enumerate() {
            let s = json_cell(row.get(*col));
            widths[i] = widths[i].max(s.len().min(60));
        }
    }

    let header: Vec<String> = columns.iter().zip(&widths).map(|(c, w)| format!("{:<w$}", c, w = w)).collect();
    writeln!(out, "{}", header.join("  "))?;
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    writeln!(out, "{}", sep.join("  "))?;

    for row in rows {
        let cells: Vec<String> = columns.iter().zip(&widths).map(|(col, w)| {
            let s = json_cell(row.get(*col));
            let s = if s.len() > *w { format!("{}…", &s[..*w - 1]) } else { s };
            format!("{:<w$}", s, w = w)
        }).collect();
        writeln!(out, "{}", cells.join("  "))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn query_raw_empty_result() {
        let pool = db::open_in_memory().await.unwrap();
        query_raw(&pool, "SELECT * FROM repo WHERE 1=0", Format::Json).await.unwrap();
    }

    #[test]
    fn json_cell_variants() {
        assert_eq!(json_cell(None), "");
        assert_eq!(json_cell(Some(&serde_json::Value::Null)), "");
        assert_eq!(json_cell(Some(&serde_json::json!("hello"))), "hello");
        assert_eq!(json_cell(Some(&serde_json::json!(99))), "99");
    }

    #[tokio::test]
    async fn query_raw_returns_rows() {
        let pool = db::open_in_memory().await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        db::upsert_repo(&mut *conn, "o", "n", "main").await.unwrap();
        drop(conn);
        query_raw(&pool, "SELECT owner, name FROM repo", Format::Json).await.unwrap();
    }
}
