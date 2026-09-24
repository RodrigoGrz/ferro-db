# ferro-db

A toy database built in Rust following CMU Database Systems lectures.

## Features (WIP)

## Storage

- [x] Disk Manager
- [x] Page
- [x] Buffer Pool Manager
- [x] LRU-K Replacement
- [ ] Page Serialization
- [ ] Slotted Pages
- [ ] Tuple Storage
- [ ] RID
- [ ] Heap File
- [ ] Free Space Management

## Data Types

- [ ] Integer
- [ ] Float
- [ ] Boolean
- [ ] String
- [ ] NULL
- [ ] Date/Time
- [ ] UUID
- [ ] Type System

## Tables

- [ ] Schema
- [ ] Table Metadata
- [ ] INSERT
- [ ] SELECT
- [ ] UPDATE
- [ ] DELETE
- [ ] Sequential Scan

## Indexes

- [ ] B+Tree
- [ ] Index Scan
- [ ] Range Scan
- [ ] Unique Index
- [ ] Composite Index

## SQL

- [ ] Lexer
- [ ] SQL Parser
- [ ] AST
- [ ] DDL
- [ ] DML
- [ ] Query Validation

## Query Execution

- [ ] Sequential Scan
- [ ] Index Scan
- [ ] Filter
- [ ] Projection
- [ ] Sort
- [ ] Limit
- [ ] Aggregation
- [ ] GROUP BY
- [ ] JOIN
- [ ] Hash Join
- [ ] Nested Loop Join

## Transactions

- [ ] Transaction Manager
- [ ] BEGIN
- [ ] COMMIT
- [ ] ROLLBACK
- [ ] Isolation Levels
- [ ] Lock Manager
- [ ] Concurrency Control
- [ ] Deadlock Detection

## Recovery

- [ ] WAL
- [ ] LSN
- [ ] Checkpoints
- [ ] Redo
- [ ] Undo
- [ ] Crash Recovery

## Query Optimization

- [ ] Logical Query Plan
- [ ] Physical Query Plan
- [ ] Cost Model
- [ ] Statistics
- [ ] Cardinality Estimation
- [ ] Index Selection
- [ ] Join Ordering

## Constraints

- [ ] PRIMARY KEY
- [ ] UNIQUE
- [ ] NOT NULL
- [ ] FOREIGN KEY
- [ ] CHECK
- [ ] DEFAULT

## Connectivity

- [ ] CLI
- [ ] Client Protocol
- [ ] TCP Server
- [ ] Prepared Statements
- [ ] Connection Pooling

## Security

- [ ] Authentication
- [ ] Users
- [ ] Roles
- [ ] Permissions

## Reliability

- [ ] Unit Tests
- [ ] Integration Tests
- [ ] Concurrency Tests
- [ ] Crash Recovery Tests
- [ ] Fuzz Tests
- [ ] Benchmarks

## Goal

Build a fully working educational database and connect real applications to it.

## Run

1. Tests

```bash
cargo test
```
2. Run app

```bash
cargo run
```