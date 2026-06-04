use std::hint::black_box;
use std::io::{Cursor, Read, Write};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main, measurement::WallTime};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use futures::StreamExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use tokio_util::compat::{FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};
use uv_distribution_filename::SourceDistExtension;

const MANY_FILES_SDIST_FILENAME: &str = "manyfiles-0.0.0.tar.gz";
const MANY_FILES_SDIST_TOP_LEVEL: &str = "manyfiles-0.0.0";
const MANY_FILES_SDIST_FILE_COUNT: usize = 10_000;

/// Number of files used by the pure file-system I/O benchmarks.
const FS_IO_FILE_COUNT: usize = 10_000;
/// Size, in bytes, of each file written by the pure file-system I/O benchmarks.
const FS_IO_FILE_SIZE: usize = 4 * 1024;

fn create_many_files_sdist() -> tempfile::NamedTempFile {
    let archive = tempfile::NamedTempFile::new().expect("Failed to create temporary archive");
    let encoder = GzEncoder::new(archive.as_file(), flate2::Compression::default());
    let mut writer =
        tokio_tar::Builder::new_non_terminated(AllowStdIo::new(encoder).compat_write());
    for index in 0..MANY_FILES_SDIST_FILE_COUNT {
        write_tar_entry(
            &mut writer,
            &format!("{MANY_FILES_SDIST_TOP_LEVEL}/manyfiles/{index}.txt"),
            b"",
        );
    }
    write_tar_entry(
        &mut writer,
        &format!("{MANY_FILES_SDIST_TOP_LEVEL}/PKG-INFO"),
        b"Metadata-Version: 2.1\nName: manyfiles\nVersion: 0.0.0\n",
    );
    write_tar_entry(
        &mut writer,
        &format!("{MANY_FILES_SDIST_TOP_LEVEL}/pyproject.toml"),
        b"[project]\nname = \"manyfiles\"\nversion = \"0.0.0\"\n",
    );
    let writer = block_on(writer.into_inner()).expect("Failed to finish tar archive");
    writer
        .into_inner()
        .into_inner()
        .finish()
        .expect("Failed to finish gzip archive");
    archive
}

fn unpack_sdist_many_files(c: &mut Criterion<WallTime>) {
    let archive = create_many_files_sdist();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    c.bench_function("unpack_sdist_many_files", |b| {
        b.iter_batched(
            || {
                (
                    runtime
                        .block_on(fs_err::tokio::File::open(archive.path()))
                        .expect("Failed to open temporary archive"),
                    tempfile::tempdir().expect("Failed to create sdist extraction directory"),
                )
            },
            |(archive, extracted_sdist)| {
                let files = runtime
                    .block_on(uv_extract::stream::archive(
                        MANY_FILES_SDIST_FILENAME,
                        archive,
                        SourceDistExtension::TarGz,
                        extracted_sdist.path(),
                    ))
                    .expect("Failed to unpack sdist");
                let source_tree = uv_extract::strip_component(extracted_sdist.path())
                    .expect("Failed to strip top-level sdist directory");
                black_box((files, extracted_sdist, source_tree))
            },
            BatchSize::SmallInput,
        );
    });
}

/// Write many small files to a fresh directory using only the standard library,
/// to observe the variance of pure file-system writes.
fn fs_write_many_files(c: &mut Criterion<WallTime>) {
    let contents = vec![b'a'; FS_IO_FILE_SIZE];

    c.bench_function("fs_write_many_files", |b| {
        b.iter_batched(
            || tempfile::tempdir().expect("Failed to create write directory"),
            |dir| {
                for index in 0..FS_IO_FILE_COUNT {
                    let path = dir.path().join(format!("{index}.txt"));
                    let mut file = std::fs::File::create(&path).expect("Failed to create file");
                    file.write_all(&contents).expect("Failed to write file");
                }
                black_box(dir)
            },
            BatchSize::SmallInput,
        );
    });
}

/// Read many small files from a directory using only the standard library, to
/// observe the variance of pure file-system reads.
fn fs_read_many_files(c: &mut Criterion<WallTime>) {
    let contents = vec![b'a'; FS_IO_FILE_SIZE];
    let dir = tempfile::tempdir().expect("Failed to create read directory");
    for index in 0..FS_IO_FILE_COUNT {
        let path = dir.path().join(format!("{index}.txt"));
        std::fs::write(&path, &contents).expect("Failed to seed file");
    }

    c.bench_function("fs_read_many_files", |b| {
        b.iter_batched(
            || vec![0u8; FS_IO_FILE_SIZE],
            |mut buffer| {
                for index in 0..FS_IO_FILE_COUNT {
                    let path = dir.path().join(format!("{index}.txt"));
                    let mut file = std::fs::File::open(&path).expect("Failed to open file");
                    file.read_exact(&mut buffer).expect("Failed to read file");
                    black_box(&buffer);
                }
                black_box(buffer)
            },
            BatchSize::SmallInput,
        );
    });
}

