//! # buffer_pool
//!
//! Pool de buffers con devolución automática vía RAII.
//!
//! ## Diseño
//!
//! `PooledBuffer` es la unidad de trabajo del pipeline. Al hacer `drop()`,
//! devuelve automáticamente su memoria al pool sin ninguna llamada explícita.
//! Esto elimina el bug anterior donde `into_vec()` sacaba el buffer del pool
//! sin devolverlo, causando agotamiento y deadlock en archivos grandes.
//!
//! ## Flujo de vida
//!
//! ```text
//! BufferPool::new(block_size, pool_size)
//!      │
//!      ├─ acquire() → PooledBuffer (con Arc<Pool> interno)
//!      │                   │
//!      │              [reader escribe datos]
//!      │                   │
//!      │              se envía por canal crossbeam
//!      │                   │
//!      │              [writer lee datos, escribe a disco]
//!      │                   │
//!      │              drop(PooledBuffer) → devuelve Vec al pool automáticamente
//!      │
//!      └─ acquire() → siguiente PooledBuffer (reutiliza memoria)
//! ```
//!
//! ## Zero-allocation en el hot path
//!
//! Una vez calentado el pool, no hay allocaciones en el loop de copia.
//! El Vec pre-asignado se reutiliza indefinidamente entre reader y writer.

use std::sync::Arc;

use crossbeam::channel::{bounded, Receiver, Sender, TrySendError};

// ─────────────────────────────────────────────────────────────────────────────
// Pool interno (compartido por todos los PooledBuffer vivos)
// ─────────────────────────────────────────────────────────────────────────────

struct Pool {
    /// Canal desde el que se devuelven los Vecs al pool.
    /// La capacidad del canal == pool_size para evitar bloqueos en release.
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    buffer_size: usize,
}

impl Pool {
    fn new(buffer_size: usize, pool_size: usize) -> Arc<Self> {
        let (tx, rx) = bounded(pool_size);
        for _ in 0..pool_size {
            // Pre-asignar cada buffer con la capacidad exacta del bloque.
            // `set_len` no se usa aquí — se hace en `acquire()` después de leer.
            let mut v = Vec::with_capacity(buffer_size);
            // Safety: los bytes son sobrescritos por read() antes de usarse.
            // Usar `set_len` al buffer_size lleno para que read() tenga espacio.
            unsafe { v.set_len(buffer_size) };
            tx.send(v).expect("canal recién creado no puede estar lleno");
        }
        Arc::new(Self { tx, rx, buffer_size })
    }

    /// Toma un Vec del pool. Bloquea si el pool está vacío (backpressure correcto).
    fn acquire(self: &Arc<Self>) -> PooledBuffer {
        let data = self.rx.recv().expect("pool cerrado inesperadamente");
        PooledBuffer {
            data: Some(data),
            pool: Arc::clone(self),
        }
    }

    /// Devuelve un Vec al pool. Llamado automáticamente por `PooledBuffer::drop`.
    fn release(&self, mut data: Vec<u8>) {
        // Restaurar la longitud al tamaño completo para la próxima lectura.
        // La capacidad no cambia — es la misma memoria.
        let cap = data.capacity();
        unsafe { data.set_len(cap) };

        // `try_send` nunca bloquea. Si el canal está lleno (situación anómala),
        // descartamos el Vec — el GC lo recogerá. Esto no puede ocurrir en uso
        // normal porque pool_size == capacidad del canal.
        match self.tx.try_send(data) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("BufferPool: pool lleno al devolver buffer — descartado");
            }
            Err(TrySendError::Disconnected(_)) => {
                // Pool destruido. Normal durante el shutdown.
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PooledBuffer — el tipo público que circula por el pipeline
// ─────────────────────────────────────────────────────────────────────────────

/// Buffer RAII que pertenece al pool y se devuelve automáticamente al hacer drop.
///
/// Se comporta como un `&mut [u8]` de tamaño dinámico:
/// - El reader escribe en él con `as_write_slice()` y luego llama `set_filled(n)`.
/// - El writer lee con `as_slice()`.
/// - Al salir de scope (o al hacer drop explícito), vuelve al pool.
pub struct PooledBuffer {
    // `Option` porque `drop` necesita tomar posesión del Vec para devolverlo.
    data: Option<Vec<u8>>,
    pool: Arc<Pool>,
}

impl PooledBuffer {
    /// Slice de escritura: toda la capacidad pre-asignada del buffer.
    /// Llamar antes de `read()`.
    #[inline]
    pub fn as_write_slice(&mut self) -> &mut [u8] {
        self.data.as_mut().unwrap().as_mut_slice()
    }

    /// Slice de lectura: solo los bytes válidos (hasta `len`).
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.data.as_ref().unwrap().as_slice()
    }

    /// Establece cuántos bytes del buffer son válidos.
    /// Llamar después de `read()` con el número de bytes leídos.
    #[inline]
    pub fn set_filled(&mut self, len: usize) {
        let v = self.data.as_mut().unwrap();
        // Safety: `len` viene de `read()` que garantiza que los primeros
        // `len` bytes están inicializados.
        unsafe { v.set_len(len) };
    }

    /// Número de bytes válidos actualmente en el buffer.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.as_ref().unwrap().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        if let Some(data) = self.data.take() {
            self.pool.release(data);
        }
    }
}

// Safety: PooledBuffer contiene un Vec<u8> y un Arc<Pool>.
// Vec<u8> es Send. Arc<Pool> es Send si Pool es Send.
// Pool solo contiene canales crossbeam que son Send.
unsafe impl Send for PooledBuffer {}
// PooledBuffer no implementa Sync porque &PooledBuffer permitiría
// acceso concurrente al Vec interno. El pipeline no necesita Sync.

