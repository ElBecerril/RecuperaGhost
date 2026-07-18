//! Clonado de un disco (posiblemente fallando) a un archivo de imagen `.img`.
//!
//! El escenario central de la herramienta es un disco que está muriendo. Escanear un disco así
//! en vivo lo estresa y puede acelerar su muerte; lo correcto es sacarle primero una copia byte
//! a byte a un archivo de imagen y después escanear la imagen (que ya no tiene sectores que
//! fallen). Este módulo hace esa copia.
//!
//! Tolerancia a errores (v1, una sola pasada lineal): el clon NUNCA aborta por un error de
//! lectura. Camino rápido: lee en bloques de 1 MiB. Si un bloque falla (sector dañado), cae a
//! modo sector por sector (512 B) para rescatar los sectores buenos que rodean al dañado y
//! rellenar con ceros solo los ilegibles — así se pierde a lo sumo un sector por cada sector
//! realmente muerto, en vez de 1 MiB entero. El único error que corta el clon es de ESCRITURA
//! al destino (ej. se llenó el disco): en ese caso la imagen parcial ya escrita sigue siendo
//! válida y escaneable.
//!
//! Cancelación cooperativa (Ctrl+C): mismo patrón que el scanner. Un `AtomicBool` que el loop de
//! copia chequea una vez por bloque; se recibe por parámetro (no se lee el global directamente)
//! para poder testear sin interferencia entre tests en paralelo.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

/// Bloque del camino rápido: 1 MiB, múltiplo de 512 (alineación a sector).
const BLOCK: usize = 1024 * 1024;
/// Granularidad del refinamiento cuando un bloque falla: 512 B (tamaño de sector).
const SECTOR: usize = 512;

