//! # swarm
//!
//! Motor de enjambre para archivos pequeños (< umbral de triage).
//!
//! ## Estrategia
//!
//! Lanza hasta `config.swarm_concurrency` tareas tokio en paralelo.
//! El `Semaphore` limita las tareas activas para no saturar:
//! - File descriptors del OS.
//! - Cabezal del HDD si el destino es disco mecánico.
//!
//! ## Pausa/Reanudar
//!
//! Cuando `FlowControl` está pausado, las tareas en vuelo terminan su
//! archivo actual (no se interrumpen a mitad de escritura) y luego
//! esperan en `wait_for_resume()`. Las tareas que aún no han comenzado
//! también esperan antes de adquirir el semáforo.
//!
//! Esto es simétrico con el comportamiento de `BlockEngine`, que pausa
//! entre bloques y espera con `wait_for_resume()`.
//!
//! ## Hashing en el enjambre
//!
//! Para archivos pequeños, el hash se calcula sobre el contenido
//! completo en memoria (single-pass). No hay pipeline reader/writer:
//! todo ocurre en la misma tarea, más eficiente para archivos < 16 MB.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::checkpoint::FlowControl;
use crate::config::EngineConfig;
use crate::error::{CoreError, Result};
use crate::hash::HasherDispatch;
use crate::os_ops::OsOps;
use crate::telemetry::TelemetryHandle;

use super::orchestrator::FileEntry;

/// Resultado de copiar un archivo en el enjambre.
/// `Ok(Some(hash))` si verify, `Ok(None)` si no, `Err(...)` si falló.
type SwarmFileResult = (std::path::PathBuf, std::result::Result<Option<String>, CoreError>);

/// Motor de enjambre para archivos pequeños.
pub struct SwarmEngine {
    config:    Arc<EngineConfig>,
    flow:      FlowControl,
    telemetry: TelemetryHandle,
    os_ops:    Arc<dyn OsOps>,
    throttle:  Option<crate::bandwidth::ThrottleHandle>,
}

impl SwarmEngine {
    pub fn new(
        config:    Arc<EngineConfig>,
        flow:      FlowControl,
        telemetry: TelemetryHandle,
        os_ops:    Arc<dyn OsOps>,
    ) -> Self {
        // Crear throttle si está configurado límite de ancho de banda
        let throttle = if config.bandwidth_limit_bytes_per_sec > 0 {
            Some(crate::bandwidth::ThrottleHandle::new(
                config.bandwidth_limit_bytes_per_sec,
                config.bandwidth_burst_bytes,
            ))
        } else {
            None
        };
        
        Self { config, flow, telemetry, os_ops, throttle }
    }

    /// Copia todos los archivos de `files` en paralelo.
    ///
    /// Retorna un `Vec` con el resultado de cada archivo (incluyendo fallos).
    /// El enjambre **nunca aborta** por el fallo de un archivo individual.
    pub async fn run(
        &self,
        files:            Vec<FileEntry>,
        _checkpoint_path: &Path,
    ) -> Result<Vec<SwarmFileResult>> {
        if files.is_empty() {
            return Ok(vec![]);
        }

        let semaphore = Arc::new(Semaphore::new(self.config.swarm_concurrency));
        let mut handles = Vec::with_capacity(files.len());

        for entry in files {
            if self.flow.is_cancelled() {
                tracing::debug!("Enjambre: cancelación detectada, deteniendo despacho de tareas");
                break;
            }

            if self.flow.is_paused() {
                let flow_wait = self.flow.clone();
                let wait_result = tokio::task::spawn_blocking(move || {
                    flow_wait.wait_for_resume()
                })
                .await
                .map_err(|_| CoreError::PipelineDisconnected)?;

                if let Err(CoreError::PipelineDisconnected) = wait_result {
                    tracing::info!("Enjambre: cancelado durante espera de pausa");
                    break;
                }
            }

            let sem       = Arc::clone(&semaphore);
            let config    = Arc::clone(&self.config);
            let telemetry = self.telemetry.clone();
            let flow      = self.flow.clone();
            let os_ops    = Arc::clone(&self.os_ops);   // ← NUEVO

            let handle = tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .expect("Semáforo cerrado inesperadamente");

                if flow.is_cancelled() {
                    return (entry.relative, Err(CoreError::PipelineDisconnected));
                }

                if flow.is_paused() {
                    let flow_inner = flow.clone();
                    let resume = tokio::task::spawn_blocking(move || {
                        flow_inner.wait_for_resume()
                    })
                    .await;

                    match resume {
                        Ok(Ok(())) => {}
                        Ok(Err(CoreError::PipelineDisconnected)) | Err(_) => {
                            return (entry.relative, Err(CoreError::PipelineDisconnected));
                        }
                        Ok(Err(e)) => {
                            return (entry.relative, Err(e));
                        }
                    }
                }

                if flow.is_cancelled() {
                    return (entry.relative, Err(CoreError::PipelineDisconnected));
                }

                let result = copy_small_file(
                    &entry,
                    &config,
                    &telemetry,
                    os_ops.as_ref(),
                    self.throttle.as_ref(),
                ).await;
                (entry.relative, result)
            });

