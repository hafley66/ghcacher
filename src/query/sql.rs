use anyhow::Result;
use rusqlite::Connection;
use std::io::{self, Write};

use crate::output::Format;

pub fn query_raw(conn: &Connection, sql: &str, format: Format) -> Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let col_names: Vec<String> = stmt.column_names().into_iter().map(|s| s.to_owned()).collect();

    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            let mut map = serde_json::Map::new();
            for (i, col) in col_names.iter().enumerate() {
                let val: rusqlite::types::Value = row.get(i)?;
                map.insert(col.clone(), rusqlite_to_json(val));
            }
            Ok(serde_json::Value::Object(map))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let col_refs: Vec<&str> = col_names.iter().map(|s| s.as_str()).collect();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    match format {
        Format::Json => {
            for row in &rows {
                serde_json::to_writer(&mut out, row)?;
                writeln!(out)?;
            }
        }
        Format::JsonPretty => {
            for row in &rows {
                writeln!(out, "{}", serde_json::to_string_pretty(row)?)?;
            }
        }
        Format::Tsv => {
            writeln!(out, "{}", col_refs.join("\t"))?;
            for row in &rows {
                let fields: Vec<String> = col_refs
                    .iter()
                    .map(|col| json_cell(row.get(*col)))
                    .collect();
                writeln!(out, "{}", fields.join("\t"))?;
            }
        }
        Format::Table => {
            print_table_raw(&mut out, &rows, &col_refs)?;
        }
    }

    Ok(())
}

pub fn query_view(conn: &Connection, view: &str, format: Format) -> Result<()> {
    query_raw(conn, &format!("SELECT * FROM {view}"), format)
}

fn rusqlite_to_json(val: rusqlite::types::Value) -> serde_json::Value {
    match val {
        rusqlite::types::Value::Null => serde_json::Value::Null,
        rusqlite::types::Value::Integer(i) => serde_json::Value::Number(i.into()),
        rusqlite::types::Value::Real(f) => {
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }
        rusqlite::types::Value::Text(s) => serde_json::Value::String(s),
        rusqlite::types::Value::Blob(b) => {
            serde_json::Value::String(format!("<blob {} bytes>", b.len()))
        }
    }
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

    // Header
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

    #[test]
    fn query_raw_empty_result() {
        let conn = db::open_in_memory().unwrap();
        // Should not panic on empty result
        query_raw(&conn, "SELECT * FROM repo WHERE 1=0", Format::Json).unwrap();
    }

    #[test]
    fn rusqlite_to_json_variants() {
        use rusqlite::types::Value;
        assert_eq!(rusqlite_to_json(Value::Null), serde_json::Value::Null);
        assert_eq!(rusqlite_to_json(Value::Integer(42)), serde_json::json!(42));
        assert_eq!(rusqlite_to_json(Value::Text("hi".into())), serde_json::json!("hi"));
        match rusqlite_to_json(Value::Blob(vec![1, 2, 3])) {
            serde_json::Value::String(s) => assert!(s.contains("blob")),
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn json_cell_variants() {
        assert_eq!(json_cell(None), "");
        assert_eq!(json_cell(Some(&serde_json::Value::Null)), "");
        assert_eq!(json_cell(Some(&serde_json::json!("hello"))), "hello");
        assert_eq!(json_cell(Some(&serde_json::json!(99))), "99");
    }

    #[test]
    fn query_raw_returns_rows() {
        let conn = db::open_in_memory().unwrap();
        db::upsert_repo(&conn, "o", "n", "main").unwrap();
        // Just ensure it runs without error; output goes to stdout
        query_raw(&conn, "SELECT owner, name FROM repo", Format::Json).unwrap();
    }
}
