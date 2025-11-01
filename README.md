# Cascade: Component-Aware Geocoding with Hierarchical Address Validation

A geocoder and address validation engine that extends traditional geocoding architectures with component-aware processing and graduated hierarchy penalties.

## Overview

Cascade builds upon the proven Carmen geocoder architecture while introducing three key innovations:

1. **Component-Aware Text Processing** - Classifies address elements by type (house numbers, street names, administrative regions, address ranges) during phrase generation
2. **S2 Spatial Indexing** - Replaces Morton encoding with hierarchical S2 cell structures better suited for administrative boundaries
3. **Ranged Address Feature Support** - Enables single features to represent entire address ranges with interpolation capabilities

## Current Status

✅ **Phase 2: Query Support** - COMPLETE

All core query and coalescing functionality is implemented and tested.

## Functional Differences from Carmen-Core

Cascade is a complete port of carmen-core's query and coalescing system with the following intentional differences:

### Performance
- **Sequential Processing**: Cascade uses single-threaded execution for simplicity
  - Carmen-core uses `rayon` for parallel processing with `Send + Sync` bounds
  - Same results, but carmen-core is faster on multi-core systems
  - **Future work**: Add parallel processing with rayon for production workloads

### Dependencies
- **IntervalHeap**: Uses `interval-heap` crate for priority queues
  - Carmen-core uses `min-max-heap`
  - Both provide O(log n) min/max operations with identical behavior
  
- **Error Handling**: Uses custom `Result<T, StorageError>` type
  - Carmen-core uses `failure::Error`
  - Functionally equivalent, different error types

### API
- **Method Names**: `get_matching()` instead of `streaming_get_matching()`
  - Same functionality, clearer naming

### Algorithmic Equivalence
All core algorithms are identical to carmen-core:
- ✅ Single-phrase coalescing with deduplication
- ✅ Multi-phrase coalescing with zoom-based stacking
- ✅ Tree-based coalescing with stackable tree traversal
- ✅ Spatial overlap detection using KDBush
- ✅ Quota-based query limiting for high-zoom indexes
- ✅ Relevance penalties for single-entry and ascending stacks

## Test Coverage

**53 passing tests** covering:
- Storage layer (27 tests): insertion, retrieval, prefix bins, boundaries
- Spatial filtering (10 tests): proximity, bbox, language penalties
- Stackable trees (4 tests): type hierarchy, mask compatibility, bmask filtering
- Coalescing (3 tests): single-phrase, multi-phrase, proximity ranking
- Builder (9 tests): encoding, serialization, roundtrip verification

## Documentation

View comprehensive API documentation with rustdoc:

```bash
# Public API documentation
cargo doc --no-deps --open

# Include private modules and implementation details
cargo doc --document-private-items --no-deps --open
```

## Architecture

### Phase 1: Basic Storage ✅ COMPLETE

Complete RocksDB-backed storage layer with memory-mapped reads, prefix bin optimization, and language filtering.

### Phase 2: Query Support ✅ COMPLETE

Complete multi-phrase coalescing system with spatial stacking, type hierarchy, and proximity-based ranking.

### Phase 3: Parallel Processing (Future)

Add parallel processing to tree_coalesce for production performance. Currently sequential for simplicity.

| Component | Description | Implementation Details |
|-----------|-------------|------------------------|
| **Trait Bounds** | Add `Send + Sync` to generic types | Update `tree_coalesce<T: Borrow<GridStore> + Clone + Debug + Send + Sync>` |
| **Rayon Dependency** | Add parallel iterator support | Add `rayon = "1.10"` to Cargo.toml |
| **Phase 1: Key Fetching** | Parallelize data fetching from GridStore | Replace `for key_step in keys` with `keys.into_par_iter().map()` |
| **KeyFetchResult Collection** | Collect parallel results safely | Use `Vec<Result<KeyFetchResult>>` with error handling |
| **Phase 2: Step Processing** | Parallelize coalesce step execution | Replace `for step in step_chunk` with `step_chunk.into_par_iter().map()` |
| **Thread-Safe State** | Ensure TreeCoalesceState is thread-safe | Already uses `Arc<TreeCoalesceState>` - no changes needed |
| **Data Cache** | Handle concurrent cache access | Current `HashMap` is read-only after phase 1 - safe for parallel phase 2 |
| **Context Aggregation** | Merge parallel results into main queue | Iterate over `chunk_results` and push to `contexts` queue |
| **Error Propagation** | Handle errors from parallel operations | Use `?` operator on `Result` from parallel map |

