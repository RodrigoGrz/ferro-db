# ferroDB Documentation

The design notes that used to live in this file were split by responsibility. Each topic has one authoritative document:

| Document | Contents |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Components, how data moves between them, testing strategy, current limitations |
| [docs/storage-engine.md](docs/storage-engine.md) | Disk Manager, Page and Buffer Pool, LRU-K, page header and checksum, serialization codec, slotted pages, RID, tuple format, Heap File, free space management |
| [docs/data-model.md](docs/data-model.md) | Data types and NULL, Schema, Table Heap, Catalog, deviations from the SQL standard |
| [docs/indexes.md](docs/indexes.md) | B+Tree: key encoding, page formats, search, splits, merges, range scans, free list |
| [docs/decisions/](docs/decisions/README.md) | Architectural decision records: why the main designs were chosen |
| [ROADMAP.md](ROADMAP.md) | Implementation checklist: what is done, verified and planned |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development workflow, conventions and documentation maintenance |

When a component changes, update its section in the document above in the same change, and mark the item in the roadmap.
