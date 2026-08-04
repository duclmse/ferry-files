# Resolved Crate Versions - rar2zip

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
| ssh2                  | 0.9          | TBD              | none           |
| openssl               | 0.10         | TBD              | none           |
| clap                  | 4            | TBD              | none           |
| notify                | 6            | TBD              | none           |
| notify-debouncer-full | 0.3          | TBD              | none           |
| unrar                 | 0.5          | TBD              | none           |
| sevenz-rust           | 0.6          | TBD              | none           |
| tar                   | 0.4          | TBD              | none           |
| flate2                | 1            | TBD              | none           |
| bzip2                 | 0.4          | TBD              | none           |
| zip                   | 2            | TBD              | none           |
| sha2                  | 0.10         | TBD              | none           |
| hex                   | 0.4          | TBD              | none           |
| tiny_http             | 0.12         | TBD              | none           |
| ureq                  | 2            | TBD              | none           |
| lettre                | 0.11         | TBD              | none           |
| windows-service       | 0.6          | TBD              | none           |

## Notes

- Any entry in the "API deviations" column must have a corresponding comment in
  the affected source file: `// VERSIONS: <crate> <note>`
- Fill in "Resolved version" after first `cargo build` by running
  `cargo tree -p rar2zip` and copying the locked versions.
