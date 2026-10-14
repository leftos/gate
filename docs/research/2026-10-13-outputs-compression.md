# What makes a routed run's outputs fetch smallest in total time (GATE-221)

Date: 2026-10-13. Written for GATE-217 (the outputs archive change), from the GATE-191 trial's outputs.

## Question

The outputs archive is a tar stream compressed with zstd level 3, single-threaded (`crates/gate/src/outputs/archive.rs`, `LEVEL`; `Encoder::new` with no worker count). Which algorithm, level and window, with or without sending each distinct file once, brings yaat's `bin/` outputs back in the least total time: compress on the box, transfer, decompress on the PC?

## Method

- **Machine.** AMD Ryzen 7 7800X3D, 8 cores, 16 threads, shared with other sessions' builds. Every run went through the gate launcher. Both ends were measured on this one machine; the builder's CPU was not measured.
- **Data.** The yaat main checkout's files under the default globs (`**/bin/**`, `**/.godot/mono/temp/bin/**`), walked as the gate walks (excluded folders, links and junctions skipped). The checkout holds Debug and Release builds, 22,199 files and 28.84 GiB. The Release half matches the trial's 14.4 GiB: **11,097 files, 15,449,392,155 bytes (14.39 GiB)**.

  The trial counted 9,085 files; the difference is unexplained. No file changed or vanished while it was read (0 skipped).
