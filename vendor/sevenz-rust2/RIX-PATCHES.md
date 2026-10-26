# RixFiles resource limits on sevenz-rust2 0.23.0

This is the Apache-2.0 upstream Rust package, vendored without version changes.
The upstream `LICENSE` and source attribution remain intact. Cargo patches this
package for RixFiles; no system library or Cargo registry source is modified.

The upstream reader passed an unlimited budget to codecs. LZMA1 did not check
that budget, and an encoded header could inflate until its declared decoded size.
RixFiles needs bounded in-process decoding while the desktop is under load.

Local changes:

- Reject raw and decoded headers above 16 MiB before allocation. Raw headers use
  checked reservation. Encoded-header size conversion is checked.
- Cap files, blocks, pack streams and aggregate substreams at 100,000 before
  allocating count-derived vectors. Cap coder chains at 32, coder property bytes
  at 4096, and input/output streams at 128. Aggregate count additions are checked.
- Estimate and check the entire block's decoder chain against 128 MiB before
  constructing any decoder, including encoded headers and BCJ2 pipelines.
- Check LZMA1 dictionary/probability memory as well as LZMA2 and PPMd before
  allocating codec state. Account for enabled BZip2, Deflate, filters and AES.
- Bound Zstandard frame windows at 64 MiB, with 66 MiB reserved in the estimate.
- Present the exceeded decoder budget as a readable error with MiB amounts.

RixFiles sets serial decoding. These are conservative decoder-state budgets,
not a hard process RSS cap: parsed file metadata, input buffers, the rest of the
application and future optional codecs are separate. The application still caps
entry count, decoded output size and preview size, and supports cancellation.
RixFiles does not enable optional Brotli/LZ4 or multithreaded LZMA2 decoding; those
features would need their own memory accounting before being enabled.

Regression coverage: LZMA1/LZMA2 and PPMd oversized dictionaries are rejected
before constructing decoders; a chained pair below the individual limit exceeds
the total budget; counts are limited independently of input header size. RixFiles
also tests independent tiny 7z fixtures for data dictionaries, encoded-header
dictionaries and encoded-header expansion, alongside a normal 7z fixture.
