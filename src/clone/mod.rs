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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

/// Bytes ya procesados del origen en el clon en curso. Igual que `scanner`/`recovery`, es el
/// espejo que la GUI lee para dibujar su propia barra (no tiene la de `indicatif`). Se pasa POR
/// PARÁMETRO a la implementación para poder testear sin globales, y el punto de entrada público
/// le pasa este global.
static CLONE_PROGRESS_BYTES: AtomicU64 = AtomicU64::new(0);

pub fn is_clone_in_progress() -> bool {
    CLONE_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Lo llama el handler de Ctrl+C (o el botón "Detener" de la GUI) para pedir la cancelación
/// cooperativa del clon en curso.
pub fn request_cancel() {
    CLONE_CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Si ya se pidió cancelar el clon en curso. La GUI la usa para pasar el botón a "Deteniendo…":
/// la cancelación es cooperativa (se chequea una vez por bloque de 1 MiB), así que sin esta señal
/// el botón parece no haber hecho nada y se vuelve a apretar.
pub fn cancel_requested() -> bool {
    CLONE_CANCEL_REQUESTED.load(Ordering::SeqCst)
}

/// Bytes ya copiados del origen en el clon en curso. La GUI lo compara contra el tamaño total
/// (`device_or_file_size`) para dibujar la barra de progreso.
pub fn clone_progress_bytes() -> u64 {
    CLONE_PROGRESS_BYTES.load(Ordering::Relaxed)
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

/// Clona `source_path` (disco o archivo) a un archivo de imagen en `output_path` (modo CLI: barra
/// de progreso `indicatif` por terminal). Usa el flag global de cancelación (el que setea el
/// handler de Ctrl+C).
pub fn clone_to_image(source_path: &Path, output_path: &Path) -> Result<CloneResult> {
    clone_to_image_entry(source_path, output_path, false)
}

/// Igual que `clone_to_image`, pero SIN salida por terminal (ni barra `indicatif`). Pensada para
/// la GUI: un binario de subsistema gráfico en Windows no tiene consola, así que la barra de
/// `indicatif` escribiría a un stdout inexistente. El avance se sigue con `clone_progress_bytes()`
/// y la cancelación con `request_cancel()`.
pub fn clone_to_image_quiet(source_path: &Path, output_path: &Path) -> Result<CloneResult> {
    clone_to_image_entry(source_path, output_path, true)
}

/// Preparación común de los dos puntos de entrada: arma el estado global (flag de cancelación y
/// contador de progreso) y garantiza limpiar `CLONE_IN_PROGRESS` con un guard `Drop`, así un `?` a
/// mitad de camino no lo deja colgado en true (lo que haría que el próximo Ctrl+C no cerrara nada).
fn clone_to_image_entry(
    source_path: &Path,
    output_path: &Path,
    quiet: bool,
) -> Result<CloneResult> {
    // Guard `Drop`: garantiza limpiar `CLONE_IN_PROGRESS` pase lo que pase (incluido `?`).
    struct InProgressGuard;
    impl Drop for InProgressGuard {
        fn drop(&mut self) {
            CLONE_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
    }
    // `CLONE_IN_PROGRESS` se levanta AL FINAL, con el resto del estado ya limpio: la GUI usa ese
    // flag para saber cuándo mostrar el progreso y ofrecer "Detener". Si se levantara antes, un
    // clic en Detener caído en esa ventana lo pisaría el reset de acá y se perdería en silencio
    // (misma lección que `recovery` y `scanner`).
    CLONE_CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    CLONE_PROGRESS_BYTES.store(0, Ordering::Relaxed);
    CLONE_IN_PROGRESS.store(true, Ordering::SeqCst);
    let _guard = InProgressGuard;

    clone_to_image_impl(
        source_path,
        output_path,
        &CLONE_CANCEL_REQUESTED,
        &CLONE_PROGRESS_BYTES,
        !quiet,
    )
}

/// Núcleo del clonado. `cancel` y `progress` se reciben por parámetro (no se leen los globales)
/// para testeabilidad sin interferencia entre tests en paralelo. `show_progress` desactiva la
/// barra `indicatif` (en los tests y en la GUI).
fn clone_to_image_impl(
    source_path: &Path,
    output_path: &Path,
    cancel: &AtomicBool,
    progress: &AtomicU64,
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

    // PROTECCIÓN DE DATOS: si el destino ya existe y es un SYMLINK o un nodo de DISPOSITIVO
    // (block/char device), `File::create` escribiría a través de él. Un `copia.img -> /dev/sdb`
    // preexistente, o un device node puesto directo en el destino (o alcanzado por un directorio
    // padre symlinkeado), haría que el clon — que corre con permisos elevados — sobrescriba el
    // disco apuntado, y `is_physical_device` — que solo mira el nombre `copia.img` — no lo detecta.
    // Se rechaza antes de abrir nada. `symlink_metadata` no sigue el symlink final, así que lo ve
    // como tal en vez de resolverlo a su objetivo.
    if let Ok(meta) = std::fs::symlink_metadata(output_path) {
        let ft = meta.file_type();
        let es_symlink = ft.is_symlink();
        #[cfg(unix)]
        let es_dispositivo = {
            use std::os::unix::fs::FileTypeExt;
            ft.is_block_device() || ft.is_char_device()
        };
        #[cfg(not(unix))]
        let es_dispositivo = false;
        if es_symlink || es_dispositivo {
            anyhow::bail!(
                "El destino '{}' es un enlace o un dispositivo, no un archivo normal: podría \
                 apuntar a un disco y sobrescribirlo. Elegí una ruta de archivo normal para la copia.",
                output_path.display()
            );
        }
    }
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

        // Espejo para la GUI (que no tiene la barra de `indicatif`): se actualiza siempre, con o
        // sin barra visible.
        progress.store(pos, Ordering::Relaxed);
        if let Some(pb) = &pb {
            pb.set_position(pos);
        }
    }

    // `sync_all` (fsync) fuerza el volcado a disco antes de declarar éxito. `File::flush` NO
    // hace esto (es un no-op para un `File`): sin el fsync, el "clonado completado" podría salir
    // con datos todavía en la page cache, y un usuario no técnico que desenchufa el USB sin
    // "expulsar" se quedaría con una imagen corrupta pese al mensaje de éxito. Además, un
    // ENOSPC/EIO diferido recién aflora acá.
    dst.sync_all().with_context(|| {
        format!(
            "No se pudo terminar de guardar la imagen: {} (¿espacio insuficiente o el disco de destino se desconectó?)",
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
                // `read_filling` solo devuelve un `Ok` corto cuando llegó a EOF real
                // (`read()==0`), NUNCA por un sector dañado (eso devuelve `Err`). O sea: el
                // origen terminó antes de lo que reportó `device_or_file_size` (archivo truncado,
                // dispositivo que miente el tamaño). Rellenamos con ceros para mantener la imagen
                // del tamaño declarado, pero eso NO son sectores dañados: `bad = 0` (contarlo como
                // daño daría un resumen falso tipo "9 MB no se pudieron leer").
                dst.write_all(&buf[..n])?;
                dst.write_all(&zeros[..len - n])?;
                return Ok((n as u64, 0));
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
                    // Igual que en el camino rápido: un `Ok` corto acá es EOF del origen, no un
                    // sector dañado. Se rellena con ceros pero no se cuenta como daño.
                    dst.write_all(&buf[..m])?;
                    dst.write_all(&zeros[..sl - m])?;
                    good += m as u64;
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
    fn test_copy_block_eof_is_not_counted_as_damage() {
        // Origen más corto que el bloque pedido: el faltante es EOF (el origen se acabó), NO
        // sectores dañados. Antes esto se contaba como bytes malos y daba un resumen falso.
        let mut src = tempfile::NamedTempFile::new().unwrap();
        src.write_all(&[0xABu8; 300]).unwrap();
        src.flush().unwrap();
        let mut src_file = File::open(src.path()).unwrap();

        let dst = tempfile::NamedTempFile::new().unwrap();
        let mut dst_file = File::create(dst.path()).unwrap();

        let len = 1024usize;
        let mut buf = vec![0u8; len];
        let zeros = vec![0u8; len];
        let (good, bad) =
            copy_block(&mut src_file, &mut dst_file, 0, len, &mut buf, &zeros).unwrap();

        assert_eq!(good, 300);
        assert_eq!(bad, 0, "el relleno por EOF no debe contar como daño");
        // La imagen igual queda del tamaño del bloque (300 datos + 724 ceros de padding).
        dst_file.sync_all().unwrap();
        assert_eq!(std::fs::metadata(dst.path()).unwrap().len(), len as u64);
    }

    #[test]
    fn test_clone_copies_bytes_exactly() {
        // Origen con contenido conocido más grande que un bloque para ejercitar el loop.
        let mut src = tempfile::NamedTempFile::new().unwrap();
        let data: Vec<u8> = (0..(BLOCK + 12345)).map(|i| (i % 251) as u8).collect();
        src.write_all(&data).unwrap();
        src.flush().unwrap();

        let dst = tempfile::NamedTempFile::new().unwrap();
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let result = clone_to_image_impl(src.path(), dst.path(), &cancel, &progress, false)
            .expect("clonado falló");

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
        let progress = AtomicU64::new(0);
        let result = clone_to_image_impl(src.path(), dst.path(), &cancel, &progress, false)
            .expect("clonado falló");

        assert!(result.cancelled);
        assert!(
            result.good_bytes < data.len() as u64,
            "canceló al inicio, no debería haber copiado todo (copió {})",
            result.good_bytes
        );
    }

    #[test]
    fn test_clone_progress_counter_reaches_total() {
        // El contador que lee la GUI debe terminar en el tamaño total del origen. Se usa un
        // contador LOCAL (no el global `CLONE_PROGRESS_BYTES`) para no interferir con otros tests
        // en paralelo, igual que se hace con el flag de cancelación.
        let mut src = tempfile::NamedTempFile::new().unwrap();
        let data = vec![0x33u8; BLOCK * 2 + 777];
        src.write_all(&data).unwrap();
        src.flush().unwrap();

        let dst = tempfile::NamedTempFile::new().unwrap();
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let result = clone_to_image_impl(src.path(), dst.path(), &cancel, &progress, false)
            .expect("clonado falló");

        assert_eq!(
            progress.load(Ordering::Relaxed),
            data.len() as u64,
            "el contador de progreso debe llegar al tamaño total"
        );
        assert_eq!(result.total_bytes, data.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn test_clone_rejects_symlink_destination() {
        // Regresión (auditoría pre-beta): un destino que es un symlink preexistente (ej.
        // `copia.img -> /dev/sdb`) haría que `File::create`, con permisos elevados, siga el enlace
        // y sobrescriba el disco apuntado. Debe rechazarse antes de abrir nada.
        use std::os::unix::fs::symlink;

        let mut src = tempfile::NamedTempFile::new().unwrap();
        src.write_all(&[0u8; 2048]).unwrap();
        src.flush().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("objetivo_real");
        std::fs::write(&target, b"contenido previo").unwrap();
        let link = dir.path().join("copia.img");
        symlink(&target, &link).unwrap();

        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let res = clone_to_image_impl(src.path(), &link, &cancel, &progress, false);

        assert!(res.is_err(), "un destino symlink debe rechazarse");
        // Y el objetivo del symlink no se tocó.
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"contenido previo",
            "no se debe haber escrito a través del symlink"
        );
    }
}
