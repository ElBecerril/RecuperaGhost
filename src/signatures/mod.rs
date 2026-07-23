use std::fmt;

/// Categoría de archivo multimedia
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCategory {
    Photo,
    Video,
    Audio,
    Document,
}

impl fmt::Display for FileCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileCategory::Photo => write!(f, "📷 Foto"),
            FileCategory::Video => write!(f, "🎬 Video"),
            FileCategory::Audio => write!(f, "🎵 Audio"),
            FileCategory::Document => write!(f, "📄 Documento"),
        }
    }
}

/// Validación bit-level adicional para firmas de header corto:
/// `(validador, bytes_necesarios_desde_el_inicio_del_header)`.
pub type SignatureValidator = (fn(&[u8]) -> bool, usize);

/// Calcula el tamaño real de un stream de audio por frames a partir de sus bytes.
/// El `bool` dice si esos bytes llegan hasta el final del origen (ver `chain_size`).
pub type StreamEndFn = fn(&[u8], bool) -> Option<usize>;

/// Devuelve `(largo_del_frame, indice_de_sample_rate)` del frame que empieza en la posición dada.
type FrameAtFn = fn(&[u8], usize) -> Option<(usize, u8)>;

/// Firma de archivo: magic bytes de cabecera y pie opcional
#[derive(Debug, Clone)]
pub struct FileSignature {
    pub name: &'static str,
    pub extension: &'static str,
    pub category: FileCategory,
    pub header: &'static [u8],
    pub header_offset: usize,
    /// Verificación adicional: (bytes_esperados, offset_desde_inicio_del_archivo)
    /// Usado para desambiguar formatos que comparten la misma cabecera (ej. RIFF, OggS)
    pub extra_check: Option<(&'static [u8], usize)>,
    pub footer: Option<&'static [u8]>,
    pub max_size: usize,
    /// Validación bit-level adicional para firmas de header corto (2 bytes) que de otra forma
    /// generan falsos positivos masivos en datos de alta entropía (ej. MP3 Sync, AAC ADTS).
    /// `(validador, bytes_necesarios_desde_el_inicio_del_header)`. El validador recibe el slice
    /// del buffer que empieza en el inicio del header y debe tener al menos esa cantidad de bytes.
    pub validator: Option<SignatureValidator>,
    /// Para formatos que codifican su propio tamaño en el header (ej. BMP: BITMAPFILEHEADER
    /// trae el tamaño total en offset 2, 4 bytes little-endian) en vez de usar un footer o
    /// `max_size` fijo. `(offset_desde_inicio_del_header, cantidad_de_bytes_LE)`.
    pub size_from_header: Option<(usize, usize)>,
}

impl FileSignature {
    /// Función que calcula el tamaño real de un stream de audio basado en frames (MP3, AAC),
    /// formatos que NO tienen footer ni traen su tamaño en el header.
    ///
    /// Va como despacho por nombre —y no como un campo más de la tabla— por el mismo criterio que
    /// `signature_is_zip_ooxml` en el scanner: son excepciones puntuales de tres firmas, y meter un
    /// campo obligaría a tocar las 28 entradas para dejarlo en `None` en 25.
    pub fn stream_end(&self) -> Option<StreamEndFn> {
        match self.name {
            "MP3 (ID3)" | "MP3 (Sync)" => Some(mp3_stream_end),
            "AAC" => Some(aac_stream_end),
            _ => None,
        }
    }

    /// Familia de audio por frames de esta firma, para poder seguir la cadena leyendo del disco
    /// cuando el archivo no entra en el buffer del escaneo.
    pub fn audio_stream(&self) -> Option<AudioStream> {
        match self.name {
            "MP3 (ID3)" | "MP3 (Sync)" => Some(AudioStream::Mpeg),
            "AAC" => Some(AudioStream::Adts),
            _ => None,
        }
    }
}

impl fmt::Display for FileSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (.{})", self.name, self.extension)
    }
}

/// Tabla de bitrates (kbps) para MPEG1 Layer III, indexada por el campo bitrate_index (4
/// bits) del 3er byte del header. Índices 0 (free) y 15 (inválido) se marcan con 0 y se
/// rechazan antes de usar la tabla.
const MP3_BITRATES_KBPS: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];

/// Tabla de sample rates (Hz) para MPEG1, indexada por sample_rate_index (2 bits). Índice 3
/// (reservado) se marca con 0 y se rechaza antes de usar la tabla.
const MP3_SAMPLE_RATES_HZ: [u32; 4] = [44100, 48000, 32000, 0];

/// Cuántos frames consecutivos hay que encadenar para creerle a un syncword de audio.
///
/// MP3 y AAC se detectan por un syncword de ~12 bits sin footer, así que en datos binarios de alta
/// entropía aparecen a montones. Encadenar solo 2 frames (lo que se hacía antes) resultó MUY
/// insuficiente: medido sobre 382 MB de binarios del sistema, seguían colándose 140 "MP3" y 123
/// "AAC" que, al no tener footer, se carveaban enteros hasta `max_size` — 13 GB de basura a partir
/// de 382 MB de origen. Con 12 frames, cada candidato tiene que acertar 12 largos calculados
/// exactamente y mantener el sample rate constante; la probabilidad de que datos que no son audio
/// hagan eso es despreciable.
pub const AUDIO_MIN_CHAIN_FRAMES: usize = 12;

