//! Everything file lists (`.efu`): UTF-8 CSV with a header row naming the
//! columns, usually `Filename,Size,Date Modified,Date Created,Attributes`.
//! Filenames are full paths; dates are Windows FILETIME (100 ns ticks since
//! 1601); attributes are Windows `FILE_ATTRIBUTE_*` bits, so a folder has
//! 0x10. Every column but `Filename` may be missing or empty.

const TICKS_PER_SEC: u64 = 10_000_000;
const FILETIME_TO_UNIX_SECS: i64 = 11_644_473_600;

/// The header this module writes, in the column order Everything uses.
pub const HEADER: &str = "Filename,Size,Date Modified,Date Created,Attributes\r\n";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: Option<u64>,
    /// Unix seconds.
    pub mtime: Option<i64>,
    /// Unix seconds.
    pub ctime: Option<i64>,
    /// Windows `FILE_ATTRIBUTE_*` bits.
    pub attributes: Option<u32>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attributes
            .is_some_and(|a| a & crate::attrib::WINDOWS_DIRECTORY != 0)
    }
}

#[derive(Debug, Default)]
pub struct List {
    pub entries: Vec<Entry>,
    /// Rows left out, with the first reason, so a caller can report them.
    pub skipped: usize,
    pub first_problem: Option<String>,
}

pub fn filetime_to_unix(ticks: u64) -> i64 {
    (ticks / TICKS_PER_SEC) as i64 - FILETIME_TO_UNIX_SECS
}

pub fn unix_to_filetime(secs: i64) -> Option<u64> {
    let since_1601 = secs.checked_add(FILETIME_TO_UNIX_SECS)?;
    u64::try_from(since_1601).ok()?.checked_mul(TICKS_PER_SEC)
}

#[derive(Clone, Copy)]
enum Column {
    Filename,
    Size,
    Modified,
    Created,
    Attributes,
    Other,
}

fn column(name: &str) -> Column {
    match name.trim().to_ascii_lowercase().as_str() {
        "filename" => Column::Filename,
        "size" => Column::Size,
        "date modified" => Column::Modified,
        "date created" => Column::Created,
        "attributes" => Column::Attributes,
        _ => Column::Other,
    }
}

/// Parses a whole list. Fails only when there is no `Filename` column; a
/// row with an unreadable number is skipped and counted instead, so one bad
/// row does not lose the rest of a catalogue.
pub fn parse(text: &str) -> Result<List, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Records {
        rest: text,
        line: 1,
    };
    let Some((_, header)) = records.next() else {
        return Err("empty file".into());
    };
    let columns: Vec<Column> = header.iter().map(|h| column(h)).collect();
    if !columns.iter().any(|c| matches!(c, Column::Filename)) {
        return Err("no Filename column in the header".into());
    }
    let mut list = List::default();
    for (line, fields) in records {
        if fields.len() == 1 && fields[0].is_empty() {
            continue;
        }
        match entry(&columns, fields) {
            Ok(Some(e)) => list.entries.push(e),
            Ok(None) => {}
            Err(why) => {
                list.skipped += 1;
                list.first_problem
                    .get_or_insert_with(|| format!("line {line}: {why}"));
            }
        }
    }
    Ok(list)
}

fn entry(columns: &[Column], fields: Vec<String>) -> Result<Option<Entry>, String> {
    let mut e = Entry::default();
    for (col, value) in columns.iter().zip(fields) {
        let value = value.trim();
        let number = |what: &str| -> Result<Option<u64>, String> {
            if value.is_empty() {
                return Ok(None);
            }
            value
                .parse::<u64>()
                .map(Some)
                .map_err(|_| format!("{what} is not a number: {value:?}"))
        };
        match col {
            Column::Filename => e.path = value.to_owned(),
            Column::Size => e.size = number("size")?,
            Column::Modified => e.mtime = number("date modified")?.map(filetime_to_unix),
            Column::Created => e.ctime = number("date created")?.map(filetime_to_unix),
            Column::Attributes => {
                e.attributes = number("attributes")?
                    .map(|a| u32::try_from(a).map_err(|_| format!("attributes too large: {a}")))
                    .transpose()?
            }
            Column::Other => {}
        }
    }
    if e.path.is_empty() {
        return Ok(None);
    }
    Ok(Some(e))
}

/// Appends one row in [`HEADER`]'s column order.
pub fn write_entry(out: &mut String, e: &Entry) {
    use std::fmt::Write;
    out.push('"');
    out.push_str(&e.path.replace('"', "\"\""));
    out.push('"');
    let field = |out: &mut String, v: Option<u64>| {
        out.push(',');
        if let Some(v) = v {
            let _ = write!(out, "{v}");
        }
    };
    field(out, e.size);
    field(out, e.mtime.and_then(unix_to_filetime));
    field(out, e.ctime.and_then(unix_to_filetime));
    field(out, e.attributes.map(u64::from));
    out.push_str("\r\n");
}

