# Cascade: Component-Aware Geocoding with Hierarchical Address Validation

A geocoder and address validation engine that extends traditional geocoding architectures with component-aware processing and graduated hierarchy penalties.

## Overview

Cascade builds upon the proven Carmen geocoder architecture while introducing three key innovations:

1. **Component-Aware Text Processing** - Classifies address elements by type (house numbers, street names, administrative regions, address ranges) during phrase generation
2. **S2 Spatial Indexing** - Replaces Morton encoding with hierarchical S2 cell structures better suited for administrative boundaries
3. **Ranged Address Feature Support** - Enables single features to represent entire address ranges with interpolation capabilities

## Current Status

🚧 **Early Development** - Basic storage layer implementation in progress

### Completed
- ✅ Core data structures (GridKey, MatchKey, MatchOpts, GridEntry)
- ✅ Error handling with backend-agnostic StorageError
- ✅ Type aliases (PhraseId, LanguageSet, FeatureId, RelevScore)
- ✅ Relevance/score encoding functions with unit tests
- ✅ GridKey serialization to database format
- ✅ GridStoreBuilder with optimized BuilderEntry structure
- ✅ Module organization (common, error, builder)

### In Progress
- 🔨 BuilderEntry value serialization
- 🔨 GridStore reader implementation

## Key Concepts

**Storage Backend:** RocksDB for local storage with fast key-value lookups, efficient range queries, and LZ4 compression. Future backends: S3 (distribution), MBTiles (SQLite-based).

**Key Types:**
- `PhraseId` - Unique identifier for phrases (u32, supports 4B phrases)
- `LanguageSet` - 128-bit bitfield for language support
- `FeatureId` - Unique identifier for features (u32, truncated to 24 bits in storage)
- `RelevScore` - Combined relevance+score key (u8, 4 bits each)
- `GridKey` - Phrase + language combination for indexing
- `GridEntry` - Feature at a location with relevance scoring

## Engineering Tasks

### Phase 1: Basic Storage (Current)
- [x] Implement relevance/score encoding
  - [x] relev_float_to_int() - Quantize relevance to 2 bits
  - [x] encode_relev_score() - Combine into single byte
  - [x] pack_feature_id() - Pack feature ID with source phrase hash
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
- [x] Write basic insertion tests
  - [x] Test single entry insertion
  - [x] Test multiple entries for same key
  - [x] Test append merges entries
- [ ] Implement GridStore reader
  - [ ] Create GridStore struct
  - [ ] Implement get() method
  - [ ] Add iterator support for range queries

### Phase 2: Query Support
- [ ] Implement spatial matching
  - [ ] Bounding box queries
  - [ ] Proximity-based ranking
  - [ ] Zoom level coordination
- [ ] Add coalescing/stacking logic
  - [ ] Spatial overlap detection
  - [ ] Relevance score combination
- [ ] Write query tests
  - [ ] Test exact phrase matching
  - [ ] Test range queries (prefix matching)
  - [ ] Test spatial filtering

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

## Documentation

For detailed design documentation, see:
- [Cascade Design Document](../Stuff/src/Things/cascade/cascade_design.md) - Complete architectural specification
- API Documentation: Run `cargo doc --open` to view rustdoc

## References

- [Carmen Geocoder](https://github.com/mapbox/carmen)
- [Carmen Core (Rust)](https://github.com/mapbox/carmen-core)
- [S2 Geometry Library](http://s2geometry.io/)

## License

TBD