/// Familia de audio basado en frames encadenados, sin footer ni tamaño en el header.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AudioStream {
    /// MPEG Audio (los MP3): el largo del frame se calcula con la fórmula estándar.
    Mpeg,
    /// ADTS (los AAC): el largo viene explícito en el header del frame.
    Adts,
}

impl AudioStream {
    fn frame_at(self) -> FrameAtFn {
        match self {
            AudioStream::Mpeg => mpeg_frame_at,
            AudioStream::Adts => adts_frame_at,
        }
    }

    /// Bytes mínimos para poder leer un header de frame de esta familia.
    fn header_len(self) -> usize {
        match self {
            AudioStream::Mpeg => 3,
            AudioStream::Adts => 6,
        }
    }
}

/// Por qué se detuvo el recorrido de frames. La diferencia importa: decide si un candidato se
/// rechaza, y si se puede afirmar dónde termina el archivo.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ChainStop {
    /// Lo que sigue NO es un frame de audio: el stream terminó ahí. Es el final limpio y el único
    /// caso en que se puede afirmar el tamaño del archivo con datos de sobra por delante.
    BadData,
    /// Se acabaron los datos disponibles justo en un límite de frame. Si esos datos eran todo el
    /// origen, es un final limpio también; si solo se acabó el buffer, no se puede afirmar nada.
    NoMoreData,
    /// El último frame declara más largo del que hay: el archivo está cortado a la mitad.
    Truncated,
    /// Se llegó al tope de frames pedidos (solo lo usa la validación, que no necesita recorrer
    /// todo el archivo para convencerse).
    Capped,
}

/// Resultado de recorrer una cadena de frames de audio.
pub struct FrameChain {
    /// Cuántos frames válidos se encadenaron.
    pub frames: usize,
    /// Cuántos bytes cubren esos frames (el tamaño real del stream si la cadena terminó).
    pub bytes: usize,
    pub stop: ChainStop,
    /// Índice de sample rate visto en la cadena. Se devuelve para poder seguir el recorrido en el
    /// chunk siguiente sin perder el invariante de "el sample rate no cambia dentro de un stream".
    pub sample_rate: Option<u8>,
}

/// Largo del frame MPEG Audio que empieza en `pos`, y su índice de sample rate.
///
/// El largo sale de la fórmula estándar de MPEG1 Layer III
/// (144000 * bitrate_kbps / sample_rate_hz + padding) con los campos del propio header. Devuelve
/// `None` si ahí no hay un header estructuralmente válido (o si no quedan bytes para leerlo).
fn mpeg_frame_at(bytes: &[u8], pos: usize) -> Option<(usize, u8)> {
    let h = bytes.get(pos..pos.checked_add(3)?)?;
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let b2 = h[2];
    let bitrate_index = (b2 >> 4) & 0x0F;
    let sample_rate_index = (b2 >> 2) & 0x03;
    if bitrate_index == 0x00 || bitrate_index == 0x0F || sample_rate_index == 0x03 {
        return None;
    }
    let bitrate_kbps = MP3_BITRATES_KBPS[bitrate_index as usize];
    let sample_rate_hz = MP3_SAMPLE_RATES_HZ[sample_rate_index as usize];
    let padding = ((b2 >> 1) & 0x01) as u32;
    // Ya se descartaron los índices inválidos, así que ambos son > 0: la división es segura.
    let frame_len = ((144_000 * bitrate_kbps) / sample_rate_hz + padding) as usize;
    if frame_len == 0 {
        return None;
    }
    Some((frame_len, sample_rate_index))
}

/// Largo del frame ADTS (AAC) que empieza en `pos`, y su índice de sample rate.
///
/// A diferencia de MP3, ADTS trae el largo EXPLÍCITO en 13 bits repartidos en los bytes 3-5.
fn adts_frame_at(bytes: &[u8], pos: usize) -> Option<(usize, u8)> {
    let h = bytes.get(pos..pos.checked_add(6)?)?;
    if h[0] != 0xFF || (h[1] & 0xF0) != 0xF0 {
        return None;
    }
    let profile = (h[2] >> 6) & 0x03;
    let sampling_freq_index = (h[2] >> 2) & 0x0F;
    if profile == 0x03 || sampling_freq_index > 12 {
        return None;
    }
    let frame_length =
        (((h[3] & 0x03) as usize) << 11) | ((h[4] as usize) << 3) | ((h[5] >> 5) as usize);
    // El largo incluye el propio header ADTS (mínimo 7 bytes sin CRC): menos que eso es
    // estructuralmente imposible y además haría que la cadena no avance nunca.
    if frame_length < 7 {
        return None;
    }
    Some((frame_length, sampling_freq_index))
}

