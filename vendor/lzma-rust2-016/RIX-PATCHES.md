# RixFiles ZIP decoder limits on lzma-rust2 0.16.5

This is the Apache-2.0 upstream 0.16.5 package. Its license, API and version are
preserved. Only the older version used by ZIP is patched; the application and
7z continue using 0.21.0. No Cargo registry files are modified.

ZIP 8.6.0 calls raw LZMA constructors and XzReader::new, so limiting the standalone
XZ reader in RixFiles would leave the same dictionary allocation reachable inside
ZIP. Two checks close that route without adding a system/native decoder:

- Raw LZMA checks the effective dictionary and probability-model memory against
  128 MiB after safe reduction to the declared output size, before range-reader
  setup or codec-state allocation.
- XZ totals the declared LZMA2 memory across the filter chain, with checked
  addition, and rejects above 128 MiB before constructing any filter/decoder.

The limit is decoder state, not a hard process RSS limit. Existing application
output, entry-count and preview limits remain in place. Encoders and the newer
0.21.0 API are unchanged.

Unit regressions exercise raw LZMA and XZ constructors. RixFiles also runs tiny
independent ZIP method 14/95 fixtures to check the real outer-container path.
Upstream benchmark/reference-test dev dependencies are retained in the vendor
manifest and are unnecessary for the application regression suite.
