//! `perfscale metrics` — print the metric catalog: every metric the
//! registered families (built-in `http`/`llm`, plus any downstream pro
//! families) declare, with its kind, unit, and description (RFC 010).

use perfscale_core::metrics::catalog;

use crate::error::CliError;

pub async fn run() -> Result<(), CliError> {
    let catalog = catalog();
    if catalog.is_empty() {
        println!("no metric families registered");
        return Ok(());
    }

    let headers = ["FAMILY", "NAME", "KIND", "UNIT", "DESCRIPTION"];
    let mut rows: Vec<[String; 5]> = catalog
        .iter()
        .map(|(family, def)| {
            [
                family.to_string(),
                def.name.to_string(),
                def.kind.as_str().to_string(),
                def.unit.to_string(),
                def.description.to_string(),
            ]
        })
        .collect();
    rows.sort();

    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let print_row = |cells: [&str; 5]| {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
            cells[0],
            cells[1],
            cells[2],
            cells[3],
            cells[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        );
    };
    print_row(headers);
    for row in &rows {
        print_row([
            row[0].as_str(),
            row[1].as_str(),
            row[2].as_str(),
            row[3].as_str(),
            row[4].as_str(),
        ]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn prints_the_catalog() {
        // Exits cleanly; the built-in families are always registered.
        super::run().await.unwrap();
        let names: Vec<&str> = perfscale_core::metrics::catalog()
            .iter()
            .map(|(_, d)| d.name)
            .collect();
        assert!(names.contains(&"http_req_duration"));
        assert!(names.contains(&"llm_tokens_per_sec"));
    }
}
