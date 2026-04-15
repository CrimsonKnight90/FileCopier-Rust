# Benchmarks

Este directorio contiene benchmarks de rendimiento para el motor de FileCopier-Rust.

## Ejecutar benchmarks

```bash
# Ejecutar todos los benchmarks
cargo bench --bench engine_bench

# Ejecutar benchmarks específicos por patrón
cargo bench --bench engine_bench -- hashing
cargo bench --bench engine_bench -- preallocation
cargo bench --bench engine_bench -- sequential_copy

# Generar reporte en formato JSON
cargo bench --bench engine_bench -- --output-format json > results.json
```

## Benchmarks disponibles

### `sequential_copy`
Mide el throughput de copia secuencial de archivos usando buffers tradicionales.
- **Tamaño**: 10 MB
- **Buffer**: 8 KB
- **Métrica**: MB/s

### `buffer_allocation`
Compara la asignación tradicional de buffers vs reutilización con BufferPool.
- **traditional_alloc**: Asignación de Vec<u8> en cada iteración
- **buffer_pool_reuse**: Reutilización de buffers del pool (zero-alloc)

### `hashing`
Benchmark de algoritmos de hash sobre archivos grandes.
- **blake3_5mb**: Hash BLAKE3 sobre 5 MB de datos
- **xxhash_5mb**: Hash XXH3 sobre 5 MB de datos
- **Métrica**: MB/s

### `preallocation`
Compara el rendimiento de escritura con y sin pre-asignación de espacio.
- **without_prealloc**: Escritura incremental sin reservar espacio
- **with_prealloc**: Pre-asignación con `set_len()` antes de escribir
- **Tamaño**: 100 MB

## Interpretación de resultados

Los benchmarks usan Criterion.rs que proporciona:
- **Tiempo medio**: Tiempo promedio por operación
- **Throughput**: Datos procesados por segundo (MB/s)
- **Cambios**: Comparación con ejecuciones anteriores (si existen)

### Optimizaciones clave medidas

1. **Buffer Pool**: Reduce presión sobre el allocator en el hot path
2. **Pre-alocación**: Evita fragmentación en NTFS/ext4 y reduce syscalls
3. **Hashers**: BLAKE3 es óptimo para throughput, XXH3 para latencia baja

## Generar flamegraphs

Para profiling avanzado:

```bash
# Instalar cargo-flamegraph
cargo install flamegraph

# Ejecutar benchmark con profiling
cargo flamegraph --bench engine_bench
```

## Notas

- Los benchmarks se ejecutan con optimizaciones de release (`opt-level = 3`, LTO)
- Se incluye información de debug para permitir generación de flamegraphs
- Cada benchmark se ejecuta múltiples veces para obtener estadísticas confiables