/// Recorre frames consecutivos desde el inicio de `bytes`, parando en `max_frames`.
///
/// Además de que cada frame sea válido, exige que el **sample rate no cambie**: dentro de un stream
/// de audio real es constante, mientras que en datos que solo aciertan por casualidad varía. El
/// bitrate SÍ puede cambiar (los MP3 VBR son comunes), así que no se exige.
pub fn walk_audio_frames(
    bytes: &[u8],
    kind: AudioStream,
    max_frames: usize,
    expected_sample_rate: Option<u8>,
) -> FrameChain {
    let (frame_at, header_len) = (kind.frame_at(), kind.header_len());
    let mut pos = 0usize;
    let mut frames = 0usize;
    let mut sample_rate: Option<u8> = expected_sample_rate;
    let stop = loop {
        if frames >= max_frames {
            break ChainStop::Capped;
        }
        if pos.saturating_add(header_len) > bytes.len() {
            break ChainStop::NoMoreData;
        }
        let Some((len, sr)) = frame_at(bytes, pos) else {
            break ChainStop::BadData;
        };
        // El sample rate no cambia dentro de un stream real; si cambia, lo que se estaba siguiendo
        // no era una cadena de audio.
        if *sample_rate.get_or_insert(sr) != sr {
            break ChainStop::BadData;
        }
        if pos + len > bytes.len() {
            // El frame declara más de lo que hay. Se corta ANTES de ese frame, sin contarlo: así
            // `bytes` queda apuntando justo al inicio del frame incompleto, y quien vaya leyendo
            // por chunks puede retomar exactamente ahí (si el archivo sigue más allá del chunk) sin
            // partir un frame al medio.
            break ChainStop::Truncated;
        }
        pos += len;
        frames += 1;
    };
    FrameChain {
        frames,
        bytes: pos.min(bytes.len()),
        stop,
        sample_rate,
    }
}

/// True si una cadena de frames alcanza para creerle al candidato.
///
/// Se acepta también la cadena corta que se cortó por falta de datos: rechazar un audio real solo
/// porque el candidato cayó cerca del final del buffer disponible sería perder un archivo de la
/// persona, que es el error caro. El de más acá (colar basura) se paga en ruido, no en pérdida.
fn chain_is_credible(chain: &FrameChain) -> bool {
    chain.frames >= AUDIO_MIN_CHAIN_FRAMES || chain.stop != ChainStop::BadData
}

/// Tamaño del stream a partir de una cadena ya recorrida, o `None` si no se puede afirmar dónde
/// termina.
///
/// `at_source_end` dice si los datos que se le pasaron al walker llegan hasta el final del origen.
/// Es la diferencia entre "el audio termina justo acá" (final limpio, se puede afirmar el tamaño) y
/// "se me acabó el buffer y el archivo quizá sigue" (no se puede). Sin esta distinción, un MP3 que
/// ocupa todo el origen —el caso más común al escanear una tarjeta llena de música— se marcaba
/// "posiblemente dañado" y NO se guardaba: se detectó probando con un archivo real.
pub fn chain_size(chain: &FrameChain, at_source_end: bool) -> Option<usize> {
    if chain.frames < AUDIO_MIN_CHAIN_FRAMES {
        return None;
    }
    match chain.stop {
        ChainStop::BadData => Some(chain.bytes),
        ChainStop::NoMoreData if at_source_end => Some(chain.bytes),
        // Cortado a la mitad, o sin datos suficientes para saberlo: que salga marcado como
        // "posiblemente dañado" en vez de afirmar un tamaño que no se puede sostener.
        _ => None,
    }
}

/// Valida un candidato "MP3 (Sync)" exigiendo una cadena real de frames MPEG (ver
/// `AUDIO_MIN_CHAIN_FRAMES`), no solo un header plausible.
fn validate_mp3_sync(bytes: &[u8]) -> bool {
    if mpeg_frame_at(bytes, 0).is_none() {
        return false;
    }
    let chain = walk_audio_frames(bytes, AudioStream::Mpeg, AUDIO_MIN_CHAIN_FRAMES, None);
    chain_is_credible(&chain)
}

/// Valida un header ADTS (AAC) tras el syncword FF F1 de 12 bits. El campo layer (2 bits)
/// ya queda fijado por el byte F1 del header. Dos niveles de chequeo:
/// 1. Bits reservados en el 3er byte del header ADTS (offset 2): profile (2 bits, no debe ser
///    11 = reservado) y sampling_frequency_index (4 bits, valores 13-15 son reservados/
///    inválidos para ADTS).
/// 2. Frame chaining (C2 fix v2, mismo motivo que `validate_mp3_sync`): `frame_length` es un
///    campo explícito de 13 bits en los bytes 3-6 del header ADTS (2 bits en el byte 3, 8 bits
///    en el byte 4, 3 bits en el byte 5). Se verifica que en el offset
///    (header_start + frame_length) exista OTRO syncword ADTS válido (12 bits FF Fx). Si no
///    hay suficiente buffer para leerlo, se acepta el candidato sin ese chequeo extra.
fn validate_aac_adts(bytes: &[u8]) -> bool {
    // Con menos de 3 bytes no se puede ni mirar el byte de profile/sample rate: sin datos, se le da
    // el beneficio de la duda igual que a una cadena cortada por falta de buffer.
    if bytes.len() < 3 {
        return true;
    }
    let b2 = bytes[2];
    if (b2 >> 6) & 0x03 == 0x03 || (b2 >> 2) & 0x0F > 12 {
        return false;
    }
    let chain = walk_audio_frames(bytes, AudioStream::Adts, AUDIO_MIN_CHAIN_FRAMES, None);
    chain_is_credible(&chain)
}

