# Contributing to ferroDB

ferroDB is a personal learning project, but issues and pull requests are welcome. This file describes how the project is developed and how its documentation is kept accurate.

## Development setup

- **Rust 1.98.1** (the version in [`.tool-versions`](.tool-versions); the crate uses the 2024 edition, which needs at least 1.85). With [asdf](https://asdf-vm.com/): `asdf install`. With rustup: `rustup toolchain install 1.98.1`.
- No other system dependencies.

## Checks

Every change must pass the same checks as [CI](.github/workflows/ci.yml):

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

`cargo run` writes `data.db` in the current directory (ignored by git). Run the demo from a scratch directory if you want to keep your own file untouched.

## Code conventions

- **One folder per layer** (`src/storage/`, `src/types/`, `src/table/`, `src/catalog/`), one file per component. A layer's `mod.rs` only declares submodules and re-exports its API. Code shared by every layer (`config.rs`, `serialization.rs`) stays at the crate root.
- **Dependencies point downward only:** `catalog` → `table` → `types` → `storage`. `storage` imports no other layer.
- **Fail without side effects:** validate before changing bytes or state; return typed errors; never panic on data read from disk ([ADR 0007](docs/decisions/0007-fail-without-side-effects.md)).
- **On-disk formats are little-endian**, written through `ByteWriter` / `ByteReader`, and documented byte by byte in [`docs/`](doc.md).

## Test conventions

- Test names follow `<thing>_should_<behavior>`, e.g. `insert_should_reuse_deleted_slot`.
- Unit tests live next to the code; integration tests live in `tests/<layer>_<component>.rs`.
- Cover the happy path, boundaries (empty, one, exactly full, full + 1), every error variant, and, for anything that touches disk, persistence across eviction and reopening the file (temporary files are removed on `Drop`).
- Components with mutable state get a randomized test against a simple model (`HashMap`, `Vec`) that checks invariants along the way, like `random_operations_should_match_model`.

## Commit messages

Title in lowercase kebab-case: `<type>/<what-was-done>`, with an optional short body in English.

| Type | Use for | Example |
|---|---|---|
| `feat` | new database functionality | `feat/heap-file-insert-and-scan` |
| `fix` | bug fixes | `fix/slotted-page-compact-offset-overflow` |
| `add` | tests, documentation, supporting files | `add/buffer-pool-eviction-tests` |
| `chore` | maintenance without behavior change: refactors, moves, config, dependencies | `chore/move-storage-modules-to-folder` |

## Documentation maintenance

The database feature checklist lives in [ROADMAP.md](ROADMAP.md). The items below are housekeeping for the repository itself and must not be mixed into the roadmap.

When a change lands, check:

- [ ] The roadmap item is marked `[x]` only if the code **and** its tests exist, with links to both.
- [ ] The component's section in [docs/storage-engine.md](docs/storage-engine.md) or [docs/data-model.md](docs/data-model.md) matches the code: layouts, errors, design decisions, tests, limitations.
- [ ] [docs/architecture.md](docs/architecture.md) is updated if components, dependencies or data flow changed, including the Mermaid diagram and the Limitations section.
- [ ] A significant design decision gets a record in [docs/decisions/](docs/decisions/README.md); rationale that was not recorded is labeled as such.
- [ ] The README's status summary and test count still match `cargo test --all-targets`.
- [ ] If the demo changed, the captured output in the README is re-captured from a real run in an empty directory.
- [ ] The CI workflow still matches the commands above, and the Rust version matches `.tool-versions`.

## Repository settings

These are configured in GitHub, not in files.

- [ ] **About → Description:** `Educational relational database engine built from scratch in Rust, inspired by CMU Database Systems.`
- [ ] **About → Topics:** `rust`, `database`, `database-engine`, `database-internals`, `storage-engine`, `systems-programming`

To apply them, open the repository page, click the gear icon next to **About**, fill in the description and topics, and save. From a terminal with the GitHub CLI:

```sh
gh repo edit RodrigoGrz/ferro-db \
  --description "Educational relational database engine built from scratch in Rust, inspired by CMU Database Systems." \
  --add-topic rust --add-topic database --add-topic database-engine \
  --add-topic database-internals --add-topic storage-engine --add-topic systems-programming
```

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
