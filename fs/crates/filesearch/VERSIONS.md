# Resolved Crate Versions — filesearch

Generated at implementation time. Update whenever a dep is added or changed.

| Crate                 | Spec version | Resolved version | API deviations |
| --------------------- | ------------ | ---------------- | -------------- |
| tokio                 | 1            | TBD              | none           |
| tokio-util            | 0.7          | TBD              | none           |
| serde                 | 1            | TBD              | none           |
| serde_json            | 1            | TBD              | none           |
| toml                  | 1            | TBD              | none           |
| tracing               | 0.1          | TBD              | none           |
| tracing-subscriber    | 0.3          | TBD              | none           |
| tracing-appender      | 0.2          | TBD              | none           |
| thiserror             | 1            | TBD              | none           |
| anyhow                | 1            | TBD              | none           |
| chrono                | 0.4          | TBD              | none           |
| rusqlite              | 0.31         | TBD              | none           |
| r2d2                  | 0.8          | TBD              | none           |
| r2d2_sqlite           | 0.23         | TBD              | none           |
| clap                  | 4            | TBD              | none           |
| notify                | 6            | TBD              | none           |
| notify-debouncer-full | 0.3          | TBD              | none           |
| tantivy               | 0.22         | TBD              | none           |
| tonic                 | 0.11         | TBD              | none           |
| tonic-reflection      | 0.11         | TBD              | none           |
| prost                 | 0.12         | TBD              | none           |
| axum                  | 0.7          | TBD              | none           |
| tower                 | 0.4          | TBD              | none           |
| tower-http            | 0.5          | TBD              | none           |
| hyper                 | 1            | TBD              | none           |
| nucleo                | 0.5          | TBD              | none           |
| regex                 | 1            | TBD              | none           |
| rayon                 | 1            | TBD              | none           |
| ignore                | 0.4          | TBD              | none           |
| encoding_rs           | 0.8          | TBD              | none           |
| memmap2               | 0.9          | TBD              | none           |
| zip                   | 2            | TBD              | none           |
| tempfile              | 3            | TBD              | none           |
| ratatui               | 0.26         | TBD              | none           |
| crossterm             | 0.27         | TBD              | none           |
| humansize             | 2            | TBD              | none           |
| once_cell             | 1            | TBD              | none           |
| dashmap               | 5            | TBD              | none           |

## Notes

- Any entry in the "API deviations" column must have a corresponding comment in
  the affected source file: `// VERSIONS: <crate> <note>`
- Fill in "Resolved version" after first `cargo build` by running
  `cargo tree -p filesearch` and copying the locked versions.