/// CSV records with their first line numbers. Quoted fields may hold commas,
/// `""` and line breaks.
struct Records<'a> {
    rest: &'a str,
    line: usize,
}

impl Iterator for Records<'_> {
    type Item = (usize, Vec<String>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let start_line = self.line;
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut chars = self.rest.char_indices().peekable();
        let mut end = self.rest.len();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' if quoted => {
                    if chars.peek().is_some_and(|&(_, n)| n == '"') {
                        chars.next();
                        field.push('"');
                    } else {
                        quoted = false;
                    }
                }
                '"' if field.is_empty() => quoted = true,
                ',' if !quoted => fields.push(std::mem::take(&mut field)),
                '\n' if !quoted => {
                    end = i + 1;
                    break;
                }
                '\r' if !quoted && chars.peek().is_some_and(|&(_, n)| n == '\n') => {}
                c => {
                    if c == '\n' {
                        self.line += 1;
                    }
                    field.push(c);
                }
            }
        }
        self.line += 1;
        self.rest = &self.rest[end..];
        fields.push(field);
        Some((start_line, fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // As Everything 1.4 writes it: quoted names, FILETIME dates, attribute 16
    // on folders and 32 (archive) on files.
    const SAMPLE: &str = "\u{feff}Filename,Size,Date Modified,Date Created,Attributes\r\n\
\"D:\\Photos\",0,133400000000000000,133300000000000000,16\r\n\
\"D:\\Photos\\beach, 2024.jpg\",2048,133400000000000000,,32\r\n\
\"D:\\Photos\\say \"\"cheese\"\".png\",,,,\r\n";

    #[test]
    fn reads_an_everything_export() {
        let list = parse(SAMPLE).unwrap();
        assert_eq!(list.skipped, 0);
        assert_eq!(list.entries.len(), 3);
        let dir = &list.entries[0];
        assert_eq!(dir.path, r"D:\Photos");
        assert!(dir.is_dir());
        assert_eq!(dir.mtime, Some(filetime_to_unix(133_400_000_000_000_000)));
        let photo = &list.entries[1];
        assert_eq!(photo.path, r"D:\Photos\beach, 2024.jpg");
        assert_eq!(photo.size, Some(2048));
        assert_eq!(photo.ctime, None);
        assert!(!photo.is_dir());
        assert_eq!(list.entries[2].path, r#"D:\Photos\say "cheese".png"#);
        assert_eq!(list.entries[2].size, None);
    }

    #[test]
    fn columns_are_found_by_name_in_any_order() {
        let list = parse("attributes,FILENAME,Extra\n16,/a/b,x\n").unwrap();
        assert_eq!(list.entries[0].path, "/a/b");
        assert!(list.entries[0].is_dir());
    }

    #[test]
    fn a_bad_row_is_skipped_and_reported() {
        let list = parse("Filename,Size\n/a,1\n/b,lots\n/c,3\n").unwrap();
        let paths: Vec<_> = list.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/a", "/c"]);
        assert_eq!(list.skipped, 1);
        assert!(list.first_problem.unwrap().starts_with("line 3:"));
    }

    #[test]
    fn a_quoted_line_break_is_part_of_the_name() {
        let list = parse("Filename,Size\n\"/a\nb\",1\n/c,2\n").unwrap();
        assert_eq!(list.entries[0].path, "/a\nb");
        assert_eq!(list.entries[1].path, "/c");
    }

    #[test]
    fn a_list_without_filenames_is_refused() {
        assert!(parse("Size,Attributes\n1,2\n").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn what_is_written_reads_back() {
        let entries = vec![
            Entry {
                path: r#"C:\a "b", c"#.into(),
                size: Some(10),
                mtime: Some(1_700_000_000),
                ctime: Some(1_600_000_000),
                attributes: Some(0x20),
            },
            Entry {
                path: "/x".into(),
                attributes: Some(0x10),
                ..Entry::default()
            },
        ];
        let mut text = HEADER.to_owned();
        for e in &entries {
            write_entry(&mut text, e);
        }
        assert_eq!(parse(&text).unwrap().entries, entries);
    }

    #[test]
    fn filetime_round_trips_whole_seconds() {
        assert_eq!(filetime_to_unix(unix_to_filetime(0).unwrap()), 0);
        assert_eq!(unix_to_filetime(-FILETIME_TO_UNIX_SECS - 1), None);
    }
}
