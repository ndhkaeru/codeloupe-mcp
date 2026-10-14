# Tantivy LRU compatibility adapter

Tantivy 0.26.2 requires the lru 0.16 series, which is affected by RUSTSEC-2026-0253. This local package satisfies that dependency's API requirement and re-exports the maintained registry implementation lru 0.18.5. It contains no copied LRU implementation or unsafe code.

The 0.16.5 version identifies the compatibility interface; the implementation and its registry checksum remain visible as lru 0.18.5 in Cargo.lock and cargo tree. The audit has no advisory exceptions. A regression test exercises a panicking key destructor followed by cache insertion, eviction, lookup, and removal.

Keep the implementation pin current with upstream security fixes. Remove this adapter when Tantivy accepts a patched lru version directly.