/// Tamaño de la etiqueta ID3v2 que abre un MP3 (header de 10 bytes + cuerpo, más el footer opcional
/// de otros 10). El tamaño viene "synchsafe": 4 bytes de los que solo cuentan los 7 bits bajos, para
/// que nunca se parezca a un syncword de audio.
pub fn id3v2_tag_size(bytes: &[u8]) -> Option<usize> {
    let h = bytes.get(..10)?;
    if &h[..3] != b"ID3" {
        return None;
    }
    // Un bit alto prendido en el tamaño significa que esto no es un ID3v2 bien formado.
    if h[6..10].iter().any(|b| b & 0x80 != 0) {
        return None;
    }
    let size = h[6..10]
        .iter()
        .fold(0usize, |acc, b| (acc << 7) | (*b as usize & 0x7F));
    let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + size + footer)
}

/// Tamaño real de un MP3 que empieza en el inicio de `bytes`: la etiqueta ID3v2 inicial (si la hay)
/// más todos los frames MPEG encadenados.
///
/// Existe porque MP3 no tiene footer: sin esto, TODO candidato —real o falso— se carveaba hasta
/// `max_size` (50 MB). Eso inflaba el resultado a lo bestia y, encima, dejaba a los MP3 de verdad
/// con 50 MB de relleno pegado atrás. Devuelve `None` si la cadena se corta por falta de buffer (no
/// se puede afirmar dónde termina) o si es demasiado corta para creerle.
///
/// La etiqueta ID3v1 del final (128 bytes de metadatos) queda afuera a propósito: no es audio y su
/// ausencia no afecta la reproducción.
pub fn mp3_stream_end(bytes: &[u8], at_source_end: bool) -> Option<usize> {
    let tag = id3v2_tag_size(bytes).unwrap_or(0);
    if tag >= bytes.len() {
        return None;
    }
    let chain = walk_audio_frames(&bytes[tag..], AudioStream::Mpeg, usize::MAX, None);
    Some(tag + chain_size(&chain, at_source_end)?)
}

/// Tamaño real de un AAC (ADTS) que empieza en el inicio de `bytes`: la suma de los largos que
/// declaran sus propios frames. Mismo motivo que `mp3_stream_end`.
pub fn aac_stream_end(bytes: &[u8], at_source_end: bool) -> Option<usize> {
    let chain = walk_audio_frames(bytes, AudioStream::Adts, usize::MAX, None);
    chain_size(&chain, at_source_end)
}

/// HEIC/HEIF y MP4 comparten la misma caja contenedora `ftyp` (ISOBMFF): la única forma de
/// distinguirlos es leer el `major_brand` de 4 bytes que sigue inmediatamente a "ftyp".
const HEIC_BRANDS: [&[u8; 4]; 10] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
];

/// `bytes` empieza en "ftyp" (offset del header MP4/HEIC), no en el inicio de la caja.
fn is_heic_brand(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let brand: &[u8; 4] = bytes[4..8].try_into().unwrap();
    HEIC_BRANDS.contains(&brand)
}

/// Valida que la caja `ftyp` sea HEIC/HEIF (major_brand conocido), no MP4 genérico.
fn validate_heic_ftyp(bytes: &[u8]) -> bool {
    is_heic_brand(bytes)
}

/// 3GP y M4A también son cajas `ftyp` (ISOBMFF) válidas y comparten el mismo header/offset
/// que "MP4/M4V" — sin esta exclusión, cualquier .3gp o .m4a real se carvea DOS veces (una
/// bajo su firma propia y otra, redundante, bajo "MP4/M4V"). `bytes` empieza en "ftyp", igual
/// que `is_heic_brand`.
/// El major_brand de 3GP siempre empieza con los 3 bytes "3gp" (el 4to byte es un dígito de
/// versión variable, ej. "3gp4", "3gp5", "3gp6"). El de M4A real es "M4A " (con espacio
/// final), pero acá se compara solo el prefijo de 3 bytes "M4A" — a propósito, para que esta
/// exclusión sea simétrica con lo que la propia firma "M4A" matchea (su `header` también
/// compara solo esos mismos 3 bytes del brand, sin el 4to byte/espacio; ver esa firma más
/// abajo). Si se comparara el brand completo de 4 bytes acá, un candidato con basura en el 4to
/// byte (ej. datos corruptos, no un M4A real) matchearía la firma "M4A" pero no esta
/// exclusión, y volvería a duplicarse bajo "MP4/M4V".
fn is_3gp_or_m4a_brand(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let brand = &bytes[4..8];
    &brand[0..3] == b"3gp" || &brand[0..3] == b"M4A"
}

