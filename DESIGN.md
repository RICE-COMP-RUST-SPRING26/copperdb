# LSM-Tree Storage Engine — Design Reference

A persistent Key-Value storage engine in **Rust** using a Log-Structured Merge-tree (LSM-tree) architecture. Optimized for high write throughput by buffering writes in memory and flushing sequentially to disk.

---

## Critical Paths

### Write Path (`put`, `delete`)

1. **API Receipt:** Tokio server receives the payload.
2. **WAL Append:** Operation is sequentially appended to the active Write-Ahead Log.
3. **MemTable Insertion:** Inserted into the concurrent SkipList MemTable. A `delete` writes a "tombstone" marker.
4. **Threshold Check:** If the active MemTable reaches **64 MB**, it is marked immutable. A new MemTable and WAL are created, and a background thread is queued to flush the immutable MemTable to disk.

### Read Path (`get`)

1. **Memory Search:** Query the active MemTable, then immutable MemTables in reverse chronological order.
2. **Disk Search (SSTables):** Query Level 0 (newest) down to Level N (oldest):
   - Check the in-memory **Bloom Filter** for the target SSTable.
   - If positive, query the in-memory **Sparse Index** to find the 4 KB block offset.
   - Load the block from disk and perform a binary search.

---

## Module Hierarchy

```
src/
├── main.rs          — Entry point. Parses config, inits engine, boots Tokio server.
├── server.rs        — HTTP handlers and routing. Depends only on the public API trait.
├── db.rs            — LsmEngine struct. Glues MemTable, WAL, and versioning together.
├── api.rs           — Public traits (StorageEngine) and common types.
├── memtable.rs      — Wraps crossbeam-skiplist. In-memory insertions and lookups.
├── wal.rs           — Append-only logging and crash recovery replay.
├── sstable/
│   ├── mod.rs
│   ├── writer.rs    — Writes a 64MB .sst file (blocks, filters, index, footer).
│   ├── reader.rs    — Parses footer, loads index/filter, binary searches blocks.
│   └── block.rs     — Encoding/decoding of 4KB data blocks.
├── versioning.rs    — Manages the append-only Manifest file.
├── compaction.rs    — Background worker thread. Leveled K-way merge.
└── iter.rs          — MergingIterator and min-heap logic.
```

---

## Core Interfaces (Traits)

### `StorageEngine` — The API Contract

Decouples the Tokio server from the LSM implementation.

```rust
use async_trait::async_trait;

#[async_trait]
pub trait StorageEngine: Send + Sync {
    async fn put(&self, key: String, value: Vec<u8>) -> Result<(), EngineError>;
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, EngineError>;
    async fn delete(&self, key: String) -> Result<(), EngineError>;
    fn get_iterator(&self, low: String, high: String) -> Box<dyn KvIterator>;
}
```

### `KvIterator` — The Universal Reader

Used by both `get_iterator` and the compaction worker. Implementors: `MemTableIterator`, `SSTableIterator`, `MergingIterator`.

```rust
pub trait KvIterator: Send {
    fn next(&mut self) -> Option<(String, Vec<u8>, u64)>;
    fn is_valid(&self) -> bool;
}
```

### `TableBuilder` — The Storage Writer

Abstracts 4KB blocks, Bloom filters, and sparse indexes away from compaction logic.

```rust
pub trait TableBuilder {
    fn append(&mut self, key: String, value: Vec<u8>, seq_num: u64);
    fn finish(self) -> Result<SSTableMetadata, EngineError>;
    fn estimated_size(&self) -> usize;
}
```

---

## Feature: Write-Ahead Log (WAL)

> **Owner: You (Partner B)**
> **Module: `wal.rs`**
> **Status: TO BE IMPLEMENTED**

### Purpose

The WAL guarantees crash recovery for data that lives in the MemTable but hasn't been flushed to an SSTable yet. It is append-only. Each active MemTable is paired with exactly one WAL file.

### Record Format

Each WAL entry is a binary record with the following layout:

| Field       | Size     | Description                           |
|-------------|----------|---------------------------------------|
| Checksum    | 4 bytes  | Integrity check for the record        |
| Seq Number  | 8 bytes  | Monotonically increasing sequence ID  |
| OpType      | 1 byte   | `Put` or `Delete`                     |
| Key Len     | 4 bytes  | Length of the key in bytes             |
| Key         | Variable | The key bytes                         |
| Value Len   | 4 bytes  | Length of the value in bytes           |
| Value       | Variable | The value bytes (empty for deletes)   |

