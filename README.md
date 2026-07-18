# RecupeGhost - El Detective de Archivos Perdidos

```
             ▄▄██████▄▄
          ▄▄████████████▄▄
       ██████████████████████
       █████  ████████  █████
       █████  ████████  █████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████  ██████  ██████
        ▀▀▀▀    ▀▀▀▀    ▀▀▀▀
            👻 RecupeGhost
```

Herramienta CLI de recuperacion de archivos multimedia borrados, escrita en Rust. Utiliza **file carving** para buscar firmas (magic bytes) directamente en discos o imagenes raw, sin depender del sistema de archivos.

## Formatos soportados (25)

| Categoria | Formatos |
|-----------|----------|
| Fotos (9) | JPEG, PNG, GIF, BMP, WebP, TIFF (little-endian y big-endian), HEIC/HEIF, CR2 (Canon RAW) |
| Videos (6) | MP4/M4V, AVI, MKV, FLV, MOV, 3GP |
| Audio (9) | MP3, WAV, FLAC, OGG Vorbis, AAC, M4A, WMA, AMR, OPUS |
| Documentos (1) | PDF |

> **Nikon NEF:** no tiene un marcador propio a offset fijo (es TIFF-based sin firma
> distintiva salvo parseando tags IFD como `Make`), asi que un NEF se recupera bajo la firma
> generica "TIFF" — el contenedor es valido y los datos se preservan, solo queda con
> extension `.tiff` en vez de `.nef`.

## Instalacion

### Requisitos

