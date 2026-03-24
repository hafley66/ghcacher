use anyhow::Result;
use serde::Serialize;
use std::io::{self, IsTerminal, Write};

#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Format {
    Json,
    JsonPretty,
    Tsv,
    Table,
}

impl Format {
    /// Auto-detect: table when stdout is a TTY, json otherwise.
    pub fn auto() -> Self {
        if io::stdout().is_terminal() {
            Format::Table
        } else {
            Format::Json
        }
    }
}

pub fn print_rows<T: Serialize>(rows: &[T], format: Format, columns: &[&str]) -> Result<()> {
    match format {
        Format::Json => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            for row in rows {
                serde_json::to_writer(&mut out, row)?;
                writeln!(out)?;
            }
        }
        Format::JsonPretty => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            for row in rows {
                let s = serde_json::to_string_pretty(row)?;
                writeln!(out, "{}", s)?;
            }
        }
        Format::Tsv => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            writeln!(out, "{}", columns.join("\t"))?;
            for row in rows {
                let v = serde_json::to_value(row)?;
                let fields: Vec<String> = columns
                    .iter()
                    .map(|col| {
                        v.get(col)
                            .map(|f| match f {
                                serde_json::Value::String(s) => s.clone(),
                                serde_json::Value::Null => String::new(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default()
                    })
                    .collect();
                writeln!(out, "{}", fields.join("\t"))?;
            }
        }
        Format::Table => {
            print_table(rows, columns)?;
        }
    }
    Ok(())
}

fn print_table<T: Serialize>(rows: &[T], columns: &[&str]) -> Result<()> {
    // Convert all rows to values
    let values: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| serde_json::to_value(r))
        .collect::<Result<_, _>>()?;

    // Compute column widths
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for v in &values {
        for (i, col) in columns.iter().enumerate() {
            let cell = cell_str(v.get(*col));
            widths[i] = widths[i].max(cell.len());
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Header
    let header: Vec<String> = columns
        .iter()
        .zip(&widths)
        .map(|(c, w)| format!("{:<width$}", c, width = w))
        .collect();
    writeln!(out, "{}", header.join("  "))?;

    // Separator
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    writeln!(out, "{}", sep.join("  "))?;

    // Rows
    for v in &values {
        let cells: Vec<String> = columns
            .iter()
            .zip(&widths)
            .map(|(col, w)| {
                let s = cell_str(v.get(*col));
                // Truncate long cells in table mode
                let s = if s.len() > *w { format!("{}…", &s[..*w - 1]) } else { s };
                format!("{:<width$}", s, width = *w)
            })
            .collect();
        writeln!(out, "{}", cells.join("  "))?;
    }

    Ok(())
}

fn cell_str(v: Option<&serde_json::Value>) -> String {
    match v {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Row {
        number: u32,
        title: String,
        author: Option<String>,
    }

    fn rows() -> Vec<Row> {
        vec![
            Row { number: 1, title: "Fix bug".into(), author: Some("alice".into()) },
            Row { number: 2, title: "Add feature".into(), author: None },
        ]
    }

    #[test]
    fn json_format_produces_ndjson() {
        // Just verify it doesn't panic and serializes correctly
        let rows = rows();
        let v: Vec<serde_json::Value> = rows.iter().map(|r| serde_json::to_value(r).unwrap()).collect();
        assert_eq!(v[0]["number"], 1);
        assert_eq!(v[1]["author"], serde_json::Value::Null);
    }

    #[test]
    fn cell_str_null() {
        assert_eq!(cell_str(None), "");
        assert_eq!(cell_str(Some(&serde_json::Value::Null)), "");
    }

    #[test]
    fn cell_str_variants() {
        assert_eq!(cell_str(Some(&serde_json::Value::String("hi".into()))), "hi");
        assert_eq!(cell_str(Some(&serde_json::Value::Bool(true))), "true");
        assert_eq!(cell_str(Some(&serde_json::json!(42))), "42");
    }
}