/// Valida que la caja `ftyp` NO sea HEIC/HEIF ni 3GP/M4A, para que "MP4/M4V" no duplique la
/// detección de fotos HEIC ni de videos/audios 3GP/M4A (mismo header `ftyp`, mismo offset).
fn validate_mp4_generic_ftyp(bytes: &[u8]) -> bool {
    !is_heic_brand(bytes) && !is_3gp_or_m4a_brand(bytes)
}

/// Valida un header BMP (`BITMAPFILEHEADER`) tras el magic "BM" de 2 bytes, para descartar los
/// falsos positivos masivos que produce ese header corto en datos de alta entropía (mismo
/// problema que `validate_mp3_sync`/`validate_aac_adts`, agravado acá porque `size_from_header`
/// lee 4 bytes de basura como tamaño total del archivo si no se valida nada). `bytes` empieza
/// en "BM" (offset 0 del header). Estructura real (little-endian):
/// - offset 0-1: "BM"
/// - offset 2-5 (u32): tamaño total del archivo (`bfSize`)
/// - offset 6-9: reservado1 + reservado2 (no se usan acá)
/// - offset 10-13 (u32): offset a los datos de píxeles (`bfOffBits`)
///
/// Se verifica que `bfSize` sea mayor al tamaño mínimo de header (14 bytes) y no exceda
/// `max_bmp_size` (el `max_size` de la firma), y que `bfOffBits` sea mayor a 14 y no mayor que
/// `bfSize`. Ambos campos son estructuralmente coherentes en cualquier BMP real, así que este
/// chequeo filtra la enorme mayoría de coincidencias casuales de "BM" en datos aleatorios sin
/// arriesgar rechazar BMPs válidos.
fn validate_bmp_header(bytes: &[u8], max_bmp_size: usize) -> bool {
    if bytes.len() < 14 {
        return false;
    }
    let file_size = u32::from_le_bytes(bytes[2..6].try_into().unwrap()) as usize;
    let pixel_offset = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;

    if file_size <= 14 || file_size > max_bmp_size {
        return false;
    }
    if pixel_offset <= 14 || pixel_offset > file_size {
        return false;
    }
    true
}

/// Adaptador para `FileSignature::validator` (firma `fn(&[u8]) -> bool`): fija `max_bmp_size`
/// al `max_size` de la firma "BMP" (50 MB), ver `validate_bmp_header`.
fn validate_bmp_default_max(bytes: &[u8]) -> bool {
    validate_bmp_header(bytes, 50 * 1024 * 1024)
}

/// Canon CR2 es TIFF little-endian ("II*\0") con un marcador propio "CR\x02\x00" en offset 8
/// (justo despues del puntero a IFD0 del header TIFF estandar).
/// `bytes` empieza en el header TIFF ("II*\0"), no en un offset separado.
fn is_cr2_marker(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    &bytes[8..12] == b"CR\x02\x00"
}

/// Valida que un header "II*\0" sea TIFF genuino y no un CR2 (ver "CR2 (Canon RAW)" abajo),
/// para no duplicar la deteccion del mismo header bajo dos firmas distintas.
fn validate_tiff_le_not_cr2(bytes: &[u8]) -> bool {
    !is_cr2_marker(bytes)
}

// ── Documentos de Office modernos (DOCX/XLSX/PPTX) = paquetes OOXML = archivos ZIP ──
// Comparten el header `PK\x03\x04` con CUALQUIER zip/jar/epub, y ese header se repite en CADA
// entrada interna del paquete. La detección NO usa `extra_check`: el nombre de la primera entrada
// varía según el productor (MS Office abre con `[Content_Types].xml`, LibreOffice con `_rels/.rels`
// — verificado con un .docx real de cada uno), así que un patrón fijo en un offset fijo dejaría
// afuera a LibreOffice. En su lugar el `validator` (`ooxml_has_part`) valida el EOCD del zip —que
// estructuralmente solo cierra en el INICIO real del archivo, no en las entradas internas— y busca
// el marcador de tipo (word/xl/ppt) dentro del propio archivo. El TAMAÑO sale del mismo EOCD (ver
// `zip_local_file_end` y `scanner::zip_ooxml_size`).

