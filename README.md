# zjpm

A package manager for Zellij plugins.

`zjpm` is meant to make installing and keeping track of Zellij plugins boring. Instead of manually downloading `.wasm` files and remembering where they came from, it keeps a small manifest and a local plugin store.

The first version is focused on installing plugins from GitHub releases, listing what zjpm manages, updating or removing plugins, and checking the local setup for problems.

It will not rewrite your Zellij config automatically at first.

## Install

Install a local WebAssembly plugin:

```sh
zjpm install ./my-plugin.wasm
zjpm install ./plugin.wasm --name my-plugin
```

Or install the latest published GitHub release:

```sh
zjpm install dj95/zjstatus
```

When a release has several WASM assets, zjpm first looks for `<repository>.wasm`. You can choose another one explicitly:

```sh
zjpm install dj95/zjstatus --asset zjframes.wasm --name zjframes
```

GitHub installs resolve the latest non-draft, non-prerelease release through the GitHub REST API. If the asset is still ambiguous, zjpm lists the choices instead of guessing. If `GITHUB_TOKEN` is set, it is used for API authentication.

Every successful install validates the full WebAssembly module, streams it through SHA-256 into the content store, activates it without copying those bytes again, then records intent in `plugins.kdl` and exact resolved state in `plugins.lock.kdl`.

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

A local plugin uses a `path:` source.

`ZJPM_CONFIG_DIR` and `ZJPM_DATA_DIR` can override the normal locations, which is handy for testing.

## Storage

Plugin files are stored by SHA-256 instead of copied into every version directory. New plugin bytes are streamed through the hash while they are written to a staging file, then the finished file is synced and moved into place atomically.

If the same bytes are installed again, zjpm verifies and reuses the existing blob.

Activation uses hard links rather than copying the WASM again. Version entries and the stable `current.wasm` path point at the same verified blob bytes, and `current.wasm` is replaced atomically when a different blob is activated.

The checksum store is sharded by the first byte of the hash so it stays cheap to scan even if it grows large.

## Try it

```sh
cargo build
cargo run -- --help
cargo run -- list
```
