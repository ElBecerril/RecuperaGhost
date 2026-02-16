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

## Formatos soportados (21)

| Categoria | Formatos |
|-----------|----------|
| Fotos (6) | JPEG, PNG, GIF, BMP, WebP, TIFF |
| Videos (6) | MP4/M4V, AVI, MKV, FLV, MOV, 3GP |
| Audio (9) | MP3, WAV, FLAC, OGG Vorbis, AAC, M4A, WMA, AMR, OPUS |

## Instalacion

### Requisitos

- [Rust](https://rustup.rs/) 1.70+

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

Presenta un menu donde puedes:
1. Seleccionar una imagen de disco o dispositivo
2. Elegir que tipos de archivo buscar (fotos, videos, audio)
3. Escanear con barra de progreso en tiempo real
4. Recuperar los archivos encontrados a una carpeta organizada

### Modo batch (CLI)

```bash
# Buscar todo en una imagen de disco
./recupe_ghost disco.img

# Solo fotos, salida personalizada
./recupe_ghost disco.img --fotos -o recuperados

# Solo videos y audio
./recupe_ghost /dev/sdb1 --videos --audio

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
  fotos/    -> recovered_0001.jpg, recovered_0002.png, ...
  videos/   -> recovered_0003.mp4, recovered_0004.avi, ...
  audios/   -> recovered_0005.mp3, recovered_0006.wav, ...
```

## Como funciona

RecupeGhost escanea byte por byte buscando **firmas de archivo** (magic bytes) en el disco:

1. Lee el disco en bloques de 1 MB con overlap para no perder firmas en fronteras
2. Cuando encuentra una cabecera conocida, valida con verificaciones adicionales para desambiguar formatos que comparten firma (ej. RIFF -> WebP vs AVI vs WAV)
3. Busca el footer del archivo (si existe) o usa el tamano maximo del formato
4. Extrae los bytes y los guarda organizados por categoria

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
  signatures/mod.rs    -> 21 firmas de archivo (magic bytes, extra_check, footer)
  scanner/mod.rs       -> Motor de escaneo por file carving + 5 tests
  recovery/mod.rs      -> Extraccion de archivos a carpetas organizadas
  ui/mod.rs            -> Menus interactivos con dialoguer
  updater.rs           -> Sistema de auto-actualizacion via GitHub Releases
```

## Tests

```bash
cargo test
```

10 tests automatizados:
- Deteccion de firmas principales
- Desambiguacion RIFF (WebP vs AVI vs WAV)
- Desambiguacion OGG Vorbis vs OPUS
- Deteccion de footer JPEG
- Flujo completo de recuperacion
- Parseo de versiones (con y sin prefijo "v")
- Parseo de versiones invalidas
- Comparacion de versiones (is_newer)
- Busqueda de asset por plataforma

## Licencia

MIT

## Autor

**El_Becerril** - [YouTube](https://www.youtube.com/@el_becerril)
