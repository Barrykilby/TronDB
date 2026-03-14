use comfy_table::{ContentArrangement, Table};
use trondb_core::result::{QueryMode, QueryResult};
use trondb_proto::pb;

pub fn format_result(result: &QueryResult) -> String {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    // Header row
    table.set_header(&result.columns);

    // Data rows
    for row in &result.rows {
        let cells: Vec<String> = result
            .columns
            .iter()
            .map(|col| {
                if col == "_score" {
                    row.score
                        .map(|s| format!("{:.4}", s))
                        .unwrap_or_default()
                } else {
                    row.values
                        .get(col)
                        .map(|v| v.to_string())
                        .unwrap_or_default()
                }
            })
            .collect();
        table.add_row(cells);
    }

    let mut output = table.to_string();
    output.push('\n');

    // Stats line
    let mode = match result.stats.mode {
        QueryMode::Deterministic => "deterministic",
        QueryMode::Probabilistic => "probabilistic",
    };
    output.push_str(&format!(
        "{} row(s) | {} scanned | {} | {} | {:.2}ms",
        result.rows.len(),
        result.stats.entities_scanned,
        mode,
        result.stats.tier,
        result.stats.elapsed.as_secs_f64() * 1000.0,
    ));

    output
}

/// Format a gRPC QueryResponse for display (remote mode).
pub fn format_grpc_response(resp: &pb::QueryResponse) -> String {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    if resp.columns.is_empty() && resp.rows.is_empty() {
        // DDL or write statement — show stats only
        if let Some(ref stats) = resp.stats {
            return format!(
                "OK | {:.2}ms",
                stats.elapsed_nanos as f64 / 1_000_000.0,
            );
        }
        return "OK".to_string();
    }

    table.set_header(&resp.columns);

    for row in &resp.rows {
        let cells: Vec<String> = resp
            .columns
            .iter()
            .map(|col| {
                if col == "_score" {
                    row.score
                        .map(|s| format!("{:.4}", s))
                        .unwrap_or_default()
                } else {
                    row.values
                        .get(col)
                        .map(format_value)
                        .unwrap_or_default()
                }
            })
            .collect();
        table.add_row(cells);
    }

    let mut output = table.to_string();
    output.push('\n');

    if let Some(ref stats) = resp.stats {
        output.push_str(&format!(
            "{} row(s) | {} scanned | {:.2}ms",
            resp.rows.len(),
            stats.entities_scanned,
            stats.elapsed_nanos as f64 / 1_000_000.0,
        ));
    }

    output
}

fn format_value(v: &pb::ValueMessage) -> String {
    use pb::value_message::Value;
    match &v.value {
        Some(Value::StringVal(s)) => s.clone(),
        Some(Value::IntVal(i)) => i.to_string(),
        Some(Value::FloatVal(f)) => format!("{f}"),
        Some(Value::BoolVal(b)) => b.to_string(),
        Some(Value::NullVal(_)) => "NULL".to_string(),
        None => String::new(),
    }
}