// ─────────────────────────────────────────────────────────────────────────────
// BufferPool — handle público cloneable
// ─────────────────────────────────────────────────────────────────────────────

/// Pool de buffers pre-asignados para el pipeline de bloques.
///
/// Clonar `BufferPool` es barato — todos los clones comparten el mismo pool.
/// Crear un pool con `new()` pre-asigna todos los buffers de una vez.
#[derive(Clone)]
pub struct BufferPool {
    inner: Arc<Pool>,
}

impl BufferPool {
    /// Crea un pool con `pool_size` buffers de `buffer_size` bytes cada uno.
    ///
    /// Llamar una vez por `Orchestrator::run()`. El pool vive mientras
    /// haya al menos un `BufferPool` o `PooledBuffer` vivo.
    pub fn new(buffer_size: usize, pool_size: usize) -> Self {
        Self {
            inner: Pool::new(buffer_size, pool_size),
        }
    }

    /// Adquiere un buffer del pool.
    ///
    /// **Bloquea** si no hay buffers disponibles (backpressure correcto).
    /// El buffer se devuelve automáticamente al pool cuando se hace drop.
    pub fn acquire(&self) -> PooledBuffer {
        self.inner.acquire()
    }

    /// Número de buffers disponibles en este momento (para diagnóstico).
    pub fn available(&self) -> usize {
        self.inner.rx.len()
    }

    /// Capacidad total del pool.
    pub fn capacity(&self) -> usize {
        self.inner.buffer_size // reutilizamos el campo — renombrar si se expone más
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tipos legacy — mantenidos para compatibilidad con código existente
// ─────────────────────────────────────────────────────────────────────────────

/// Buffer standalone (sin pool). Usado por código que no usa el pipeline RAII.
///
/// Mantenido para compatibilidad — preferir `PooledBuffer` en código nuevo.
pub struct Buffer {
    data: Vec<u8>,
}

impl Buffer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self { data: Vec::with_capacity(capacity) }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] { self.data.as_mut_slice() }
    pub fn set_len(&mut self, len: usize) { unsafe { self.data.set_len(len) } }
    pub fn len(&self) -> usize { self.data.len() }
    pub fn capacity(&self) -> usize { self.data.capacity() }
    pub fn is_empty(&self) -> bool { self.data.is_empty() }
    pub fn as_slice(&self) -> &[u8] { self.data.as_slice() }
    pub fn clear(&mut self) { self.data.clear() }
    pub fn into_vec(self) -> Vec<u8> { self.data }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn acquire_and_auto_release() {
        let pool = BufferPool::new(4096, 4);
        assert_eq!(pool.available(), 4);

        {
            let buf = pool.acquire();
            assert_eq!(pool.available(), 3);
            drop(buf); // devolución automática
        }

        assert_eq!(pool.available(), 4);
    }

    #[test]
    fn data_survives_channel_transit() {
        let pool = BufferPool::new(1024, 2);
        let (tx, rx) = crossbeam::channel::bounded::<PooledBuffer>(2);

        // Simular reader
        let mut buf = pool.acquire();
        let slice = buf.as_write_slice();
        slice[0] = 0xDE;
        slice[1] = 0xAD;
        buf.set_filled(2);
        tx.send(buf).unwrap();
        drop(tx);

        // Simular writer
        let received = rx.recv().unwrap();
        assert_eq!(received.as_slice(), &[0xDE, 0xAD]);
        drop(received); // devolución automática

        // El buffer volvió al pool
        assert_eq!(pool.available(), 2);
    }

    #[test]
    fn no_deadlock_with_exact_pool_size() {
        // Pool de 2 buffers, canal de 2 — nunca debe deadlockear
        let pool = BufferPool::new(1024, 2);
        let (tx, rx) = crossbeam::channel::bounded::<PooledBuffer>(2);

        // Llenar el canal con todos los buffers del pool
        for i in 0u8..2 {
            let mut buf = pool.acquire();
            buf.as_write_slice()[0] = i;
            buf.set_filled(1);
            tx.send(buf).unwrap();
        }

        // Vaciar el canal — los buffers vuelven al pool
        for _ in 0..2 {
            drop(rx.recv().unwrap());
        }

        // Pool recuperado — se puede volver a usar
        assert_eq!(pool.available(), 2);
    }

    #[test]
    fn multi_thread_pipeline() {
        let pool = BufferPool::new(4096, 8);
        let (tx, rx) = crossbeam::channel::bounded::<PooledBuffer>(8);
        let pool_reader = pool.clone();

        let reader = thread::spawn(move || {
            for i in 0u8..32 {
                let mut buf = pool_reader.acquire();
                buf.as_write_slice()[0] = i;
                buf.set_filled(1);
                tx.send(buf).unwrap();
            }
        });

        let writer = thread::spawn(move || {
            let mut count = 0u8;
            for buf in &rx {
                assert_eq!(buf.as_slice()[0], count);
                count += 1;
                // drop(buf) implícito → devolución automática
            }
            count
        });

        reader.join().unwrap();
        let processed = writer.join().unwrap();
        assert_eq!(processed, 32);
        // Todos los buffers devueltos
        assert_eq!(pool.available(), 8);
    }

    #[test]
    fn buffer_reuse_no_allocations_after_warmup() {
        // Verificar que los punteros se reutilizan (misma memoria)
        let pool = BufferPool::new(4096, 1);

        let ptr1 = {
            let buf = pool.acquire();
            buf.data.as_ref().unwrap().as_ptr()
        }; // drop → vuelve al pool

        let ptr2 = {
            let buf = pool.acquire();
            buf.data.as_ref().unwrap().as_ptr()
        }; // drop → vuelve al pool

        assert_eq!(ptr1, ptr2, "el mismo Vec debe reutilizarse");
    }
}