/// Dado un buffer que ARRANCA en el header local de un zip (`PK\x03\x04`), devuelve el largo total
/// del archivo zip ubicando su registro End Of Central Directory (`PK\x05\x06`), validado
/// estructuralmente: el offset + tamaño del directorio central (campos del propio EOCD, relativos
/// al inicio del zip = 0 acá) tienen que caer EXACTAMENTE donde arranca el EOCD. Eso hace dos cosas:
///   (a) confirma que es el EOCD REAL de este archivo, no una coincidencia de esos 4 bytes en datos
///       comprimidos, y
///   (b) evita agarrar el EOCD de OTRO zip que caiga más adelante en el buffer: sus offsets serían
///       relativos a SU inicio, no a 0, así que la ecuación no daría.
/// Devuelve `None` si no hay un EOCD válido dentro del buffer (archivo grande cuyo fin cae fuera, o
/// un ZIP64, cuyos campos van en 0xFFFFFFFF y no cierran la ecuación). Lo usan el `validator` (para
/// acotar la búsqueda del marcador de tipo al propio archivo) y el scanner (para el tamaño).
pub fn zip_local_file_end(bytes: &[u8]) -> Option<usize> {
    const EOCD: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    let mut p = 0usize;
    while p + 22 <= bytes.len() {
        if bytes[p..p + 4] == EOCD {
            let cd_size =
                u32::from_le_bytes([bytes[p + 12], bytes[p + 13], bytes[p + 14], bytes[p + 15]])
                    as usize;
            let cd_offset =
                u32::from_le_bytes([bytes[p + 16], bytes[p + 17], bytes[p + 18], bytes[p + 19]])
                    as usize;
            if cd_offset.checked_add(cd_size) == Some(p) {
                let comment_len = u16::from_le_bytes([bytes[p + 20], bytes[p + 21]]) as usize;
                let end = p + 22 + comment_len;
                return if end <= bytes.len() { Some(end) } else { None };
            }
        }
        p += 1;
    }
    None
}

/// ¿El header local en `bytes` (offset 0) nombra una de las entradas que un paquete OOXML pone
/// PRIMERO? MS Office abre el zip con `[Content_Types].xml`; LibreOffice con `_rels/.rels`
/// (verificado con un .docx real de cada uno). Es un filtro BARATO (solo lee el nombre del header,
/// no barre nada) para no correr la validación cara del EOCD en cada una de las MUCHAS entradas
/// internas del zip, que tienen otros nombres (`word/document.xml`, `xl/worksheets/…`, etc.).
fn ooxml_local_name_is_start(bytes: &[u8]) -> bool {
    if bytes.len() < 30 {
        return false;
    }
    let name_len = u16::from_le_bytes([bytes[26], bytes[27]]) as usize;
    match bytes.get(30..30 + name_len) {
        Some(name) => name == b"[Content_Types].xml" || name == b"_rels/.rels",
        None => false,
    }
}

/// True si `bytes` (que arranca en un header local de zip) es el INICIO de un OOXML cuya parte
/// principal es `marker`. Tres pasos:
///  1. Filtro barato: el nombre de la entrada tiene que ser un arranque de OOXML
///     (`ooxml_local_name_is_start`); si no, es una entrada interna del zip → descartar.
///  2. `zip_local_file_end` valida el EOCD: confirma que este es el INICIO REAL del archivo (en una
///     entrada interna la ecuación del EOCD no cierra → devuelve None → descartar) y da el fin del
///     archivo, para acotar la búsqueda del marcador al PROPIO archivo. Sin acotar, la búsqueda
///     cruzaría al archivo siguiente del buffer y un docx "encontraría" el `xl/workbook.xml` del
///     xlsx de al lado, carveándose además como xlsx (falso positivo cruzado, visto en la
///     verificación real con 3 Office seguidos).
///  3. Buscar `marker` dentro de `[inicio, fin]`. Los tres marcadores (word/xl/ppt) son mutuamente
///     excluyentes → un archivo nunca matchea dos firmas → no se carvea dos veces.
///
/// Limitación v1: si el EOCD cae fuera del buffer del escaneo (archivo Office más grande que el
/// buffer), devuelve false → ese archivo no se detecta. La enorme mayoría de los documentos entran.
fn ooxml_has_part(bytes: &[u8], marker: &[u8]) -> bool {
    if !ooxml_local_name_is_start(bytes) {
        return false;
    }
    let end = match zip_local_file_end(bytes) {
        Some(e) => e,
        None => return false,
    };
    let hay = &bytes[..end.min(bytes.len())];
    hay.len() >= marker.len() && hay.windows(marker.len()).any(|w| w == marker)
}

/// DOCX: paquete OOXML con la parte principal `word/document.xml`.
fn validate_ooxml_docx(bytes: &[u8]) -> bool {
    ooxml_has_part(bytes, b"word/document.xml")
}

/// XLSX: paquete OOXML con la parte principal `xl/workbook.xml`.
fn validate_ooxml_xlsx(bytes: &[u8]) -> bool {
    ooxml_has_part(bytes, b"xl/workbook.xml")
}

/// PPTX: paquete OOXML con la parte principal `ppt/presentation.xml`.
fn validate_ooxml_pptx(bytes: &[u8]) -> bool {
    ooxml_has_part(bytes, b"ppt/presentation.xml")
}

