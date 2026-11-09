# Resolved Crate Versions - rar2zip

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
| ssh2                  | 0.9          | not used         | replaced by `russh` / `russh-sftp` |
| openssl               | 0.10         | not used         | replaced by `russh` / `russh-sftp` |
| clap                  | 4            | 4.6.1            | none           |
| notify                | 6            | 6.1.1            | none           |
| notify-debouncer-full | 0.3          | 0.3.2            | none           |
| unrar                 | 0.5          | 0.5.8            | none           |
| sevenz-rust           | 0.6          | 0.6.1            | none           |
| tar                   | 0.4          | 0.4.45           | none           |
| flate2                | 1            | 1.1.9            | none           |
| bzip2                 | 0.4          | 0.4.4            | none           |
| zip                   | 2            | 2.4.2            | none           |
| sha2                  | 0.10         | 0.10.9           | none           |
| hex                   | 0.4          | 0.4.3            | none           |
| tiny_http             | 0.12         | 0.12.0           | none           |
| ureq                  | 2            | 2.12.1           | none           |
| lettre                | 0.11         | 0.11.21          | none           |
| windows-service       | 0.6          | 0.6.0            | none           |
| bytes                 | 1            | 1.11.1           | none           |
| glob                  | 0.3          | 0.3.3            | none           |
| russh                 | 0.60         | 0.60.2           | none           |
| russh-sftp            | 2            | 2.1.2            | none           |
| uuid                  | 1            | 1.23.1           | none           |

## Notes

- Any entry in the "API deviations" column must have a corresponding comment in
  the affected source file: `// VERSIONS: <crate> <note>`
- Fill in "Resolved version" after first `cargo build` by running
  `cargo tree -p rar2zip` and copying the locked versions.
