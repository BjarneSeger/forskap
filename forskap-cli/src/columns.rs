//! Column widths for the text views that align to their widest cell.

/// What a cell takes up on screen: its chars, as `{:<w$}` counts them.
fn width(cell: &str) -> usize {
    cell.chars().count()
}

/// The width of the first `N` columns: each as wide as its widest cell. A
/// row may be shorter than the others.
pub fn widths<const N: usize, R, C>(rows: impl IntoIterator<Item = R>) -> [usize; N]
where
    R: IntoIterator<Item = C>,
    C: AsRef<str>,
{
    let mut widths = [0; N];
    for row in rows {
        for (widest, cell) in widths.iter_mut().zip(row) {
            *widest = (*widest).max(width(cell.as_ref()));
        }
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_are_those_of_the_widest_cells() {
        let rows = [["JOB", "STATUS"], ["events", "due"]];
        assert_eq!(widths(rows), [6, 6]);
        // Columns past the asked-for ones are not measured.
        assert_eq!(widths(rows), [6]);
    }

    #[test]
    fn a_short_row_leaves_its_missing_columns_alone() {
        let rows = [vec!["a"], vec!["abc", "de", "f"], vec![]];
        assert_eq!(widths(rows), [3, 2, 1]);
    }

    #[test]
    fn no_rows_are_no_width() {
        assert_eq!(widths(Vec::<[&str; 2]>::new()), [0, 0]);
    }

    #[test]
    fn a_cell_counts_chars_not_bytes() {
        let rows = [[String::from("grüße"), String::from("日本")]];
        assert_eq!(widths(&rows), [5, 2]);
    }
}