- **Stream.** One tar (GNU headers, mode 0644, the file's mtime) of those files in the gate's member order: sorted by `/`-separated relative path, as `walk` sorts them. The list member `.gate-outputs.json` is last, as `pack` writes it. The tar is 15,461,744,640 bytes.
- **Dedup set.** The same tar holding each distinct SHA-256 content once (its first occurrence in member order), then its list: 3,694 files, 4,055,787,520 bytes.
- **Codecs.** Python 3.14.7 with `zstandard` 0.25.0 (libzstd 1.5.7), `lz4` 4.4.5 (liblz4 1.9.4; frame format, 4 MiB linked blocks, default level), `brotli` 1.2.0 (window 2^24), and the standard library's `lzma` (xz container). Every codec ran single-threaded, as the gate's zstd does.

  `-long` is zstd's long-distance matching with a 2^27 window, as the CLI's `--long`. Each compress and each decompress ran in its own process. Times count only the codec calls, from a file in the OS cache. Peak memory is the process's peak working set, about 20 MiB of it the Python interpreter. Every decompress was checked against the tar's SHA-256; all matched.
- **Full and sample.** lz4 and zstd 1, 3 and 9, with and without `-long`, ran on the full and dedup sets. zstd 19, xz 6 and 9e and brotli 9 ran on a sample only.

  The sample is the whole Release `bin/` folders of four projects: one test project, its tool, one other test project and a recorder tool. That is 1,131 files, 1,789,470,720 bytes (1.665 GiB). Its byte mix by file type matches the full set to within 4 points per type (`.so` 30% vs 28%, `.dll` 28% vs 32%, other 21% vs 21%, `.pdb` 18% vs 15%).

  Its duplicate share (15%) is far below the full set's (74%): every project combination of 1 to 2 GiB was tried, and none comes near it, because the duplicates are copies across projects.
- **Extrapolation, marked (x).** A slow candidate's size on the full or dedup set is the reference fast candidate's size there, times the two's size ratio on the sample. The reference is zstd 9 for windows up to 16 MiB (zstd 19, xz 6, brotli 9) and zstd 9 `-long` for the large windows (zstd 19 `-long`, xz 9e at 64 MiB).

  Its times are its sample times scaled by input bytes. The five slow candidates ran at once, one process each, so their times carry that contention; the fast ones ran alone.
- **Link.** The trial pushed 603.01 MiB in 224.1 s (2.7 MiB/s, PC to builder) and fetched the outputs in about 600 s with the size not logged. Taking that fetch as today's zstd 3 archive of this set (5,955.7 MiB) gives an estimated **9.93 MiB/s** builder to PC.

  Total time = compress + compressed size / rate + decompress, scored at 9.93, 2.7 and 100 MiB/s.

## Duplicates

| | files | bytes | GiB |
|---|---:|---:|---:|
| all | 11,097 | 15,449,392,155 | 14.39 |
| distinct contents | 3,694 | 4,051,704,129 | 3.77 |
| duplicates (each copy after the first) | 7,403 | 11,397,688,026 | 10.61 |

**73.8% of the bytes are copies of another file in the set.** The copies sit in different projects' `bin/` folders, so in member order they are hundreds of MiB to GiB apart: past every window measured here, the 128 MiB `-long` window included.

Most duplicated contents, by bytes a dedup would not send:

| file | MiB | copies | MiB saved |
|---|---:|---:|---:|
| libSkiaSharp.pdb (one runtime) | 82.7 | 7 | 496.5 |
| libSkiaSharp.pdb (a second) | 81.0 | 7 | 486.0 |
| libSkiaSharp.pdb (a third) | 80.1 | 7 | 480.8 |
| cublasLt64_13.dll | 458.4 | 2 | 458.4 |
| libonnxruntime.1.24.4.dylib | 33.8 | 14 | 439.1 |
| LM-Kit.onnxruntime.dylib | 30.4 | 14 | 394.8 |
| libonnxruntime.1.24.4.dylib (a second) | 28.3 | 14 | 367.3 |
| LM-Kit.ggml.backend.vulkan.dll | 50.5 | 7 | 302.9 |
| LM-Kit.ggml.backend.vulkan.so | 49.2 | 7 | 295.3 |
| LM-Kit.ggml.backend.vulkan.so (a second) | 49.0 | 7 | 294.0 |

Copies per distinct content: 2,335 contents appear once, 680 twice, 346 seven times and 216 nineteen times. The largest file is 458.4 MiB.

What dedup costs outside the codec, measured on this machine:

- **Box.** One SHA-256 pass over all 11,097 files before packing took 34.9 s (one thread, files in the OS cache). Hashing only the files whose size another file shares would not save much: those hold 89% of the bytes.
- **PC.** Copying the 7,403 duplicates from their first copy into place on the PC's NTFS drive took 8.6 s for 10.61 GiB. This counts the time until the copy calls return, not until the data is flushed to disk.

The dedup totals below include both, 43.5 s.

## Results: the full set (today's archive is zstd3)

| candidate | MiB sent | % of tar | compress s | decompress s | peak MiB c/d | total s @ 9.93 | total s @ 2.7 | total s @ 100 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lz4 | 8,253 | 56.0 | 28.6 | 24.5 | 32/31 | 885 | 3,110 | 136 |
| zstd1 | 6,474 | 43.9 | 31.6 | 17.2 | 26/23 | 701 | 2,447 | 114 |
| zstd1-long | 5,434 | 36.9 | 62.9 | 13.8 | 162/151 | 624 | 2,089 | 131 |
| **zstd3 (today)** | 5,956 | 40.4 | 85.7 | 21.3 | 29/25 | 707 | 2,313 | 167 |
| zstd3-long | 5,108 | 34.6 | 91.7 | 14.5 | 163/151 | 621 | 1,998 | 157 |
| zstd9 | 5,532 | 37.5 | 385.6 | 27.1 | 41/27 | 970 | 2,462 | 468 |
| zstd9-long | 4,798 | 32.5 | 163.2 | 13.3 | 181/151 | 660 | 1,954 | 224 |
| zstd19 (x) | 4,701 | 31.9 | 6,975 | 18.9 | 114/31 | 7,467 | 8,735 | 7,041 |
| zstd19-long (x) | 4,155 | 28.2 | 7,117 | 17.8 | 297/151 | 7,553 | 8,673 | 7,176 |
| xz6 (x) | 4,205 | 28.5 | 6,691 | 128.6 | 117/31 | 7,243 | 8,377 | 6,862 |
| xz9e (x) | 3,742 | 25.4 | 7,981 | 113.4 | 697/87 | 8,471 | 9,480 | 8,131 |
| brotli9 (x) | 5,014 | 34.0 | 5,457 | 85.8 | 132/44 | 6,048 | 7,400 | 5,593 |

## Results: the dedup set (each distinct file once; totals include the 43.5 s hash pass and copies)

"% of tar" is against the full tar, so it reads as the share of today's input that travels.

| candidate | MiB sent | % of full tar | compress s | decompress s | peak MiB c/d | total s @ 9.93 | total s @ 2.7 | total s @ 100 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lz4 | 2,516 | 17.1 | 8.5 | 7.3 | 33/31 | 313 | 991 | **84** |
| zstd1 | 2,104 | 14.3 | 18.9 | 7.7 | 26/23 | 282 | 849 | 91 |
| zstd1-long | 1,808 | 12.3 | 19.4 | 7.0 | 162/151 | **252** | 739 | 88 |
| zstd3 | 1,908 | 12.9 | 25.7 | 7.2 | 28/25 | 269 | 783 | 95 |
| **zstd3-long** | 1,713 | 11.6 | 24.8 | 10.9 | 163/151 | **252** | **714** | 96 |
| zstd9 | 1,760 | 11.9 | 45.1 | 3.4 | 40/27 | 269 | 744 | 110 |
| zstd9-long | 1,624 | 11.0 | 66.2 | 4.4 | 180/151 | 278 | 716 | 130 |
| zstd19 (x) | 1,496 | 10.1 | 1,830 | 5.0 | 114/31 | 2,029 | 2,432 | 1,893 |
| zstd19-long (x) | 1,406 | 9.5 | 1,867 | 4.7 | 297/151 | 2,057 | 2,436 | 1,929 |
| xz6 (x) | 1,338 | 9.1 | 1,755 | 33.7 | 117/31 | 1,967 | 2,328 | 1,846 |
| xz9e (x) | 1,267 | 8.6 | 2,093 | 29.7 | 697/87 | 2,294 | 2,636 | 2,179 |
| brotli9 (x) | 1,595 | 10.8 | 1,432 | 22.5 | 132/44 | 1,658 | 2,088 | 1,513 |

## Results: the sample (all measured, five slow candidates run at once)

| candidate | MiB sent | % of sample tar | compress s | decompress s | peak MiB c/d |
|---|---:|---:|---:|---:|---:|
| lz4 | 849.1 | 49.8 | 3.5 | 2.0 | 32/30 |
| zstd1 | 619.6 | 36.3 | 9.7 | 4.5 | 25/23 |
| zstd1-long | 537.2 | 31.5 | 8.3 | 1.7 | 160/151 |
| zstd3 | 567.7 | 33.3 | 8.0 | 2.6 | 28/25 |
| zstd3-long | 498.3 | 29.2 | 9.5 | 1.8 | 161/151 |
| zstd9 | 522.4 | 30.6 | 22.3 | 2.2 | 39/27 |
| zstd9-long | 461.4 | 27.0 | 24.2 | 3.4 | 178/151 |
| zstd19 | 444.0 | 26.0 | 807.2 | 2.2 | 114/31 |
| zstd19-long | 399.6 | 23.4 | 823.6 | 2.1 | 297/151 |
| xz6 | 397.0 | 23.3 | 774.4 | 14.9 | 117/31 |
| xz9e | 359.9 | 21.1 | 923.6 | 13.1 | 697/87 |
| brotli9 | 473.5 | 27.7 | 631.6 | 9.9 | 132/44 |

brotli 11 ran only on a 29 MiB piece (one small project's `bin/`). It compressed at 0.34 MiB/s, which is about 12 hours for the full set, so it was not run on the sample.

## Streaming, memory and worst-case expansion

All four formats stream with bounded memory at both ends. The bound is set by the window or dictionary, not by the input. The expansion columns were measured on 32 MiB of random bytes, one stream each. The bound formulas are the libraries' own bound functions and were not measured.

| format | memory bound (measured peak c/d at these settings) | measured expansion on incompressible input | library bound | multithreaded compress |
|---|---|---:|---|---|
| zstd, window ≤ 8 MiB (levels 1 to 19) | window + tables (26 to 114 / 23 to 31 MiB) | +777 B per 32 MiB (24.3 B/MiB) | `ZSTD_compressBound`: n + n/256, plus a little under 128 KiB | yes: libzstd workers, one frame, any level, `-long` included |
| zstd `-long` (window 2^27) | 128 MiB window at both ends (162 to 297 / 151 MiB) | same as zstd | same as zstd | yes, as above |
| lz4 frame | 2 × 4 MiB blocks (32/31 MiB) | +43 B | `LZ4F_compressFrameBound`: n + 4 B per block + header and end mark | no, not in the frame library |
| xz / LZMA2 | dictionary: 8 MiB at 6, 64 MiB at 9e (117/31, 697/87 MiB) | +1,724 B (53.9 B/MiB) | `lzma_stream_buffer_bound`: n + 3 B per 64 KiB chunk + container headers | yes: liblzma block-split threads, with a small loss of ratio |
| brotli, window 2^24 | 16 MiB window (132/44 MiB) | +53 B | `BrotliEncoderMaxCompressedSize`: n + 4 B per 16 KiB + 6 | no, not in the reference library |

Every one of these bounds is below the fetch cap's `limit + limit/64 + 1 MiB`. The cap's `limit/64` term is zstd's allowance for a hostile stream (skippable frames, empty blocks; `archive.rs`, `sent_limit`). For any other format, the same hostile-stream case would need working out again.

## Recommendation

**Send each distinct file once and copy the duplicates into place on the PC, and compress the stream with zstd level 3 with long-distance matching (window 2^27).**

- **Against today.** zstd 3 on the full set takes 707 s at the estimated 9.93 MiB/s, 2,313 s at 2.7 MiB/s and 167 s at 100 MiB/s. The recommendation takes 252 s, 714 s and 96 s: 64%, 69% and 43% less.
- **Dedup is most of the gain.** It cuts the bytes compressed from 14.39 GiB to 3.77 GiB, and no algorithm comes close to that. On the full set, the best measured candidate at 2.7 MiB/s (zstd 9 `-long`, 1,954 s) is still 2.5 times slower than plain zstd 3 on the dedup set (783 s).
- **Among the deduped candidates.** zstd 3 `-long` is fastest, or within 1 s of it, at 9.93 and 2.7 MiB/s (252 s, 714 s). At 100 MiB/s it is 12 s behind lz4 (84 s) and 8 s behind zstd 1 `-long` (88 s). zstd 1 `-long` is the close alternative (252, 739, 88 s).
- **The slow candidates lose at every rate.** zstd 19, xz and brotli at high levels spend 1,400 s or more compressing even the dedup set on one thread. Their output, at most 26% smaller than zstd 3 `-long`'s, saves at most 165 s at 2.7 MiB/s.
- **Without dedup**, `-long` alone is still worth taking: zstd 3 `-long` over zstd 3 saves 86 s at 9.93 MiB/s and 315 s at 2.7 MiB/s.

**What changes, and its worst case:**

- **Reader window.** The reader's `WINDOW_LOG_MAX` (23, 8 MiB) must rise to 27. The PC then holds a 128 MiB window per archive being read: 151 MiB peak working set measured, against 25 MiB today. The box's compressor peaked at 163 MiB, against 29 MiB today.
- **Fetch cap.** The fetch cap formula is unchanged: same format, measured expansion 24.3 B/MiB on random input.
- **What the limit counts.** With dedup, the bytes the PC writes are the distinct bytes plus the copies. The extraction limit and the list must count the copies, so a small archive cannot expand past the limit through its list.
- **Dedup's added work** is one SHA-256 pass on the box (34.9 s here, one thread) and the copies on the PC (8.6 s here). Both are included in the totals above.

Multithreaded zstd compression would cut the 24.8 s compress further and was not measured.

## Limits of this measurement

- The download rate is an estimate. It assumes the trial's roughly 10-minute fetch carried a zstd 3 archive of this set's size, and the trial logged no compressed size. The ranking holds at all three rates scored.
- Compress and decompress both ran on this PC. The builder's CPU, its disk, and multithreaded compression were not measured.
- The slow candidates on the full and dedup sets are extrapolated from a sample whose duplicate share (15%) is far below the full set's (74%), and their times carry the contention of five runs at once.
- This is the full-outputs case (a first fetch, or `OutputsFull`). A routed run otherwise packs only the files the PC's manifest lacks or holds with another SHA-256 (`outputs/manifest.rs`, `select`), which may be far fewer.
