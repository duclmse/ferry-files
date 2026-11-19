# fs-ext-sftpd

An `fsd` extension that serves one folder over SFTP, in pure Rust (russh).

- Public-key login only, per user, from OpenSSH `authorized_keys` files.
  Key options such as `from=` are ignored (and logged), not enforced.
- Every session is confined to `root`, which clients see as `/`. `..` stops at
  the root, and paths that resolve through a symlink to outside it are
  refused.
- `read_only = true` refuses every change.
- An Ed25519 host key is generated on first start and kept in `host_key`.
- There is no shell. The exec channel answers only `sha256sum <path>...` and
  `rm -f <path>...` inside the root, which is what rar2zip runs to check and
  clean up an upload. Shell syntax such as `;`, `$` or `*` is refused.
- SFTP `rename` does not replace an existing file. `symlink` and `readlink`
  are not supported.

See [config.toml](config.toml) for every setting.

## Use as a rar2zip destination

```toml
[[extensions.rar2zip.destinations]]
host         = "127.0.0.1"
port         = 2222
user         = "rar2zip"
remote_path  = "/"
ssh_key_path = "/etc/fs/rar2zip/id_ed25519"
host_keys    = ["SHA256:..."]   # from `fs ext sftpd status`
```

## Commands

```sh
fs ext sftpd status   # listen address, root, users, host key fingerprint
```
