# zjpm

A package manager for Zellij plugins.

`zjpm` is meant to make installing and keeping track of Zellij plugins boring. Instead of manually downloading `.wasm` files and remembering where they came from, it will keep a small manifest and a local plugin store.

The first version will focus on a few basics: install plugins from GitHub releases, list what zjpm manages, update or remove plugins, and check the local setup for problems.

It will not try to rewrite your Zellij config automatically at first.

## Getting started

This project is just getting started.

```sh
cargo build
cargo run -- --help
```
