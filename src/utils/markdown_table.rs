//! Row scanning for the Markdown tables of GitHub Docs pages.

/// Calls `on_row(columns, cells)` for every row below a table's header row. `columns` are the
/// header cells as written. A line that does not start with `|` ends the table.
pub fn for_each_row(markdown: &str, mut on_row: impl FnMut(&[String], &[String])) {
    let mut header: Option<Vec<String>> = None;
    for line in markdown.lines().map(str::trim) {
        if !line.starts_with('|') {
            header = None;
            continue;
        }
        let cells = table_cells(line);
        let Some(columns) = &header else {
            header = Some(cells);
            continue;
        };
        on_row(columns, &cells);
    }
}

/// The cell under the column called `name`, when the table has that column and the row reaches it.
pub fn cell<'a>(columns: &[String], cells: &'a [String], name: &str) -> Option<&'a str> {
    columns
        .iter()
        .position(|column| column == name)
        .and_then(|index| cells.get(index))
        .map(String::as_str)
}

fn table_cells(line: &str) -> Vec<String> {
    line.trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_each_row_passes_every_row_with_the_header_of_its_table() {
        let markdown =
            "# Title\n\n| Name | Size |\n| --- | --- |\n| a | 1 |\n\ntext\n\n| Kind |\n| x |\n";
        let mut seen: Vec<(Vec<String>, Vec<String>)> = Vec::new();
        for_each_row(markdown, |columns, cells| {
            seen.push((columns.to_vec(), cells.to_vec()));
        });
        let strings = |items: &[&str]| items.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            seen,
            vec![
                (strings(&["Name", "Size"]), strings(&["---", "---"])),
                (strings(&["Name", "Size"]), strings(&["a", "1"])),
                (strings(&["Kind"]), strings(&["x"])),
            ]
        );
    }

    #[test]
    fn for_each_row_of_a_page_without_tables_calls_nothing() {
        let mut calls = 0;
        for_each_row("No tables today.", |_, _| calls += 1);
        assert_eq!(calls, 0);
    }

    #[test]
    fn cell_finds_a_column_by_name_and_is_none_for_a_missing_column_or_short_row() {
        let columns = vec!["Name".to_string(), "Size".to_string()];
        let cells = vec!["a".to_string()];
        assert_eq!(cell(&columns, &cells, "Name"), Some("a"));
        assert_eq!(cell(&columns, &cells, "Size"), None);
        assert_eq!(cell(&columns, &cells, "Kind"), None);
    }
}
