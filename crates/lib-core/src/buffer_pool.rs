//! Buffer Pool para reutilización de memoria sin asignaciones en el hot path.
//! 
//! Este módulo implementa un pool de buffers reutilizables usando canales crossbeam
//! para evitar la presión sobre el allocator durante las operaciones de copia intensiva.

use std::sync::Arc;
use crossbeam::channel::{bounded, Sender, Receiver, TrySendError};

/// Un buffer reutilizable para operaciones de I/O.
pub struct Buffer {
    data: Vec<u8>,
}

impl Buffer {
    /// Crea un nuevo buffer con la capacidad especificada.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
        }
    }

    /// Obtiene una referencia mutable al slice interno.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    /// Establece la longitud del buffer (útil después de leer datos).
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        unsafe {
            self.data.set_len(len);
        }
    }

    /// Obtiene la longitud actual del buffer.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Obtiene la capacidad del buffer.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    /// Verifica si el buffer está vacío.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Obtiene una referencia inmutable a los datos.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Limpia el buffer manteniendo la capacidad para reutilización.
    #[inline]
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Devuelve el Vec interno, consumiendo el buffer.
    pub fn into_vec(self) -> Vec<u8> {
        self.data
    }
}

/// Pool de buffers para reutilización de memoria.
/// 
/// Usa un canal bounded para gestionar un conjunto fijo de buffers,
/// evitando asignaciones dinámicas durante el procesamiento del pipeline.
pub struct BufferPool {
    sender: Sender<Buffer>,
    receiver: Arc<Receiver<Buffer>>,
    capacity: usize,
}

impl BufferPool {
    /// Crea un nuevo pool de buffers con la capacidad y cantidad especificadas.
    /// 
    /// # Argumentos
    /// * `buffer_size` - Tamaño de cada buffer en bytes
    /// * `pool_size` - Número de buffers en el pool
    pub fn new(buffer_size: usize, pool_size: usize) -> Self {
        let (sender, receiver) = bounded(pool_size);
        
        // Pre-allocar todos los buffers
        for _ in 0..pool_size {
            let buffer = Buffer::with_capacity(buffer_size);
            sender.send(buffer).expect("Failed to initialize buffer pool");
        }
        
        Self {
            sender,
            receiver: Arc::new(receiver),
            capacity: pool_size,
        }
    }

    /// Adquiere un buffer del pool. Bloquea si no hay buffers disponibles.
    pub fn acquire(&self) -> Buffer {
        self.receiver.recv().expect("Buffer pool channel closed")
    }

    /// Intenta adquirir un buffer sin bloquear.
    /// Retorna None si no hay buffers disponibles inmediatamente.
    pub fn try_acquire(&self) -> Option<Buffer> {
        match self.receiver.try_recv() {
            Ok(buffer) => Some(buffer),
            Err(_) => None,
        }
    }

    /// Devuelve un buffer al pool para su reutilización.
    pub fn release(&self, mut buffer: Buffer) {
        buffer.clear();
        // Usar try_send para evitar bloqueos si el pool está lleno o cerrado
        match self.sender.try_send(buffer) {
            Ok(_) => {},
            Err(TrySendError::Full(_)) => {
                // Pool lleno, descartar el buffer (se recolectará)
                // Esto puede pasar si el pool se está destruyendo
            },
            Err(TrySendError::Disconnected(_)) => {
                // El receptor se desconectó, el pool se está destruyendo
            },
        }
    }

    /// Obtiene la capacidad total del pool.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Obtiene el número de buffers actualmente disponibles.
    pub fn available_count(&self) -> usize {
        self.receiver.len()
    }

    /// Crea un clon del receptor para ser usado por múltiples consumidores.
    pub fn clone_receiver(&self) -> Arc<Receiver<Buffer>> {
        Arc::clone(&self.receiver)
    }
}

impl Clone for BufferPool {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            receiver: Arc::clone(&self.receiver),
            capacity: self.capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_pool_creation() {
        let pool = BufferPool::new(1024, 5);
        assert_eq!(pool.capacity(), 5);
        assert_eq!(pool.available_count(), 5);
    }

    #[test]
    fn test_acquire_release() {
        let pool = BufferPool::new(1024, 3);
        
        let mut buf1 = pool.acquire();
        assert_eq!(pool.available_count(), 2);
        
        buf1.as_mut_slice()[0] = 42;
        pool.release(buf1);
        
        assert_eq!(pool.available_count(), 3);
        
        let buf2 = pool.acquire();
        assert_eq!(buf2.as_slice()[0], 0); // Debería estar limpio después de release
    }

    #[test]
    fn test_try_acquire() {
        let pool = BufferPool::new(1024, 2);
        
        let _buf1 = pool.acquire();
        let _buf2 = pool.acquire();
        
        assert!(pool.try_acquire().is_none());
    }

    #[test]
    fn test_concurrent_access() {
        let pool = Arc::new(BufferPool::new(1024, 10));
        let mut handles = vec![];

        for i in 0..5 {
            let pool_clone = Arc::clone(&pool);
            let handle = thread::spawn(move || {
                for _ in 0..10 {
                    let mut buffer = pool_clone.acquire();
                    buffer.as_mut_slice()[0] = i as u8;
                    thread::sleep(Duration::from_millis(1));
                    pool_clone.release(buffer);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(pool.available_count(), 10);
    }

    #[test]
    fn test_buffer_operations() {
        let mut buffer = Buffer::with_capacity(100);
        
        assert_eq!(buffer.len(), 0);
        assert!(buffer.is_empty());
        
        buffer.set_len(50);
        assert_eq!(buffer.len(), 50);
        assert!(!buffer.is_empty());
        
        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert!(buffer.is_empty());
    }
}
