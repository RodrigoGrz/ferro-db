# ferroDB

![image](ferrodb.png)

A small relational database implemented from scratch in Rust, inspired by database systems concepts from CMU Database Systems courses.

The goal of ferroDB is not to compete with PostgreSQL, MySQL, or other production databases.

The goal is to understand how a relational database works internally by implementing its main components from the ground up and eventually having a real, usable SQL database that small applications can connect to.

## Features (WIP)

## Storage

- [x] Disk Manager
- [x] Page
- [x] Buffer Pool Manager
- [x] LRU-K Replacement
- [x] Page Serialization
- [x] Slotted Pages
- [x] Tuple Storage
- [x] RID
- [x] Heap File
- [ ] Free Space Management

## Data Types

- [x] Integer
- [ ] Float
- [ ] Boolean
- [ ] String
- [x] NULL
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

## Query Planning

- [ ] Logical Query Plan
- [ ] Physical Query Plan

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

## Catalog

- [ ] Database Catalog
- [ ] Table Metadata
- [ ] Column Metadata
- [ ] Index Metadata
- [ ] Constraint Metadata

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