/// Read the many-files sdist archive fully into memory, so the compression
/// benchmarks below can exercise the codec without touching the file system.
fn read_many_files_sdist_bytes() -> Vec<u8> {
    let archive = create_many_files_sdist();
    std::fs::read(archive.path()).expect("Failed to read temporary archive")
}

/// Gzip-decompress the sdist archive entirely in memory, discarding the output.
///
/// Isolates the pure decompression CPU cost of `unpack_sdist_many_files` (same
/// `zlib-rs` codec) with no file-system reads, writes, or tar parsing, to see
/// how much of that benchmark's run-to-run range comes from decompression.
fn gz_decompress_archive(c: &mut Criterion<WallTime>) {
    let compressed = read_many_files_sdist_bytes();

    c.bench_function("gz_decompress_archive", |b| {
        b.iter_batched(
            || vec![0u8; 64 * 1024],
            |mut buffer| {
                let mut decoder = GzDecoder::new(Cursor::new(black_box(&compressed)));
                let mut total = 0usize;
                loop {
                    let read = decoder.read(&mut buffer).expect("Failed to decompress");
                    if read == 0 {
                        break;
                    }
                    total += read;
                }
                black_box(total)
            },
            BatchSize::SmallInput,
        );
    });
}

/// Gzip-compress the decompressed archive contents entirely in memory.
///
/// The inverse of `gz_decompress_archive`: isolates compression CPU cost with no
/// file-system I/O, as a cross-check on whether the compression codec (rather
/// than the file system) is the source of run-to-run range.
fn gz_compress_archive(c: &mut Criterion<WallTime>) {
    let compressed = read_many_files_sdist_bytes();
    let mut decompressed = Vec::new();
    GzDecoder::new(Cursor::new(&compressed))
        .read_to_end(&mut decompressed)
        .expect("Failed to decompress");

    c.bench_function("gz_compress_archive", |b| {
        b.iter(|| {
            let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder
                .write_all(black_box(&decompressed))
                .expect("Failed to compress");
            black_box(encoder.finish().expect("Failed to finish gzip"))
        });
    });
}

/// Decompress and tar-parse the sdist archive without writing any files to disk.
///
/// Decompresses with the same `zlib-rs` codec, then drains every tar entry
/// (reading its body) but never unpacks to the file system. The gap between
/// this and `unpack_sdist_many_files` isolates the file-system metadata-write
/// cost (~10,002 inode creations) from the decompression + tar-parsing CPU cost.
fn untar_archive_to_memory(c: &mut Criterion<WallTime>) {
    let compressed = read_many_files_sdist_bytes();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    c.bench_function("untar_archive_to_memory", |b| {
        b.iter(|| {
            // Decompress with the same `zlib-rs` codec, into memory.
            let mut decompressed = Vec::new();
            GzDecoder::new(Cursor::new(black_box(&compressed)))
                .read_to_end(&mut decompressed)
                .expect("Failed to decompress");

            runtime.block_on(async {
                let reader = AllowStdIo::new(Cursor::new(&decompressed)).compat();
                let mut archive = tokio_tar::Archive::new(reader);
                let mut entries = archive.entries().expect("Failed to read entries");
                let mut pinned = std::pin::Pin::new(&mut entries);
                let mut count = 0usize;
                let mut sink = Vec::new();
                while let Some(entry) = pinned.next().await {
                    let mut entry = entry.expect("Failed to read entry");
                    sink.clear();
                    tokio::io::AsyncReadExt::read_to_end(&mut entry, &mut sink)
                        .await
                        .expect("Failed to drain entry");
                    count += 1;
                }
                black_box(count)
            })
        });
    });
}

fn write_tar_entry<W: tokio::io::AsyncWrite + Unpin + Send>(
    writer: &mut tokio_tar::Builder<W>,
    path: &str,
    contents: &[u8],
) {
    let mut header = tokio_tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tokio_tar::EntryType::Regular);
    header.set_cksum();
    block_on(writer.append_data(
        &mut header,
        path,
        AllowStdIo::new(Cursor::new(contents)).compat(),
    ))
    .expect("Failed to write tar entry");
}

criterion_group!(
    uv,
    unpack_sdist_many_files,
    fs_write_many_files,
    fs_read_many_files,
    gz_decompress_archive,
    gz_compress_archive,
    untar_archive_to_memory
);
criterion_main!(uv);
