# Refactoring Plan for Remaining Code Review Comments

## Status: In Progress

This document tracks the remaining architectural changes needed based on code review feedback.

## Completed ✅

- [x] **GPG Library Replacement** (Comment on crypto/gpg.rs:12)
  - Replaced `pgp` crate with `gpg` CLI tool
  - Removed unused `decrypt_with_private_key` function
  - Removed unused `find_public_key` function
  - Commit: 483e502

## Remaining Changes

### 1. Filesystem URI Parsing (Comment on fs/mod.rs:62)

**Issue**: Common implementation of `parse_uri` should not exist. Each filesystem should have its own parser.

**Current Code**:
```rust
fn parse_uri(&self, uri: &str) -> Result<SnapshotSource> {
    SnapshotSource::parse(uri)  // Common implementation
}
```

**Required Changes**:
- Remove `SnapshotSource::parse()` common implementation
- Each filesystem implementation (ZFS, Dummy) should implement `parse_uri` method
- ZFS should parse URIs like `zfs:pool/dataset@snapshot`
- Dummy should parse URIs like `dummy:volume@snapshot`
- Return type should be filesystem-specific or generic

**Benefits**:
- More flexible parsing for different filesystem types
- Each filesystem can validate its own URI format
- Allows for filesystem-specific extensions

### 2. Filesystem Instantiation (Comment on main.rs:91)

**Issue**: Parsing filesystem type from URI and creating filesystem struct should be in `fs/` module, not main.rs.

**Current Code** (in main.rs):
```rust
let source_info = fs::SnapshotSource::parse(&source)?;
let fs: Box<dyn fs::SnapshotableFilesystem> = match source_info.filesystem_type.as_str() {
    "zfs" => Box::new(fs::zfs::ZfsFilesystem::new()),
    "dummy" => Box::new(...),
    ...
}
```

**Required Changes**:
- Create a factory function in `fs/mod.rs` like `fs::create_from_uri(uri: &str)`
- Move the match logic from main.rs into fs/mod.rs
- Parse filesystem type from URI prefix
- Return `(Box<dyn SnapshotableFilesystem>, SnapshotInfo)`

**Example**:
```rust
// In fs/mod.rs
pub fn create_from_uri(uri: &str) -> Result<(Box<dyn SnapshotableFilesystem>, SnapshotInfo)> {
    let fs_type = uri.split(':').next().ok_or(...)?;
    match fs_type {
        "zfs" => {
            let fs = zfs::ZfsFilesystem::new();
            let info = fs.parse_uri(uri)?;
            Ok((Box::new(fs), info))
        }
        "dummy" => { ... }
        _ => Err(anyhow!("Unknown filesystem type"))
    }
}

// In main.rs
let (fs, snapshot_info) = fs::create_from_uri(&source)?;
```

### 3. EncryptingStream Fix (Comment on crypto/aes.rs:70)

**Issue**: Current `EncryptingStream` implementation buffers entire stream in memory. Should work as a proper streaming pipeline.

**Current Problem**:
- Reads all plaintext into memory
- Encrypts everything at once
- Buffers encrypted output
- Not suitable for large snapshots

**Required Implementation**:
- Read small chunks from input reader
- Encrypt each chunk on-the-fly
- Write encrypted chunks to output
- Use AES-GCM in streaming mode or chunk the data properly
- Keep memory usage bounded regardless of input size

**Technical Considerations**:
- AES-GCM doesn't naturally support streaming
- May need to encrypt fixed-size chunks with separate nonces
- Or use a different cipher mode that supports streaming (like AES-CTR with HMAC)
- Need to handle chunk boundaries correctly

**Example Structure**:
```rust
impl<R: AsyncRead + Unpin> AsyncRead for EncryptingStream<R> {
    fn poll_read(...) -> Poll<Result<()>> {
        // Read chunk from inner reader
        // Encrypt just that chunk
        // Return encrypted chunk
        // Never buffer entire stream
    }
}
```

### 4. Human-Readable Rate Limit (Comment on main.rs:54)

**Issue**: Rate limit should accept human-readable units like `10kbps`, `10mbps` instead of just bytes.

**Current Code**:
```rust
#[arg(short, long)]
rate_limit: Option<u64>,  // Bytes per second
```

**Required Changes**:
- Change type to `Option<String>`
- Parse string to extract number and unit
- Support units: `bps`, `kbps`, `Mbps`, `Gbps`, `KB/s`, `MB/s`, etc.
- Convert to bytes per second
- Update help text to clarify it's upload speed limit

**Example**:
```rust
fn parse_rate_limit(s: &str) -> Result<u64> {
    // Parse "10kbps" -> 10 * 1000
    // Parse "5MB/s" -> 5 * 1024 * 1024
    // Parse "100" -> 100 (assume bytes)
}
```

**Help Text**:
```
--rate-limit <RATE_LIMIT>
    Upload speed limit (e.g., 10kbps, 5MB/s, 100Mbps)
```

## Implementation Priority

1. **Human-Readable Rate Limit** - Simple string parsing, low risk
2. **Filesystem Instantiation** - Moderate refactoring, moves logic to better location
3. **Filesystem URI Parsing** - Requires coordination with #2
4. **EncryptingStream Fix** - Complex, requires careful crypto implementation

## Notes

- Each change should be implemented, tested, and committed separately
- Maintain backward compatibility where possible
- Update tests for each change
- Update documentation to reflect new behavior