### Responsibilities

1. **Append:** Every `put` or `delete` must be sequentially appended to the WAL *before* being inserted into the MemTable. This is the durability guarantee.
2. **Recovery/Replay:** On startup, read the WAL file(s) and replay every record back into the MemTable to restore pre-crash state.
3. **Rotation:** When the active MemTable is frozen (hits 64 MB), a new WAL file is created alongside the new active MemTable. The old WAL persists until its paired MemTable has been successfully flushed to an SSTable on disk.
4. **Cleanup:** After a successful SSTable flush, the old WAL file can be safely deleted.

### Integration Points

- **`db.rs` (LsmEngine):** The engine coordinates the WAL. On every write, `db.rs` calls WAL append, then MemTable insert. On startup, `db.rs` calls WAL replay to rebuild the MemTable.
- **`memtable.rs`:** During replay, the WAL feeds records back into the MemTable using the same `put` interface.
- **Background Flusher:** After a successful flush to SSTable, the flusher signals that the old WAL can be deleted.

### Key Design Decisions

- The WAL is **append-only** — never modified in place.
- The **checksum** (CRC32 recommended) protects against partial/corrupt writes from crashes mid-append.
- During recovery, if a record fails its checksum, stop replay at that point — all prior records are valid, and the partial record represents a write that never completed.
- Each WAL file should be named or numbered to associate it with its MemTable generation.

---

## Feature: MemTable

> **Owner: Partner A**
> **Module: `memtable.rs`**
> **Status: ALREADY IMPLEMENTED**

### Summary

Lock-free concurrent skip list (`crossbeam-skiplist`) serving as the in-memory write buffer. Supports MVCC via composite `InternalKey` (user_key ascending, seq_num descending).

### Key Structures

- **`Record`** — Enum: `Put(Vec<u8>)` or `Delete` (tombstone).
- **`InternalKey`** — Composite key `{ user_key: String, seq_num: u64 }`. Sorted ascending by key, descending by seq_num.
- **`CrossbeamMemTable`** — The physical implementation. Fields: `map: Arc<SkipMap<InternalKey, Record>>`, `approximate_size: AtomicUsize`, `active_writers: AtomicUsize`.
- **`MemTableIterator`** — Iterates the skip list with eager key caching for safe concurrent reads.

### State Management (`state.rs`)

- **`MemTableState`** — Thread-safe orchestrator using `RwLock<Arc<InnerState>>` (Copy-on-Write pattern).
- **`put()`** returns `true` when the active table needs to be frozen, signaling the engine to wake the flusher.
- **`freeze_active()`** uses double-checked locking with `Arc::ptr_eq()` to prevent thundering herd races.
- **`get_oldest_immutable()`** / **`drop_immutable()`** — Used by the background flusher to safely consume and release frozen tables.

### Concurrency Mitigations

- **Thundering Herd:** Double-checked locking on freeze with `Arc::ptr_eq`.
- **Missed Write:** `active_writers` atomic barrier — flusher spin-waits until zero before iterating.
- **Lock Contention:** CoW via `RwLock<Arc<T>>` — readers clone the Arc in nanoseconds.
- **Use-After-Free:** Arc reference counting keeps memory alive for slow readers.
- **Shifting Ground:** Eager next-key caching with fresh `lower_bound` searches per iteration.

### Configuration

- Active MemTable size limit: **64 MB**
- Max immutable MemTables in RAM: **4** (backpressure stalls writes if exceeded)

---

## Feature: SSTables (On-Disk Storage)

> **Module: `sstable/` (writer.rs, reader.rs, block.rs)**

### File Layout

Each SSTable targets **64 MB**, divided into **4 KB Data Blocks**:

1. **Data Blocks** — Sequentially stored, sorted KV pairs.
2. **Meta Block (Bloom Filter)** — Serialized Bloom Filter for all keys in the file.
3. **Index Block (Sparse Index)** — Highest key of every 4 KB block + byte offset.
4. **Footer** — Fixed-size trailer (~48 bytes) with pointers to Meta and Index blocks.

### Startup Loading

On startup, the engine reads the Footer of each valid SSTable (per the Manifest), loads the Bloom Filter and Sparse Index into RAM, and keeps them resident for the file's lifetime.

