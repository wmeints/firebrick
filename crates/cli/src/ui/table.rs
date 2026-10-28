//! Tables for people, rendered as plain text lines with `ratatui` (see ADR 0003).

use anyhow::Result;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Padding, Row, Table, Widget};

/// Room between columns for a padded separator: a space, the line and a space.
const COLUMN_SPACING: u16 = 3;
/// Width the table adds around its columns: a border and a space of padding
/// on each side.
const FRAME_WIDTH: usize = 4;

/// Renders the rows as a bordered table with a header row, a line below the
/// header and separators between the columns.
pub fn render<const N: usize>(header: [&str; N], rows: &[[String; N]]) -> Result<String> {
    let widths = column_widths(header, rows)?;
    let content_width: usize = widths.iter().copied().map(usize::from).sum();
    let spacing = usize::from(COLUMN_SPACING) * N.saturating_sub(1);
    let width = u16::try_from(content_width + spacing + FRAME_WIDTH)?;
    let height = u16::try_from(rows.len() + 4)?;

    let table = Table::new(rows.iter().map(|row| Row::new(row.clone())), widths)
        .header(Row::new(header).bottom_margin(1))
        .column_spacing(COLUMN_SPACING)
        .block(Block::bordered().padding(Padding::horizontal(1)));

    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
    Widget::render(table, buffer.area, &mut buffer);
    draw_grid_lines(&mut buffer, widths);

    Ok(buffer_to_string(&buffer))
}

/// Draws the line below the header and the column separators, joined to the
/// border. `Table` leaves room for them but doesn't draw them.
fn draw_grid_lines<const N: usize>(buffer: &mut Buffer, widths: [u16; N]) {
    let area = buffer.area;
    let (left, right) = (area.left(), area.right() - 1);
    let (top, bottom) = (area.top(), area.bottom() - 1);
    let header_line = top + 2;

    for x in left + 1..right {
        buffer[(x, header_line)].set_symbol("─");
    }

    buffer[(left, header_line)].set_symbol("├");
    buffer[(right, header_line)].set_symbol("┤");

    for x in separator_columns(widths, left) {
        for y in top + 1..bottom {
            buffer[(x, y)].set_symbol("│");
        }

        buffer[(x, top)].set_symbol("┬");
        buffer[(x, header_line)].set_symbol("┼");
        buffer[(x, bottom)].set_symbol("┴");
    }
}

/// Returns the x position of the separator after each column but the last.
fn separator_columns<const N: usize>(widths: [u16; N], left: u16) -> impl Iterator<Item = u16> {
    // The first column starts after the border and one space of padding.
    let mut column_start = left + 2;

    widths
        .into_iter()
        .take(N.saturating_sub(1))
        .map(move |width| {
            column_start += width + COLUMN_SPACING;
            column_start - COLUMN_SPACING + 1
        })
}

/// Returns the width of each column: the widest of its header and cells.
fn column_widths<const N: usize>(header: [&str; N], rows: &[[String; N]]) -> Result<[u16; N]> {
    let mut widths = header.map(|header| Line::from(header).width());

    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(Line::from(cell.as_str()).width());
        }
    }

    let mut result = [0; N];
    for (target, width) in result.iter_mut().zip(widths) {
        *target = u16::try_from(width)?;
    }

    Ok(result)
}

/// Converts a rendered buffer into text lines without trailing whitespace.
fn buffer_to_string(buffer: &Buffer) -> String {
    let width = usize::from(buffer.area.width);

    buffer
        .content
        .chunks(width)
        .map(|line| format!("{}\n", line_text(line).trim_end()))
        .collect()
}

/// Joins the symbols of a buffer line. A wide character covers the cells after
/// it, which hold a filler space that terminal backends skip, so skip them too.
fn line_text(cells: &[Cell]) -> String {
    let mut text = String::new();
    let mut covered = 0;

    for cell in cells {
        if covered > 0 {
            covered -= 1;
            continue;
        }

        text.push_str(cell.symbol());
        covered = Line::from(cell.symbol()).width().saturating_sub(1);
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_two_columns() {
        let rows = [["GH_TOKEN".to_string(), "github.com".to_string()]];

        let table = render(["NAME", "ALLOWED HOSTS"], &rows).unwrap();

        assert_eq!(
            table,
            "┌──────────┬───────────────┐\n\
             │ NAME     │ ALLOWED HOSTS │\n\
             ├──────────┼───────────────┤\n\
             │ GH_TOKEN │ github.com    │\n\
             └──────────┴───────────────┘\n"
        );
    }

    #[test]
    fn renders_single_column() {
        let table = render(["NAME"], &[["A".to_string()]]).unwrap();

        assert_eq!(
            table,
            "┌──────┐\n\
             │ NAME │\n\
             ├──────┤\n\
             │ A    │\n\
             └──────┘\n"
        );
    }
}