- [Rust](https://rustup.rs/) 1.74+

### Compilar desde fuente

```bash
git clone https://github.com/ElBecerril/RecuperaGhost.git
cd RecuperaGhost
cargo build --release
```

El binario queda en `target/release/recupe_ghost.exe` (Windows) o `target/release/recupe_ghost` (Linux/macOS).

## Uso

### Modo interactivo

```bash
./recupe_ghost
```

Presenta un menu inteligente donde puedes:
1. **Seleccionar origen con deteccion automatica:**
   - Memorias USB / discos externos (auto-detectados)
   - Disco interno / ver todos los discos del sistema (incluye el disco de la PC)
   - Archivos de imagen (.img, .dd, .raw) encontrados en el directorio actual
   - Ruta manual para usuarios avanzados
   - En cualquier prompt de ruta escrita a mano, dejar el campo vacio y presionar Enter cancela y vuelve al menu anterior
2. Elegir que tipos de archivo buscar (fotos, videos, audio, documentos)
3. Escanear con barra de progreso en tiempo real
4. Recuperar los archivos encontrados a una carpeta organizada

Los resultados se **ordenan y marcan por integridad** para que sepas cuales son confiables:
`✅ integro` (se encontro el final real del archivo), `⚠️ posiblemente danado` (quedo truncado,
probable falso positivo o archivo incompleto), y sin marca (`no se pudo verificar`, formatos sin
un final detectable). **No se oculta nada**: podes recuperar todos igual.

Tambien podes **clonar un disco que esta fallando a un archivo de imagen** antes de escanear
(ver abajo).

### Clonar un disco que esta fallando (recomendado para discos con problemas)

Si el disco o memoria esta fallando, lo mas seguro es copiarlo entero a un archivo de imagen
**antes** de buscar nada, y despues escanear esa copia. Asi no se estresa el disco enfermo (cada
lectura extra puede acelerar su muerte). En el menu principal:

```
📀 Clonar un disco que esta fallando (copiarlo a una imagen primero)
```

- Copia byte a byte a un `.img`. Los sectores que no se puedan leer se **saltan** (se rellenan
  con ceros y se registran): un solo sector danado no aborta la copia.
- Muestra barra de progreso y se puede **cancelar con Ctrl+C** en cualquier momento; la copia
  parcial queda guardada y se puede escanear igual.
- Al terminar, ofrece escanear la imagen recien creada directamente.
- **La imagen se guarda en un disco distinto al que se clona** (avisa si detecta que es el mismo).

### Modo batch (CLI)

```bash
# Buscar todo en una imagen de disco
./recupe_ghost disco.img

# Solo fotos, salida personalizada
./recupe_ghost disco.img --fotos -o recuperados

# Solo videos y audio
./recupe_ghost /dev/sdb1 --videos --audio

# Solo documentos (PDF)
./recupe_ghost disco.img --documentos

# Ayuda
./recupe_ghost --help
```

### Entradas soportadas

- Imagenes de disco: `.img`, `.dd`, `.raw`
- Dispositivos Windows: `\\.\PhysicalDrive1`
- Dispositivos Linux/macOS: `/dev/sdb1`

### Salida

Los archivos se organizan automaticamente:

```
RecupeGhost_20260216_143022/
  fotos/       -> recovered_0001.jpg, recovered_0002.png, ...
  videos/      -> recovered_0003.mp4, recovered_0004.avi, ...
  audios/      -> recovered_0005.mp3, recovered_0006.wav, ...
  documentos/  -> recovered_0006.pdf, ...
```

## Como funciona

RecupeGhost escanea byte por byte buscando **firmas de archivo** (magic bytes) en el disco:

1. Lee el disco en bloques de 1 MB con overlap para no perder firmas en fronteras
2. Cuando encuentra una cabecera conocida, valida con verificaciones adicionales para desambiguar formatos que comparten firma (ej. RIFF -> WebP vs AVI vs WAV)
3. Busca el footer del archivo dentro del buffer actual (sin seeks extra) o usa el tamano maximo del formato
4. Extrae los bytes y los guarda organizados por categoria

**Escaneo multi-hilo inteligente:**
- **Discos fisicos (USB/HDD):** 1 hilo, 100% secuencial (sin seeks aleatorios), eficiente incluso en memorias USB lentas
- **Imagenes de disco (SSD/NVMe):** auto-detecta CPU cores y divide el archivo en segmentos paralelos (hasta 8 hilos), acelerando el escaneo 2-6x
- Overlap entre segmentos garantiza que ninguna firma se pierda en fronteras
- Barra de progreso y tiempo estimado en tiempo real
- **Cancelable con Ctrl+C:** cortar un escaneo largo conserva todo lo encontrado hasta ese momento (podes recuperarlo igual). La cancelacion es cooperativa: no interrumpe una lectura ya colgada en el kernel (ej. un dispositivo que se desconecto), solo evita empezar el siguiente bloque

**Compatibilidad con discos fisicos (Windows):**
- Detecta automaticamente memorias USB y discos externos
- Obtiene el tamano del disco via `IOCTL_DISK_GET_LENGTH_INFO`
- Lecturas alineadas a 512 bytes (sector size) como requiere Windows
- Requiere ejecucion como Administrador para acceder a discos fisicos

## Auto-actualizacion

RecupeGhost verifica automaticamente si hay una nueva version disponible en GitHub Releases al iniciar:

1. Consulta la API de GitHub para obtener la ultima release
2. Compara la version con la actual usando versionado semantico
3. Si hay una nueva version, muestra un aviso y pregunta si deseas actualizar
4. Descarga el nuevo binario con barra de progreso y reemplaza el ejecutable
5. Si no hay internet o falla algo, continua normalmente sin bloquear

La actualizacion funciona en Windows, Linux y macOS. En Windows usa la tecnica de renombrar el .exe en ejecucion para poder sobreescribirlo.

## Arquitectura

```
src/
  main.rs              -> Punto de entrada, CLI con clap, modo interactivo/batch
  banner.rs            -> Banner ASCII y branding
  drives.rs            -> Deteccion de discos por plataforma (Windows/Linux/macOS)
  signatures/mod.rs    -> 25 firmas de archivo (magic bytes, extra_check, footer)
  scanner/mod.rs       -> Motor de escaneo multi-hilo por file carving + IOCTL + tests
  clone/mod.rs         -> Clonado de disco a imagen .img tolerante a sectores danados
  recovery/mod.rs      -> Extraccion de archivos a carpetas organizadas
  ui/mod.rs            -> Menus interactivos con seleccion inteligente de origen
  updater.rs           -> Sistema de auto-actualizacion via GitHub Releases
```

## Tests

```bash
cargo test
```

47 tests automatizados:
- Deteccion de las 10 firmas principales
- Desambiguacion RIFF (WebP vs AVI vs WAV)
- Desambiguacion OGG Vorbis vs OPUS
- Deteccion de TIFF big-endian (Motorola byte order)
- Desambiguacion HEIC/HEIF vs MP4 (misma caja ftyp, distinto major_brand, incluye brands hevm/hevs)
- Desambiguacion CR2 vs TIFF generico (mismo header, distinto marcador)
- Desambiguacion 3GP/M4A vs MP4 (misma caja ftyp)
- Validador BMP (BITMAPFILEHEADER coherente vs datos aleatorios)
- Deteccion de PDF (header %PDF- y footer %%EOF)
- Recuperacion truncada vs completa (RecoveryResult distingue ambos casos)
- Deteccion de footer JPEG (incluye fotos con thumbnail EXIF embebido)
- Flujo completo de recuperacion
- Calculo de segmentos para escaneo paralelo
- Seleccion automatica de hilos (dispositivo vs archivo)
- Consistencia multi-hilo (1 vs N hilos, todas las categorias)
- Deteccion de firmas en frontera de segmento
- No-englobing de archivos adyacentes en el mismo buffer
- Rechazo de falsos positivos MP3/AAC via frame-chaining
- Parseo de versiones y busqueda de assets (updater)
- Deteccion del disco de sistema (Windows C: / raiz Unix) para avisos de UI
- Cancelacion cooperativa del escaneo (corta antes de leer y conserva lo hallado)
- Normalizacion de particion a disco completo (sd/nvme/mmcblk/nbd/loop/macOS), sin mal-normalizar discos que terminan en digito
- same_device_warning no advierte cuando el origen es un archivo de imagen (no un disco fisico)
- Clonado de disco a imagen: copia byte a byte exacta (ida y vuelta, multi-bloque)
- Clonado cancelable: corta y conserva la copia parcial
- Clasificacion de integridad de resultados (integro / posiblemente danado / no verificable) y orden de presentacion

## Contribuir

Las contribuciones son bienvenidas. Consulta [CONTRIBUTING.md](CONTRIBUTING.md) para la guia de contribucion.

## Licencia

Este proyecto esta licenciado bajo la **GNU General Public License v3.0**. Consulta el archivo [LICENSE](LICENSE) para los detalles completos.

## Autor

**El_Becerril** - [YouTube](https://www.youtube.com/@el_becerril)