---

## Feature: Manifest (Version Tracking)

> **Module: `versioning.rs`**

### Purpose

Append-only log of metadata changes that tracks the state of the LSM-tree across crashes.

### Mechanics

- On flush or compaction completion, a "Version Edit" is appended (e.g., `+ SSTable 15 to L1`, `- SSTable 8 from L0`).
- On startup, the engine replays the Manifest to rebuild the in-memory metadata table tracking every active SSTable file, its level, and its smallest/largest key.

---

## Feature: Leveled Compaction

> **Module: `compaction.rs`, `iter.rs`**

### Level Rules

- **Level 0 (L0):** Files flushed directly from MemTables. Key ranges **can overlap**.
- **Level 1+ (L1–LN):** Files **must not overlap**. Strict global order by smallest key.

### Compaction Trigger

When a level exceeds its size threshold (e.g., L1 > 640 MB):

1. **Pick Victim:** Select a file from L_i.
2. **Find Overlaps:** Find all files in L_{i+1} whose key ranges overlap the victim.
3. **Merge:** Stream KV pairs from all selected files into a min-heap. Filter out overwritten keys and stale tombstones.
4. **Write & Swap:** Write output to new 64 MB SSTable files in L_{i+1}. Append a Version Edit to the Manifest, update in-memory metadata, delete old files.

### Compaction Pseudocode

```rust
fn compact_sstables(input_files: Vec<SSTable>) -> Vec<SSTable> {
    let mut min_heap = PriorityQueue::new();
    let mut current_writer = SSTableWriter::new();
    let mut new_sstables = Vec::new();

    for (id, file) in input_files.iter().enumerate() {
        let mut iter = file.get_iterator();
        if let Some((k, v, seq)) = iter.next() {
            min_heap.push(HeapItem { k, v, seq, id, iter });
        }
    }

    let mut last_key = None;
    while let Some(mut item) = min_heap.pop() {
        if last_key == Some(item.k.clone()) {
            advance_iter(&mut item, &mut min_heap);
            continue;
        }
        if !is_stale_tombstone(&item) {
            if current_writer.size() >= MAX_FILE_SIZE {
                new_sstables.push(current_writer.finish());
                current_writer = SSTableWriter::new();
            }
            current_writer.append(item.k.clone(), item.v.clone());
        }
        last_key = Some(item.k.clone());
        advance_iter(&mut item, &mut min_heap);
    }

    if current_writer.size() > 0 {
        new_sstables.push(current_writer.finish());
    }
    new_sstables
}
```

---

## Feature: Tokio HTTP Server

> **Module: `server.rs`**

Async HTTP server wrapping the engine in an `Arc`. Exposes REST endpoints (e.g., `POST /kv/{key}`, `GET /kv/{key}`). Depends only on the `StorageEngine` trait, never on internal engine structs.

---

## Stretch Goal: Distributed Replication (Raft)

Scale the engine using the **Raft consensus algorithm**. The Raft log serves as the distributed WAL. Each node is a replicated state machine — writes are applied to the local LSM-tree only after a majority of nodes acknowledge.

---

## Timeline

| Phase | Dates | Partner A | Partner B (You) |
|-------|-------|-----------|-----------------|
| **1: In-Memory Engine & Durability** | Mar 30 – Apr 4 | SkipList MemTable + core API | **WAL:** append logic + recovery replay |
| **2: Disk Persistence** | Apr 5 – Apr 9 | SSTable Writer | Background flusher + **Manifest file** |
| **3: Read Path & Compaction** | Apr 10 – Apr 13 | SSTable Reader + full get path | **Compaction Worker** (K-way merge) |
| **4: Networking & Polish** | Apr 14 – Apr 16 | Tokio HTTP Server | Integration testing + benchmarking |

### Milestones

- **Apr 4:** Write, read, and survive a process kill without data loss.
- **Apr 9:** Engine no longer runs out of memory. L0 `.sst` files created on disk.
- **Apr 13:** Full read path from disk. Compaction prevents infinite disk growth.
- **Apr 16:** Final submission.

---

## Recommended Crates

Don't reimplement what already exists — use stable crates:

- `crossbeam` — lock-free SkipList
- `tokio` — async runtime
- `bloomfilter` — Bloom filter implementation
- `async-trait` — async trait support
- `crc32fast` or similar — WAL checksum
