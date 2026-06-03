use std::hint::black_box;
use std::io::{Cursor, Read, Write};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main, measurement::WallTime};
use flate2::write::GzEncoder;
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
    fs_read_many_files
);
criterion_main!(uv);
