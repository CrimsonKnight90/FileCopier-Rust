//! Benchmarks para el motor de copia de archivos.
//!
//! Ejecutar con: cargo bench --bench engine

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use tempfile::TempDir;

// Importar componentes del crate (ajustar según la estructura real)
// use lib_core::engine::{BlockEngine, SwarmEngine, Orchestrator};
// use lib_core::config::EngineConfig;
// use lib_core::buffer_pool::BufferPool;

/// Benchmark de throughput para operaciones de copia secuencial.
fn benchmark_sequential_copy(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let src_path = temp_dir.path().join("source.bin");
    let dst_path = temp_dir.path().join("dest.bin");

    // Crear archivo de prueba de 10 MB
    let file_size = 10 * 1024 * 1024; // 10 MB
    {
        let mut file = File::create(&src_path).unwrap();
        let data = vec![0u8; file_size];
        file.write_all(&data).unwrap();
    }

    let mut group = c.benchmark_group("sequential_copy");
    group.throughput(Throughput::Bytes(file_size as u64));

    group.bench_function("copy_10mb", |b| {
        b.iter(|| {
            let mut src = File::open(&src_path).unwrap();
            let mut dst = File::create(&dst_path).unwrap();
            
            let mut buffer = vec![0u8; 8192]; // 8 KB buffer
            loop {
                let bytes_read = src.read(&mut buffer).unwrap();
                if bytes_read == 0 {
                    break;
                }
                dst.write_all(&buffer[..bytes_read]).unwrap();
            }
            
            black_box(());
        });
    });

    group.finish();
}

/// Benchmark del buffer pool vs asignación tradicional.
fn benchmark_buffer_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_allocation");

    group.bench_function("traditional_alloc", |b| {
        b.iter(|| {
            let _buffer: Vec<u8> = vec![0u8; 8192];
            black_box(());
        });
    });

    // Nota: Descomentar cuando se integre el BufferPool en el pipeline
    // group.bench_function("buffer_pool_reuse", |b| {
    //     let pool = BufferPool::new(8192, 10);
    //     b.iter(|| {
    //         let buffer = pool.acquire();
    //         black_box(buffer);
    //         // pool.release(buffer); // En benchmark real, devolver al pool
    //     });
    // });

    group.finish();
}

/// Benchmark de hashing con diferentes algoritmos.
fn benchmark_hashing(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test_data.bin");

    // Crear archivo de prueba de 5 MB
    let file_size = 5 * 1024 * 1024;
    {
        let mut file = File::create(&file_path).unwrap();
        let data = vec![0x42u8; file_size];
        file.write_all(&data).unwrap();
    }

    let mut group = c.benchmark_group("hashing");
    group.throughput(Throughput::Bytes(file_size as u64));

    group.bench_function("blake3_5mb", |b| {
        b.iter(|| {
            let mut file = File::open(&file_path).unwrap();
            let mut buffer = vec![0u8; 8192];
            let mut hasher = blake3::Hasher::new();
            
            loop {
                let bytes_read = file.read(&mut buffer).unwrap();
                if bytes_read == 0 {
                    break;
                }
                hasher.update(&buffer[..bytes_read]);
            }
            
            black_box(hasher.finalize());
        });
    });

    group.bench_function("xxhash_5mb", |b| {
        b.iter(|| {
            let mut file = File::open(&file_path).unwrap();
            let mut buffer = vec![0u8; 8192];
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            
            loop {
                let bytes_read = file.read(&mut buffer).unwrap();
                if bytes_read == 0 {
                    break;
                }
                hasher.update(&buffer[..bytes_read]);
            }
            
            black_box(hasher.digest64());
        });
    });

    group.finish();
}

/// Benchmark de pre-asignación de archivos.
fn benchmark_preallocation(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let file_size = 100 * 1024 * 1024; // 100 MB

    let mut group = c.benchmark_group("preallocation");

    group.bench_function("without_prealloc", |b| {
        b.iter(|| {
            let path = temp_dir.path().join("no_prealloc.bin");
            let mut file = File::create(&path).unwrap();
            let chunk_size = 1024 * 1024; // 1 MB chunks
            let data = vec![0u8; chunk_size];
            
            for _ in 0..(file_size / chunk_size) {
                file.write_all(&data).unwrap();
            }
            
            black_box(());
        });
    });

    group.bench_function("with_prealloc", |b| {
        b.iter(|| {
            let path = temp_dir.path().join("prealloc.bin");
            let mut file = File::create(&path).unwrap();
            
            // Pre-asignar espacio
            file.set_len(file_size as u64).unwrap();
            
            // Escribir datos
            let chunk_size = 1024 * 1024; // 1 MB chunks
            let data = vec![0u8; chunk_size];
            
            for _ in 0..(file_size / chunk_size) {
                file.write_all(&data).unwrap();
            }
            
            black_box(());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_sequential_copy,
    benchmark_buffer_pool,
    benchmark_hashing,
    benchmark_preallocation,
);

criterion_main!(benches);