// Igual que el scanner: `CLONE_IN_PROGRESS` le dice al handler de Ctrl+C que hay un clon en
// curso (para cancelarlo en vez de cerrar el programa); `CLONE_CANCEL_REQUESTED` es el flag que
// el handler setea y que el loop de copia chequea.
static CLONE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static CLONE_CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn is_clone_in_progress() -> bool {
    CLONE_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Lo llama el handler de Ctrl+C para pedir la cancelación cooperativa del clon en curso.
pub fn request_cancel() {
    CLONE_CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Resultado de un clonado.
pub struct CloneResult {
    /// Tamaño del disco/archivo de origen (== tamaño de la imagen resultante).
    pub total_bytes: u64,
    /// Bytes leídos correctamente del origen.
    pub good_bytes: u64,
    /// Bytes que no se pudieron leer (sectores dañados), rellenados con ceros en la imagen.
    pub bad_bytes: u64,
    /// Cantidad de bloques de 1 MiB que tuvieron al menos un sector ilegible.
    pub bad_blocks: u64,
    /// El usuario canceló con Ctrl+C antes de terminar (la imagen parcial sigue siendo válida).
    pub cancelled: bool,
    pub output_path: PathBuf,
}

impl CloneResult {
    /// Resumen amigable para el público no técnico.
    pub fn summary(&self) -> String {
        use crate::util::format_size;
        let mut s = String::new();
        if self.cancelled {
            s.push_str("  ⏹️  Cancelaste el clonado. La copia parcial quedó guardada y se puede escanear igual.\n");
        }
        s.push_str(&format!(
            "  📀 Imagen creada: {}\n     Tamaño: {} — copiado correctamente: {}",
            self.output_path.display(),
            format_size(self.total_bytes),
            format_size(self.good_bytes),
        ));
        if self.bad_bytes > 0 {
            s.push_str(&format!(
                "\n  ⚠️  {} no se pudieron leer (sectores dañados en {} zona/s) y quedaron en blanco en la imagen.\n     Es normal en un disco que está fallando; lo que sí se pudo leer está a salvo en la imagen.",
                format_size(self.bad_bytes),
                self.bad_blocks,
            ));
        }
        s
    }
}

/// Clona `source_path` (disco o archivo) a un archivo de imagen en `output_path`.
/// Usa el flag global de cancelación (el que setea el handler de Ctrl+C).
pub fn clone_to_image(source_path: &Path, output_path: &Path) -> Result<CloneResult> {
    // Guard `Drop`: garantiza limpiar `CLONE_IN_PROGRESS` pase lo que pase (incluido `?`).
    struct InProgressGuard;
    impl Drop for InProgressGuard {
        fn drop(&mut self) {
            CLONE_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
    }
    CLONE_CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    CLONE_IN_PROGRESS.store(true, Ordering::SeqCst);
    let _guard = InProgressGuard;

    clone_to_image_impl(source_path, output_path, &CLONE_CANCEL_REQUESTED, true)
}

/// Núcleo del clonado. `cancel` se recibe por parámetro (no se lee el global) para testeabilidad.
/// `show_progress` desactiva la barra en los tests.
fn clone_to_image_impl(
    source_path: &Path,
    output_path: &Path,
    cancel: &AtomicBool,
    show_progress: bool,
) -> Result<CloneResult> {
    let total = crate::scanner::device_or_file_size(source_path).with_context(|| {
        format!(
            "No se pudo determinar el tamaño de: {}",
            source_path.display()
        )
    })?;

    let mut src = File::open(source_path)
        .with_context(|| format!("No se pudo abrir el origen: {}", source_path.display()))?;
    let mut dst = File::create(output_path)
        .with_context(|| format!("No se pudo crear la imagen: {}", output_path.display()))?;

    let pb = if show_progress {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(
                "  👻 Clonando [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
            )
            .unwrap()
            .progress_chars("█▓▒░  "),
        );
        Some(pb)
    } else {
        None
    };

    let mut buf = vec![0u8; BLOCK];
    let zeros = vec![0u8; BLOCK];

    let mut pos: u64 = 0;
    let mut good_bytes: u64 = 0;
    let mut bad_bytes: u64 = 0;
    let mut bad_blocks: u64 = 0;
    let mut cancelled = false;

    while pos < total {
        // Cancelación cooperativa: se chequea una vez por bloque (~decenas de ms de respuesta).
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            break;
        }

        let this_block = std::cmp::min(BLOCK as u64, total - pos) as usize;
        let (good, bad) = copy_block(&mut src, &mut dst, pos, this_block, &mut buf, &zeros)
            .with_context(|| {
                format!(
                    "No se pudo escribir la imagen en: {} (¿espacio insuficiente en el destino?)",
                    output_path.display()
                )
            })?;

        good_bytes += good;
        bad_bytes += bad;
        if bad > 0 {
            bad_blocks += 1;
        }
        pos += this_block as u64;

        if let Some(pb) = &pb {
            pb.set_position(pos);
        }
    }

    dst.flush().with_context(|| {
        format!(
            "No se pudo finalizar la imagen: {} (¿espacio insuficiente?)",
            output_path.display()
        )
    })?;

    if let Some(pb) = &pb {
        if cancelled {
            pb.abandon_with_message("⏹️  Clonado cancelado");
        } else {
            pb.finish_with_message("✅ Clonado completado");
        }
    }

    Ok(CloneResult {
        total_bytes: total,
        good_bytes,
        bad_bytes,
        bad_blocks,
        cancelled,
        output_path: output_path.to_path_buf(),
    })
}

/// Copia la región `[start, start+len)` del origen al destino, escribiendo SIEMPRE exactamente
/// `len` bytes al destino (datos buenos + relleno de ceros donde no se pudo leer), para que la
/// imagen quede alineada y del tamaño exacto del origen. Devuelve `(bytes_buenos, bytes_malos)`.
///
/// Camino rápido: un solo `read` del bloque completo. Si falla (sector dañado), refina sector por
/// sector para rescatar los sectores buenos alrededor del dañado. Solo puede devolver `Err` por un
/// fallo de ESCRITURA al destino; los errores de LECTURA del origen se absorben (relleno + conteo).
fn copy_block(
    src: &mut File,
    dst: &mut File,
    start: u64,
    len: usize,
    buf: &mut [u8],
    zeros: &[u8],
) -> io::Result<(u64, u64)> {
    // El destino se escribe siempre secuencialmente (bloques en orden, sub-sectores en orden),
    // así que nunca se hace seek en `dst`.
    if src.seek(SeekFrom::Start(start)).is_ok() {
        match read_filling(src, &mut buf[..len]) {
            Ok(n) if n == len => {
                dst.write_all(&buf[..len])?;
                return Ok((len as u64, 0));
            }
            Ok(n) => {
                // EOF/short read prematuro: escribir lo bueno y rellenar el resto con ceros.
                dst.write_all(&buf[..n])?;
                dst.write_all(&zeros[..len - n])?;
                return Ok((n as u64, (len - n) as u64));
            }
            Err(_) => {
                // Sector dañado en algún punto del bloque: refinar sector por sector.
            }
        }
    }

    // Modo refinamiento: sector por sector (512 B).
    let mut good: u64 = 0;
    let mut bad: u64 = 0;
    let mut off = 0usize;
    while off < len {
        let sl = std::cmp::min(SECTOR, len - off);
        let ok = src.seek(SeekFrom::Start(start + off as u64)).is_ok();
        if ok {
            match read_filling(src, &mut buf[..sl]) {
                Ok(m) if m == sl => {
                    dst.write_all(&buf[..sl])?;
                    good += sl as u64;
                    off += sl;
                    continue;
                }
                Ok(m) => {
                    dst.write_all(&buf[..m])?;
                    dst.write_all(&zeros[..sl - m])?;
                    good += m as u64;
                    bad += (sl - m) as u64;
                    off += sl;
                    continue;
                }
                Err(_) => {}
            }
        }
        // Sector ilegible (o seek falló): rellenar con ceros.
        dst.write_all(&zeros[..sl])?;
        bad += sl as u64;
        off += sl;
    }

    Ok((good, bad))
}

/// Lee de `src` hacia `buf` reintentando en short reads hasta llenar `buf` o llegar a EOF real
/// (`read` == 0). Devuelve la cantidad de bytes leídos. Propaga el primer error de lectura real
/// (sector dañado): eso es lo que dispara el modo refinamiento en `copy_block`.
fn read_filling(src: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break, // EOF real
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clone_copies_bytes_exactly() {
        // Origen con contenido conocido más grande que un bloque para ejercitar el loop.
        let mut src = tempfile::NamedTempFile::new().unwrap();
        let data: Vec<u8> = (0..(BLOCK + 12345)).map(|i| (i % 251) as u8).collect();
        src.write_all(&data).unwrap();
        src.flush().unwrap();

        let dst = tempfile::NamedTempFile::new().unwrap();
        let cancel = AtomicBool::new(false);
        let result =
            clone_to_image_impl(src.path(), dst.path(), &cancel, false).expect("clonado falló");

        assert_eq!(result.total_bytes, data.len() as u64);
        assert_eq!(result.good_bytes, data.len() as u64);
        assert_eq!(result.bad_bytes, 0);
        assert_eq!(result.bad_blocks, 0);
        assert!(!result.cancelled);

        // La imagen debe ser byte a byte idéntica al origen.
        let mut out = Vec::new();
        File::open(dst.path())
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn test_clone_cancellation_stops_early() {
        // Con el flag ya en true, el clon corta en el primer chequeo (antes de copiar todo).
        let mut src = tempfile::NamedTempFile::new().unwrap();
        let data = vec![0x5Au8; BLOCK * 4];
        src.write_all(&data).unwrap();
        src.flush().unwrap();

        let dst = tempfile::NamedTempFile::new().unwrap();
        let cancel = AtomicBool::new(true);
        let result =
            clone_to_image_impl(src.path(), dst.path(), &cancel, false).expect("clonado falló");

        assert!(result.cancelled);
        assert!(
            result.good_bytes < data.len() as u64,
            "canceló al inicio, no debería haber copiado todo (copió {})",
            result.good_bytes
        );
    }
}
