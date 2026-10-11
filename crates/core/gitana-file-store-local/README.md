# gitana-file-store-local

The filesystem `FileStore` backend for local and standalone deployments. It stores
git-relative paths beneath a confined root through one of two backends, selected at compile
time: a `cap_std::fs::Dir` capability on native targets, and an open `wasi:filesystem`
directory descriptor on `wasm32-wasip2`. The crate never mints ambient authority; callers hand
it the directory.

## Durability

`FileStore::durability_barrier` flushes each target file, then its ancestor directories,
deepest first.

- **Native:** files and directories are reopened through the capability and flushed with
  `sync_all`, which is `F_FULLFSYNC` on Apple platforms.
- **WASI:** files are flushed with `descriptor.sync-data` and directories with
  `descriptor.sync`. The backend does not check a directory descriptor's flags before syncing
  it:
  - Under wasmtime, a subdirectory opened through `open_at` never reports
    `MUTATE_DIRECTORY`, because wasmtime derives a directory's mode from `READ` and `WRITE`
    alone.
  - Its `sync` still reopens the directory and fsyncs it whatever that mode is. This was
    checked against wasmtime-wasi 46.0.3, 49.0.2 and 50.0.0-rc.1.
  - A barrier therefore relies on the host really syncing directories. On a host whose `sync`
    does nothing for a directory opened without write access, a barrier would report success
    without making new directory entries durable.
