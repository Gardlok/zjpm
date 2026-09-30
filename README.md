# zjpm

A package manager for Zellij plugins.

`zjpm` is meant to make installing and keeping track of Zellij plugins boring. Instead of manually downloading `.wasm` files and remembering where they came from, it keeps a small manifest and a local plugin store.

The first version is focused on installing plugins from GitHub releases, listing what zjpm manages, updating or removing plugins, and checking the local setup for problems.

It will not rewrite your Zellij config automatically at first.

## Manifest

By default, zjpm reads `~/.config/zjpm/plugins.kdl`:

```kdl
plugin "zjstatus" {
    source "github:dj95/zjstatus"
}

plugin "pinned-example" {
    source "github:owner/repo"
    version "1.2.3"
}
```

A local plugin can use a `path:` source as well.

`ZJPM_CONFIG_DIR` and `ZJPM_DATA_DIR` can override the normal locations, which is handy for testing.

## Storage

Plugin files will be stored by SHA-256 instead of copied into every version directory. Each managed plugin gets a stable `current.wasm` path that can later be switched atomically during updates or rollbacks.

The checksum store is sharded by the first byte of the hash so it stays cheap to scan even if it grows large.

## Try it

```sh
cargo build
cargo run -- --help
cargo run -- list
```
