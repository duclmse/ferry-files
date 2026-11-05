# Resolved Crate Versions - filesearch

Generated at implementation time. Update whenever a dep is added or changed.

| Crate                 | Spec version | Resolved version | API deviations |
| --------------------- | ------------ | ---------------- | -------------- |
| tokio                 | 1            | 1.52.1           | none           |
| tokio-util            | 0.7          | 0.7.18           | none           |
| serde                 | 1            | 1.0.228          | none           |
| serde_json            | 1            | 1.0.149          | none           |
| toml                  | 1            | 1.1.2+spec-1.1.0 | none           |
| tracing               | 0.1          | 0.1.44           | none           |
| tracing-subscriber    | 0.3          | 0.3.23           | none           |
| tracing-appender      | 0.2          | 0.2.5            | none           |
| thiserror             | 1            | 1.0.69           | none           |
| anyhow                | 1            | 1.0.102          | none           |
| chrono                | 0.4          | 0.4.44           | none           |
| rusqlite              | 0.31         | 0.31.0           | none           |
| r2d2                  | 0.8          | 0.8.10           | none           |
| r2d2_sqlite           | 0.23         | 0.24.0           | none           |
| clap                  | 4            | 4.6.1            | none           |
| notify                | 6            | 6.1.1            | none           |
| notify-debouncer-full | 0.3          | 0.3.2            | none           |
| tantivy               | 0.22         | 0.22.1           | none           |
| tonic                 | 0.11         | 0.11.0           | none           |
| tonic-reflection      | 0.11         | 0.11.0           | none           |
| prost                 | 0.12         | 0.12.6           | none           |
| axum                  | 0.7          | 0.7.9            | none           |
| tower                 | 0.4          | 0.4.13           | none           |
| tower-http            | 0.5          | 0.5.2            | none           |
| hyper                 | 1            | 1.9.0            | none           |
| nucleo                | 0.5          | 0.5.0            | none           |
| regex                 | 1            | 1.12.3           | none           |
| rayon                 | 1            | 1.12.0           | none           |
| ignore                | 0.4          | 0.4.25           | none           |
| encoding_rs           | 0.8          | 0.8.35           | none           |
| memmap2               | 0.9          | 0.9.10           | none           |
| zip                   | 2            | 2.4.2            | none           |
| tempfile              | 3            | 3.27.0           | none           |
| ratatui               | 0.26         | 0.26.3           | none           |
| crossterm             | 0.27         | 0.27.0           | none           |
| humansize             | 2            | 2.1.3            | none           |
| once_cell             | 1            | 1.21.4           | none           |
| dashmap               | 5            | 5.5.3            | none           |
| async-stream          | 0.3          | 0.3.6            | none           |
| async-trait           | 0.1          | 0.1.89           | none           |
| blake3                | 1            | 1.8.5            | none           |
| bytes                 | 1            | 1.11.1           | none           |
| flate2                | 1            | 1.1.9            | none           |
| futures               | 0.3          | 0.3.32           | none           |
| glob                  | 0.3          | 0.3.3            | none           |
| memchr                | 2            | 2.8.0            | none           |
| nix                   | 0.28         | 0.28.0           | none           |
| postcard              | 1            | 1.1.3            | none           |
| sd-notify             | 0.4          | 0.4.5            | none           |
| tokio-stream          | 0.1          | 0.1.18           | none           |
| uuid                  | 1            | 1.23.1           | none           |
| windows               | 0.54         | 0.54.0           | none           |
| windows-service       | 0.6          | 0.6.0            | none           |

## Notes

- Any entry in the "API deviations" column must have a corresponding comment in
  the affected source file: `// VERSIONS: <crate> <note>`
- Fill in "Resolved version" after first `cargo build` by running
  `cargo tree -p filesearch` and copying the locked versions.