**Performance Impact**: 2-4x speedup on multi-core systems for complex multi-phrase queries with 8+ parallel workers.

**Testing Strategy**: 
- Verify identical results between sequential and parallel implementations
- Add benchmark comparing sequential vs parallel performance
- Test with varying COALESCE_CHUNK_SIZE (current: 8)

**Code Changes Required**:
```rust
// Phase 1: Parallel key fetching
let key_data: Vec<Result<KeyFetchResult>> = keys
    .into_par_iter()  // Change from sequential to parallel
    .map(|key_step| {
        // Existing key fetch logic
    })
    .collect();

// Phase 2: Parallel step processing  
let chunk_results: Vec<Result<(Vec<CoalesceContext>, Vec<CoalesceStep<T>>)>> = 
    step_chunk
        .into_par_iter()  // Change from sequential to parallel
        .map(|step| {
            // Existing step processing logic
        })
        .collect();
```

**Estimated Effort**: 2-3 hours
- Add rayon dependency and imports
- Update trait bounds with Send + Sync
- Replace sequential loops with par_iter
- Test for correctness and performance
- Update documentation

### Phase 4: Component-Aware Processing (Future)

| Component | Description |
|-----------|-------------|
| **ComponentType Enum** | Define HouseNumber, StreetName, AdministrativeRegion, AddressRange, Intersection |
| **GridEntry Extension** | Add component_type field to GridEntry |
| **Component Matching** | Implement component-specific matching strategies |
| **Hierarchy Penalties** | Apply graduated penalties (0.05, 0.15, 0.25) based on missing levels |

### Phase 5: S2 Spatial Indexing (Future)

| Component | Description |
|-----------|-------------|
| **S2 Cell IDs** | Replace x/y coordinates with S2 Level 16 cell IDs |
| **S2 Cell Covering** | Implement S2 covering algorithm for address ranges |
| **Spatial Matching** | Update spatial queries to use S2 hierarchy |
| **Performance Benchmarks** | Compare S2 vs tile-based performance |

### Phase 6: Address Range Support (Future)

| Component | Description |
|-----------|-------------|
| **Range Recognition** | Pattern matching for house number ranges, ZIP+4 ranges |
| **Range Metadata** | Store boundaries, parity constraints, interpolation geometry |
| **Address Interpolation** | Calculate coordinates for addresses within ranges |
| **Parity Validation** | Enforce odd/even constraints for house numbers |

### Phase 7: Production Readiness (Future)

| Component | Description |
|-----------|-------------|
| **Benchmarks** | Comprehensive performance benchmarks for all operations |
| **Serialization** | Optimize encoding format for size and speed |
| **S3 Backend** | Add S3 support for distributed index storage |
| **Error Logging** | Log corrupted entries and skipped results |
| **Integration Tests** | Real-world dataset testing |

## Building

### Prerequisites

**macOS:**
```bash
brew install llvm
```

The project is configured to use Homebrew LLVM automatically via `.cargo/config.toml`.

**Linux:**
```bash
# Ubuntu/Debian
apt-get install clang libclang-dev

# Fedora/RHEL
dnf install clang clang-devel
```

### Build Commands

```bash
# Check compilation
cargo check

# Build
cargo build

# Run tests
cargo test

# Build with optimizations
cargo build --release

# Generate documentation
cargo doc --open
```

## References

Cascade builds upon geocoding concepts pioneered by the Carmen project:
- [Carmen Geocoder](https://github.com/mapbox/carmen) - Original JavaScript implementation
- [Carmen Core (Rust)](https://github.com/mapbox/carmen-core) - Rust port with performance optimizations
- [S2 Geometry Library](http://s2geometry.io/) - Hierarchical spatial indexing

## License

TBD