            handles.push(handle);
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) if e.is_cancelled() => {
                    tracing::warn!("Tarea del enjambre cancelada por el runtime de tokio");
                }
                Err(e) => {
                    tracing::error!("Tarea del enjambre entró en pánico: {e}");
                }
            }
        }

        Ok(results)
    }
}

/// Copia un archivo pequeño de forma async (single-pass: leer todo → escribir).
async fn copy_small_file(
    entry:     &FileEntry,
    config:    &EngineConfig,
    telemetry: &TelemetryHandle,
    os_ops:    &dyn OsOps,
    throttle:  Option<&crate::bandwidth::ThrottleHandle>,
) -> std::result::Result<Option<String>, CoreError> {
    // Aplicar throttling a la lectura
    if let Some(th) = throttle {
        th.consume(entry.size);
    }
    
    let data = tokio::fs::read(&entry.source)
        .await
        .map_err(|e| CoreError::read(&entry.source, e))?;

    let size = data.len() as u64;

    let hash = if config.verify {
        let mut hasher = HasherDispatch::new(config.hash_algorithm);
        hasher.update(&data);
        Some(hasher.finalize())
    } else {
        None
    };

    if let Some(parent) = entry.dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| CoreError::io(parent, e))?;
    }

    let partial_dest = if config.use_partial_files {
        let file_name = entry.dest.file_name().unwrap();
        let partial_name = format!("{}.partial", file_name.to_string_lossy());
        entry.dest.parent().unwrap().join(partial_name)
    } else {
        entry.dest.clone()
    };

    // ← NUEVO: preallocación
    if size > 0 {
        if let Err(e) = os_ops.preallocate(&partial_dest, size) {
            tracing::warn!(
                "Preallocación fallida en enjambre para '{}': {}",
                partial_dest.display(),
                e
            );
        }
    }

    // Aplicar throttling a la escritura
    if let Some(th) = throttle {
        th.consume(size);
    }
    
    tokio::fs::write(&partial_dest, &data)
        .await
        .map_err(|e| CoreError::write(&partial_dest, e))?;

    if config.use_partial_files {
        tokio::fs::rename(&partial_dest, &entry.dest)
            .await
            .map_err(|e| CoreError::rename(&partial_dest, &entry.dest, e))?;
    }

    // ← NUEVO: copia de metadatos
    if let Err(e) = os_ops.copy_metadata(&entry.source, &entry.dest) {
        tracing::warn!(
            "copy_metadata fallida en enjambre para '{}': {}",
            entry.dest.display(),
            e
        );
    }

    telemetry.add_bytes(size);
    telemetry.complete_file();

    tracing::trace!(
        "Enjambre: copiado {} ({} bytes)",
        entry.source.display(),
        size
    );

    Ok(hash)
}