/// Retorna todas las firmas conocidas
pub fn all_signatures() -> Vec<FileSignature> {
    vec![
        // ═══════════════════════ FOTOS ═══════════════════════
        FileSignature {
            name: "JPEG",
            extension: "jpg",
            category: FileCategory::Photo,
            header: &[0xFF, 0xD8, 0xFF],
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0xFF, 0xD9]),
            max_size: 25 * 1024 * 1024, // 25 MB
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "PNG",
            extension: "png",
            category: FileCategory::Photo,
            header: &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82]),
            max_size: 30 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "GIF",
            extension: "gif",
            category: FileCategory::Photo,
            header: &[0x47, 0x49, 0x46, 0x38],
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0x00, 0x3B]),
            max_size: 20 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "BMP",
            extension: "bmp",
            category: FileCategory::Photo,
            header: &[0x42, 0x4D],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            // Header de solo 2 bytes ("BM") sin esto genera falsos positivos masivos en datos
            // de alta entropía, agravado por size_from_header abajo (ver validate_bmp_header).
            validator: Some((validate_bmp_default_max, 14)),
            // BITMAPFILEHEADER: tamaño total del archivo en offset 2, 4 bytes little-endian.
            size_from_header: Some((2, 4)),
        },
        FileSignature {
            name: "WebP",
            extension: "webp",
            category: FileCategory::Photo,
            header: &[0x52, 0x49, 0x46, 0x46], // RIFF
            header_offset: 0,
            extra_check: Some((&[0x57, 0x45, 0x42, 0x50], 8)), // "WEBP" en offset 8
            footer: None,
            max_size: 25 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "CR2 (Canon RAW)",
            extension: "cr2",
            category: FileCategory::Photo,
            // Mismo header TIFF little-endian que "TIFF" abajo; se desambigua por el
            // marcador "CR\x02\x00" en offset 8.
            header: &[0x49, 0x49, 0x2A, 0x00],
            header_offset: 0,
            extra_check: Some((b"CR\x02\x00", 8)),
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "TIFF",
            extension: "tiff",
            category: FileCategory::Photo,
            header: &[0x49, 0x49, 0x2A, 0x00],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            // NEF (Nikon RAW) tambien es TIFF-based sin un marcador propio a offset fijo
            // (requeriria parsear tags IFD, ej. Make = "NIKON CORPORATION", fuera del alcance
            // de file carving por magic bytes); se recupera bajo esta firma generica como
            // .tiff, contenedor valido que preserva los datos aunque no distinga el fabricante.
            validator: Some((validate_tiff_le_not_cr2, 12)),
            size_from_header: None,
        },
        FileSignature {
            name: "TIFF (big-endian)",
            extension: "tiff",
            category: FileCategory::Photo,
            // Motorola byte order: "MM" + 0x002A (big-endian, vs "II" + 0x2A00 little-endian).
            header: &[0x4D, 0x4D, 0x00, 0x2A],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "HEIC/HEIF",
            extension: "heic",
            category: FileCategory::Photo,
            // Misma caja ftyp (ISOBMFF) que MP4; se desambigua por major_brand via validator.
            header: &[0x66, 0x74, 0x79, 0x70],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            validator: Some((validate_heic_ftyp, 8)),
            size_from_header: None,
        },
        // ═══════════════════════ VIDEOS ═══════════════════════
        FileSignature {
            name: "MP4/M4V",
            extension: "mp4",
            category: FileCategory::Video,
            // ftyp box
            header: &[0x66, 0x74, 0x79, 0x70],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 2 * 1024 * 1024 * 1024, // 2 GB
            // Excluye brands HEIC/HEIF (ver "HEIC/HEIF" arriba) y 3GP/M4A (ver esas firmas
            // abajo) para no duplicar la deteccion de la misma caja ftyp bajo dos firmas
            // distintas.
            validator: Some((validate_mp4_generic_ftyp, 8)),
            size_from_header: None,
        },
        FileSignature {
            name: "AVI",
            extension: "avi",
            category: FileCategory::Video,
            header: &[0x52, 0x49, 0x46, 0x46], // RIFF
            header_offset: 0,
            extra_check: Some((&[0x41, 0x56, 0x49, 0x20], 8)), // "AVI " en offset 8
            footer: None,
            max_size: 2 * 1024 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "MKV",
            extension: "mkv",
            category: FileCategory::Video,
            header: &[0x1A, 0x45, 0xDF, 0xA3],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 2 * 1024 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "FLV",
            extension: "flv",
            category: FileCategory::Video,
            header: &[0x46, 0x4C, 0x56, 0x01],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 1024 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        // Nota (M4): la firma "MOV" standalone (atomo "moov" en offset 4 desde cualquier
        // posición) fue eliminada — "moov" puede aparecer en cualquier parte del archivo
        // (normalmente al final), no solo al inicio, así que producía carvings inservibles
        // que empezaban 4 bytes antes de un átomo "moov" aleatorio. Los MOV reales (que
        // empiezan con "ftyp qt  ") ya se detectan vía la firma MP4/M4V de arriba.
        FileSignature {
            name: "3GP",
            extension: "3gp",
            category: FileCategory::Video,
            // ftyp3gp
            header: &[0x66, 0x74, 0x79, 0x70, 0x33, 0x67, 0x70],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 500 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        // ═══════════════════════ AUDIO ═══════════════════════
        FileSignature {
            name: "MP3 (ID3)",
            extension: "mp3",
            category: FileCategory::Audio,
            header: &[0x49, 0x44, 0x33],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "MP3 (Sync)",
            extension: "mp3",
            category: FileCategory::Audio,
            header: &[0xFF, 0xFB],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            // Header de solo 2 bytes: sin esto, datos de alta entropía generan cientos de
            // miles de falsos positivos carveados a max_size (ver C2).
            validator: Some((validate_mp3_sync, 3)),
            size_from_header: None,
        },
        FileSignature {
            name: "WAV",
            extension: "wav",
            category: FileCategory::Audio,
            header: &[0x52, 0x49, 0x46, 0x46], // RIFF
            header_offset: 0,
            extra_check: Some((&[0x57, 0x41, 0x56, 0x45], 8)), // "WAVE" en offset 8
            footer: None,
            max_size: 200 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "FLAC",
            extension: "flac",
            category: FileCategory::Audio,
            header: &[0x66, 0x4C, 0x61, 0x43],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 200 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "OGG Vorbis",
            extension: "ogg",
            category: FileCategory::Audio,
            header: &[0x4F, 0x67, 0x67, 0x53], // OggS
            header_offset: 0,
            // "\x01vorbis" en offset 28 (tras cabecera de página OGG con 1 segmento)
            extra_check: Some((&[0x01, 0x76, 0x6F, 0x72, 0x62, 0x69, 0x73], 28)),
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "AAC",
            extension: "aac",
            category: FileCategory::Audio,
            header: &[0xFF, 0xF1],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            // Header de solo 2 bytes (syncword ADTS real es de 12 bits): sin esto, datos de
            // alta entropía generan cientos de miles de falsos positivos (ver C2).
            validator: Some((validate_aac_adts, 3)),
            size_from_header: None,
        },
        FileSignature {
            name: "M4A",
            extension: "m4a",
            category: FileCategory::Audio,
            header: &[0x66, 0x74, 0x79, 0x70, 0x4D, 0x34, 0x41],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "WMA",
            extension: "wma",
            category: FileCategory::Audio,
            header: &[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "AMR (Nota de voz)",
            extension: "amr",
            category: FileCategory::Audio,
            header: &[0x23, 0x21, 0x41, 0x4D, 0x52],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 20 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "OPUS",
            extension: "opus",
            category: FileCategory::Audio,
            header: &[0x4F, 0x67, 0x67, 0x53], // OggS
            header_offset: 0,
            // "OpusHead" en offset 28 (tras cabecera de página OGG con 1 segmento)
            extra_check: Some((&[0x4F, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64], 28)),
            footer: None,
            max_size: 50 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        // ═══════════════════════ DOCUMENTOS ═══════════════════════
        FileSignature {
            name: "PDF",
            extension: "pdf",
            category: FileCategory::Document,
            header: &[0x25, 0x50, 0x44, 0x46, 0x2D], // "%PDF-"
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0x25, 0x25, 0x45, 0x4F, 0x46]), // "%%EOF"
            max_size: 200 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        // ── Office moderno (OOXML = ZIP). Ver los validators `validate_ooxml_*` arriba: el
        //    `validator` valida el EOCD del zip (que estructuralmente solo cierra en el arranque
        //    REAL del archivo, no en las entradas internas que también empiezan con PK0304) y
        //    distingue el tipo. El TAMAÑO tampoco sale del header ni de un footer fijo: lo calcula
        //    `scanner::zip_ooxml_size` con el mismo EOCD. Sin `extra_check`: el nombre de la primera
        //    entrada varía entre productores (MS Office vs LibreOffice). ──
        FileSignature {
            name: "Word (DOCX)",
            extension: "docx",
            category: FileCategory::Document,
            header: &[0x50, 0x4B, 0x03, 0x04], // "PK\x03\x04" (header local de zip)
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024, // 100 MB (documentos con imágenes embebidas)
            validator: Some((validate_ooxml_docx, 30)),
            size_from_header: None,
        },
        FileSignature {
            name: "Excel (XLSX)",
            extension: "xlsx",
            category: FileCategory::Document,
            header: &[0x50, 0x4B, 0x03, 0x04],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: Some((validate_ooxml_xlsx, 30)),
            size_from_header: None,
        },
        FileSignature {
            name: "PowerPoint (PPTX)",
            extension: "pptx",
            category: FileCategory::Document,
            header: &[0x50, 0x4B, 0x03, 0x04],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: Some((validate_ooxml_pptx, 30)),
            size_from_header: None,
        },
    ]
}

/// Filtra firmas por categoría
pub fn signatures_for_categories(categories: &[FileCategory]) -> Vec<FileSignature> {
    all_signatures()
        .into_iter()
        .filter(|sig| categories.contains(&sig.category))
        .collect()
}
