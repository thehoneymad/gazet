# Cascade: Component-Aware Geocoding with Hierarchical Address Validation

A geocoder and address validation engine that extends traditional geocoding architectures with component-aware processing and graduated hierarchy penalties.

## Overview

Cascade builds upon the proven Carmen geocoder architecture while introducing three key innovations:

1. **Component-Aware Text Processing** - Classifies address elements by type (house numbers, street names, administrative regions, address ranges) during phrase generation
2. **S2 Spatial Indexing** - Replaces Morton encoding with hierarchical S2 cell structures better suited for administrative boundaries
3. **Ranged Address Feature Support** - Enables single features to represent entire address ranges with interpolation capabilities

## Current Status

🚧 **Phase 2: Query Support** - In Progress

Recent completion:
- ✅ Streaming iterator with BinaryHeap priority queue
- ✅ Deterministic ordering via write-time sorting
- ✅ Basic range query test

Still needed:
- ❌ Spatial filtering (bbox, proximity, zoom)
- ❌ Comprehensive query tests
- ❌ Coalescing/stacking logic

## Documentation

View comprehensive API documentation with rustdoc:

```bash
# Public API documentation
cargo doc --no-deps --open

# Include private modules and implementation details
cargo doc --document-private-items --no-deps --open
```

## Engineering Tasks

### Phase 1: Basic Storage ✅ COMPLETE
- [x] Implement relevance/score encoding
  - [x] relev_float_to_int() - Quantize relevance to 2 bits
  - [x] encode_relev_score() - Combine into single byte
  - [x] decode_relev_score() - Unpack relevance and score
  - [x] pack_feature_id() - Pack feature ID with source phrase hash
  - [x] unpack_feature_id() - Extract ID and hash
  - [x] Unit tests for encoding functions
- [x] Implement GridKey serialization
  - [x] to_db_key() method with type marker support
  - [x] Big-endian phrase_id encoding
  - [x] Compressed language set encoding
- [x] Implement GridStoreBuilder.insert() with serialization
  - [x] Optimized BuilderEntry structure (grouped by relev+score)
  - [x] extend_entries() helper with batch grouping
  - [x] Serialize BuilderEntry to database value format (bincode)
  - [x] Write to RocksDB via finish()
- [x] Implement prefix bin support
  - [x] load_bin_boundaries() - Configure bin boundaries
  - [x] group_by_owned() - Group phrases by bin with owned values
  - [x] copy_entries() - Aggregate BuilderEntry data for bins
  - [x] Update finish() to create PrefixBin entries
  - [x] Store ~BOUNDS metadata in database
- [x] Write basic insertion tests
  - [x] Test single entry insertion
  - [x] Test multiple entries for same key
  - [x] Test append merges entries
- [x] Write prefix bin tests
  - [x] Test finish with no boundaries
  - [x] Test finish with single boundary
  - [x] Test finish with multiple boundaries
  - [x] Test finish with multiple languages
  - [x] Test copy_entries aggregation
- [x] Implement GridStore reader
  - [x] Create GridStore struct with read-only + mmap
  - [x] Implement get() method for exact lookups
  - [x] Add roundtrip tests (write→read verification)
- [x] Abstract boundary encoding/decoding
  - [x] Add encode_boundaries() and decode_boundaries()
  - [x] Add comprehensive tests for boundary serialization

### Phase 2: Query Support (Current)
- [x] Read bin boundaries from database
  - [x] Load ~BOUNDS in GridStore::new()
  - [x] Store bin_boundaries in GridStore struct
  - [x] Add tests for boundary reading
- [x] Implement basic range query support
  - [x] Add MatchKey type (exact phrase or range)
  - [x] Add MatchPhrase enum (Exact/Range)
  - [x] Add MatchOpts type (bbox, proximity, zoom)
  - [x] Add MatchEntry type (query result with metadata)
  - [x] Implement get_matching() basic version
  - [x] Use PrefixBin entries for range queries
  - [x] Language filtering during iteration
- [x] Complete get_matching() implementation
  - [x] Add max_values parameter for result limiting
  - [x] Implement priority queue (BinaryHeap) for top-K results
  - [x] Return streaming iterator (std::iter::from_fn pattern)
  - [x] Deterministic ordering via write-time sorting
  - [x] Basic range query test
- [ ] Implement spatial matching (adds zoom, bboxes, coalesce_radius to GridStore)
  - [ ] Add new_with_options() constructor
  - [ ] Bounding box queries
  - [ ] Proximity-based ranking
  - [ ] Zoom level coordination
  - [ ] Note: Fields added incrementally as features are implemented
  - [ ] Add new_with_options() constructor
  - [ ] Bounding box queries
  - [ ] Proximity-based ranking
  - [ ] Zoom level coordination
  - [ ] Note: Fields added incrementally as features are implemented
- [ ] Add coalescing/stacking logic
  - [ ] Spatial overlap detection
  - [ ] Relevance score combination
- [ ] Write query tests
  - [ ] Test exact phrase matching
  - [ ] Test range queries (prefix matching)
  - [ ] Test prefix bin optimization
  - [ ] Test language filtering
  - [ ] Test spatial filtering
- [ ] Add error logging
  - [ ] Log corrupted database entries during iteration
  - [ ] Log skipped entries in get_matching()

### Phase 3: Component-Aware Processing
- [ ] Define ComponentType enum
  - [ ] HouseNumber, StreetName, AdministrativeRegion
  - [ ] AddressRange, Intersection
- [ ] Extend GridEntry with component metadata
- [ ] Implement component-aware matching
- [ ] Add hierarchy penalty system (0.05, 0.15, 0.25)

### Phase 4: S2 Spatial Indexing
- [ ] Replace x/y coordinates with S2 cell IDs
- [ ] Implement S2 cell covering for ranges
- [ ] Update spatial matching for S2 hierarchy
- [ ] Benchmark S2 vs tile-based performance

### Phase 5: Address Range Support
- [ ] Implement range pattern recognition
- [ ] Add range metadata to GridEntry
- [ ] Implement address interpolation
- [ ] Support parity constraints (odd/even)

### Phase 6: Production Readiness
- [ ] Add comprehensive benchmarks
- [ ] Optimize serialization format
- [ ] Add S3 backend for distribution
- [ ] Documentation and examples
- [ ] Integration tests with real datasets

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

- [Carmen Geocoder](https://github.com/mapbox/carmen)
- [Carmen Core (Rust)](https://github.com/mapbox/carmen-core)
- [S2 Geometry Library](http://s2geometry.io/)

## License

TBD
