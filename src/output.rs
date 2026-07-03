use serde_json::Value;

/// Output format selected by the mutually-exclusive `--pretty`/`--table`
/// global flags (enforced via clap `conflicts_with`); `Compact` is the
/// default, meant for other programs/agents to parse.
#[derive(Clone, Copy)]
pub enum Format {
    Compact,
    Pretty,
    Table,
}

/// Print a successful result in the selected format.
pub fn print(value: &Value, format: Format) {
    println!("{}", render(value, format));
}

/// Print an error as `{"error": "..."}` in the selected format and exit
/// non-zero. Honors the same format as the success path, for consistency —
/// this is only reachable after a successful parse, so `format` is always
/// known here.
pub fn fail(err: &anyhow::Error, format: Format) -> ! {
    fail_with_code(&err.to_string(), 1, format)
}

/// Same as `fail`, but with an explicit exit code — used for clap parse
/// errors, which conventionally exit 2 (usage error) rather than 1 (the
/// code used for command-logic failures). `format` is always `Compact` at
/// that call site: parsing failed before `--pretty`/`--table` could be read
/// from it, so there's no signal to honor either way.
pub fn fail_with_code(message: &str, code: i32, format: Format) -> ! {
    let value = serde_json::json!({ "error": message });
    eprintln!("{}", render(&value, format));
    std::process::exit(code);
}

fn render(value: &Value, format: Format) -> String {
    match format {
        Format::Compact => serde_json::to_string(value).expect("value is always serializable"),
        Format::Pretty => {
            serde_json::to_string_pretty(value).expect("value is always serializable")
        }
        Format::Table => render_table(value),
    }
}

/// Render a JSON value as a human-readable table:
/// - an array of objects (e.g. `list`, `agent list`) becomes a table with
///   one column per key of the first element (all our arrays are uniform)
/// - a single object (e.g. `show`, `add`, or an `{"error": "..."}`) becomes
///   a two-column FIELD/VALUE table
/// - anything else (a bare scalar or an array of scalars) is rendered
///   line-by-line without alignment, since there are no columns to align
// clippy::nursery's suggested `map_or_else` rewrite here nests a multi-
// statement block inside a closure argument, which reads worse than the
// match it would replace -- kept as a match for readability.
#[allow(clippy::option_if_let_else)]
fn render_table(value: &Value) -> String {
    match value {
        Value::Array(items) if items.is_empty() => "(no results)".to_string(),
        Value::Array(items) => match items[0].as_object() {
            Some(first_obj) => {
                let headers: Vec<String> = first_obj.keys().cloned().collect();
                let mut rows = vec![headers.iter().map(|h| h.to_uppercase()).collect()];
                for item in items {
                    let obj = item.as_object();
                    rows.push(
                        headers
                            .iter()
                            .map(|h| obj.and_then(|o| o.get(h)).map(cell).unwrap_or_default())
                            .collect(),
                    );
                }
                render_rows(&rows)
            }
            None => items.iter().map(cell).collect::<Vec<_>>().join("\n"),
        },
        Value::Object(map) => {
            let mut rows = vec![vec!["FIELD".to_string(), "VALUE".to_string()]];
            for (k, v) in map {
                rows.push(vec![k.clone(), cell(v)]);
            }
            render_rows(&rows)
        }
        other => cell(other),
    }
}

/// Render a single JSON value as one table cell: scalars print as their
/// plain value (null as an empty cell, not the string "null"); anything
/// else (a nested array/object, e.g. a task's `tags`/`tests`) is rendered
/// as compact inline JSON rather than a nested table.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Pad every column to the widest cell in that column (by char count) and
/// join columns with two spaces, rows with newlines.
fn render_rows(rows: &[Vec<String>]) -> String {
    let num_cols = rows.first().map_or(0, Vec::len);
    let mut widths = vec![0usize; num_cols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| format!("{cell:<width$}", width = widths[i]))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_table_array_of_objects_becomes_aligned_table() {
        let value = json!([
            {"id": 1, "name": "alice"},
            {"id": 2, "name": "bob"},
        ]);
        let table = render_table(&value);
        assert!(table.contains("ID"));
        assert!(table.contains("NAME"));
        assert!(table.contains("alice"));
        assert!(table.contains("bob"));
    }

    #[test]
    fn render_table_empty_array_is_friendly_message() {
        assert_eq!(render_table(&json!([])), "(no results)");
    }

    #[test]
    fn render_table_single_object_becomes_field_value_table() {
        let value = json!({"error": "task 1 not found"});
        let table = render_table(&value);
        assert!(table.contains("FIELD"));
        assert!(table.contains("VALUE"));
        assert!(table.contains("error"));
        assert!(table.contains("task 1 not found"));
    }

    /// This codebase's own commands never actually produce an array whose
    /// elements aren't objects, but `render_table`'s doc comment explicitly
    /// documents this as intentional, designed behavior ("anything else...
    /// is rendered line-by-line without alignment") -- verified directly
    /// since nothing else was ever going to exercise it.
    #[test]
    fn render_table_array_of_scalars_renders_line_by_line() {
        let value = json!(["alpha", "beta"]);
        assert_eq!(render_table(&value), "alpha\nbeta");
    }

    /// Same reasoning: no command ever returns a bare top-level scalar, but
    /// it's documented, intended behavior for the general-purpose renderer.
    #[test]
    fn render_table_bare_top_level_scalar_falls_through_to_cell() {
        assert_eq!(render_table(&json!("just a string")), "just a string");
        assert_eq!(render_table(&json!(42)), "42");
    }

    #[test]
    fn cell_renders_each_scalar_type() {
        assert_eq!(cell(&Value::Null), "");
        assert_eq!(cell(&json!("text")), "text");
        assert_eq!(cell(&json!(42)), "42");
        // No field in this codebase's actual data model is ever a bool, but
        // `cell` is written to handle it -- verified directly since no
        // command output was ever going to exercise this arm.
        assert_eq!(cell(&json!(true)), "true");
        assert_eq!(cell(&json!(false)), "false");
    }

    #[test]
    fn cell_renders_nested_array_or_object_as_inline_compact_json() {
        assert_eq!(cell(&json!(["a", "b"])), "[\"a\",\"b\"]");
        assert_eq!(cell(&json!({"k": "v"})), "{\"k\":\"v\"}");
    }

    #[test]
    fn render_rows_pads_columns_to_widest_cell() {
        let rows = vec![
            vec!["ID".to_string(), "NAME".to_string()],
            vec!["1".to_string(), "alice".to_string()],
            vec!["22".to_string(), "b".to_string()],
        ];
        let rendered = render_rows(&rows);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        // The ID column must be padded to width 2 (from "22") on the header
        // and first data row, so the NAME column starts at the same offset
        // on every line.
        assert_eq!(lines[0].find("NAME"), lines[1].find("alice"));
        assert_eq!(lines[0].find("NAME"), lines[2].find('b'));
    }
}
