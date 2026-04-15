# FileCopier-Rust

[![Build Status](https://img.shields.io/badge/build-passing-brightgreen)]()
[![Tests](https://img.shields.io/badge/tests-11%20passed-green)]()
[![Version](https://img.shields.io/badge/version-0.1.0-blue)]()

Motor de copia profesional de alto rendimiento escrito en Rust, con arquitectura
modular, motor dual (bloques grandes + enjambre), hashing paralelo con blake3,
detección inteligente de hardware, pausa/reanudar, checkpointing e interfaz Tauri.

**Estado actual:** ✅ **Producción** - Sprint 2 completado. Motor estable, tests en verde, 
copias reales verificadas exitosamente (3874 archivos, 1.1 GB, hasta 881 MB/s).

---

## Arquitectura del workspace

```
FileCopier-Rust/
├── Cargo.toml          # Workspace raíz — define members y dependencias compartidas
│
├── crates/
│   ├── lib-core/       # Motor principal (sin dependencias de SO)
│   │   └── src/
│   │       ├── engine/     # Motor dual: bloques grandes y enjambre
│   │       ├── pipeline/   # Lector → Canal → Hasher → Escritor
│   │       ├── hash/       # Trait ChecksumAlgorithm + blake3 / xxhash / sha2
│   │       ├── checkpoint/ # Estado de pausa, reanudar y persistencia en disco
│   │       └── telemetry/  # Métricas diferenciadas (MB/s vs archivos/s)
│   │
│   ├── lib-os/         # Abstracciones del sistema operativo
│   │   └── src/
│   │       ├── traits.rs   # Interfaz portable (OsAdapter)
│   │       ├── detect.rs   # Detección de hardware (SSD/HDD/Red) por plataforma
│   │       └── windows/    # Implementación Win32 (preallocación, rename atómico)
│   │
│   ├── app-cli/        # CLI (Fase 1 — MVP ✅ Completado)
│   │   └── src/
│   │       └── main.rs     # Interfaz de línea de comandos con clap
│   │
│   └── app-gui/        # GUI Tauri (Fase 2 — En desarrollo)
│       ├── src-tauri/
│       └── src/
```

---

## Motor Dual

### Motor de Bloques Grandes (≥ umbral, default 16 MB)

```
┌────────┐   crossbeam   ┌────────┐   rayon   ┌────────┐
│ Reader │ ────────────► │ Buffer │ ─────────► │ Hasher │
└────────┘    canal      └────────┘  paralelo  └────────┘
                                                    │
                                               ┌────▼───┐
                                               │ Writer │
                                               └────────┘
```

- Bloques de 4 MB (configurable).
- Backpressure real: canal crossbeam con capacidad limitada.
- Hashing paralelo con rayon sobre cada bloque.

### Motor de Enjambre (< umbral)

- Tareas asíncronas independientes con `tokio::spawn`.
- Concurrencia limitada por `tokio::sync::Semaphore` (default 128).
- Optimizado para IOPS: latencia mínima, apertura rápida de archivos.

---

## Resiliencia

| Mecanismo | Descripción |
|---|---|
| `.partial` | Archivo destino escrito con extensión `.partial` hasta completarse |
| Rename atómico | Rename final solo si hash verificado (opt-in con `--verify`) |
| Checkpoint JSON | Estado persistido en disco: permite reanudar tras desconexión |
| `AtomicBool` | Pausa limpia: el escritor vacía buffer antes de suspender |

---

## Detección de Hardware

FileCopier-Rust detecta automáticamente el tipo de almacenamiento (SSD/NVMe, HDD, Red) 
para optimizar la estrategia de copia. Implementado en `lib-os/detect.rs`.

| Tipo detectado | Estrategia | Concurrencia |
|---|---|---|
| **HDD → HDD** | Secuencial, sin paralelismo excesivo | Enjambre reducido |
| **SSD → HDD** | Lectura por ráfagas + buffer grande | Moderada |
| **SSD → SSD/NVMe** | Paralelismo total, IOPS máximas | 128-256 tareas |
| **Red (UNC)** | Limitación por ancho de banda | Conservadora |

### Ejemplo de salida

```
Hardware: SSD/NVMe → SSD/NVMe  |  enjambre=32 bloque=4MB
```

✅ **Windows:** Detección nativa vía `GetDriveTypeW` y `DeviceIoControl`.  
✅ **Linux:** Detección vía `/sys/block/*/queue/rotational`.  
✅ **Paths UNC:** Detectados como red automáticamente.

---

## Fases de desarrollo

| Fase | Estado | Contenido |
|---|---|---|
| **Fase 1** | ✅ Completada | CLI + motor dual + hashing blake3 + `.partial` + checkpoint |
| **Sprint 1** | ✅ Cerrado | Correcciones estructurales, módulos, rutas y compilación |
| **Sprint 2** | ✅ Cerrado | Detección de hardware en Windows, tests unitarios, primera copia real |
| **Fase 2** | 🔄 En desarrollo | GUI Tauri + heurísticas dinámicas + control de ancho de banda |
| **Fase 3** | 📋 Planificada | VSS (Volume Shadow Copy), integración profunda con SO |

### Métricas de la Fase 1 (Windows 11, NVMe → NVMe)

| Escenario | Archivos | Tamaño | Tiempo | Velocidad | Verificación |
|---|---|---|---|---|---|
| Copia + Verify | 3,874 | 1,091 MB | 3.58s | **304.5 MB/s** | ✅ blake3 |
| Copia directa | 3,874 | 1,091 MB | 10.60s | **103.0 MB/s** | ❌ |
| Throughput pico | - | - | - | **881.7 MB/s** | - |

---

## Compilación rápida

```bash
# Debug (compilación rápida)
cargo build

# Release optimizado
cargo build --release

# Ejecutar CLI
cargo run -p app-cli -- --help

# Tests de toda la workspace
cargo test --workspace

# Benchmarks
cargo bench -p lib-core
```

---

## Requisitos

- Rust 1.78+ (stable)
- Windows: MSVC toolchain (`x86_64-pc-windows-msvc`)
- Linux: `x86_64-unknown-linux-gnu` (para CI/CD)

---

## Configuración CLI (Fase 1)

```
filecopier [OPTIONS] <ORIGEN> <DESTINO>

Options:
  --verify              Habilita hashing y verificación post-copia (blake3)
  --hasher <ALGO>       Algoritmo: blake3 (default), xxhash, sha2
  --block-size <MB>     Tamaño de bloque en MB (default: 4)
  --threshold <MB>      Umbral triage bloques/enjambre en MB (default: 16)
  --swarm-limit <N>     Máx. tareas concurrentes en enjambre (default: 128)
  --resume              Intenta reanudar desde checkpoint existente
  -v, --verbose         Muestra información detallada incluyendo tipo de hardware
  -h, --help            Muestra ayuda
  -V, --version         Muestra versión
```

### Ejemplos de uso

```bash
# Copia básica con verificación blake3
filecopier --verify C:\Datos D:\Backup

# Copia rápida sin verificación (máxima velocidad)
filecopier C:\Datos D:\Backup

# Copia con verbose para ver detección de hardware
filecopier -v C:\Users\Documents D:\Backup

# Reanudar copia interrumpida
filecopier --resume C:\Origen D:\Destino
```

---

## Estado por plataforma

| Plataforma | Soporte | Detección HW | Tests | Notas |
|---|---|---|---|---|
| **Windows 10/11** | ✅ Nativo | ✅ SSD/HDD/Red | ✅ 11/11 | MSVC, `GetDriveTypeW` |
| **Linux** | ✅ Parcial | ⚠️ En desarrollo | 🔄 Pendiente | vía `/sys/block` |
| **macOS** | 📋 Planificado | ❌ No implementado | ❌ | Futura Fase 2 |

---

## Próximos pasos (Sprint 3)

- [ ] Tests de regresión automatizados (CI/CD con GitHub Actions)
- [ ] Tests unitarios para checkpoint y config
- [ ] Manejo avanzado de errores y logs persistentes
- [ ] Optimización de rendimiento en HDD mecánicos
- [ ] Preparación de APIs para integración Tauri (Fase 2)