use std::io::{self, Write};

pub(super) fn write_table(
    output: &mut impl Write,
    headers: &[&str],
    rows: &[Vec<String>],
) -> io::Result<()> {
    if headers.is_empty() || rows.iter().any(|row| row.len() != headers.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid table columns",
        ));
    }
    let widths: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain(std::iter::once(header.chars().count()))
                .max()
                .unwrap()
        })
        .collect();
    for row in std::iter::once(
        headers
            .iter()
            .map(|header| header.to_string())
            .collect::<Vec<_>>(),
    )
    .chain(rows.iter().cloned())
    {
        for (column, value) in row.iter().enumerate() {
            if column > 0 {
                write!(output, "  ")?;
            }
            if column + 1 == headers.len() {
                write!(output, "{value}")?;
            } else if headers[column].ends_with("_BYTES") {
                write!(output, "{value:>width$}", width = widths[column])?;
            } else {
                write!(output, "{value:<width$}", width = widths[column])?;
            }
        }
        writeln!(output)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_align_with_long_names_and_large_or_empty_images() {
        let rows = vec![
            vec!["FDA".into(), "0".into(), "bootstrap".into()],
            vec![
                "emmc1$ap2$cluster_kernel_a".into(),
                "5601212484".into(),
                "image".into(),
            ],
        ];
        let mut output = vec![];
        write_table(&mut output, &["RESOURCE", "IMAGE_BYTES", "NOTES"], &rows).unwrap();
        let output = String::from_utf8(output).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert!(!output.contains('\t'));
        let notes_column = lines[0].find("NOTES").unwrap();
        assert_eq!(lines[1].find("bootstrap"), Some(notes_column));
        assert_eq!(lines[2].find("image"), Some(notes_column));
        assert_eq!(
            lines[1].find('0').unwrap() + 1,
            lines[2].find("5601212484").unwrap() + 10
        );
        assert!(lines.iter().all(|line| !line.ends_with(' ')));
    }

    #[test]
    fn empty_table_still_has_a_header() {
        let mut output = vec![];
        write_table(&mut output, &["RESOURCE", "IMAGE_BYTES", "NOTES"], &[]).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "RESOURCE  IMAGE_BYTES  NOTES\n"
        );
    }
}
