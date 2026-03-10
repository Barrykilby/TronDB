use comfy_table::{ContentArrangement, Table};
use trondb_core::result::{QueryMode, QueryResult};

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
