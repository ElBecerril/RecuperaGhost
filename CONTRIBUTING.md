# Contribuir a RecupeGhost

Gracias por tu interes en contribuir a RecupeGhost. Esta guia te ayudara a comenzar.

## Requisitos de desarrollo

- [Rust](https://rustup.rs/) 1.70+
- En Windows con toolchain GNU: agregar MinGW al PATH antes de compilar

## Configurar el entorno

```bash
git clone https://github.com/ElBecerril/RecuperaGhost.git
cd RecuperaGhost
cargo build
cargo test
```

## Reportar bugs

Abre un [issue](https://github.com/ElBecerril/RecuperaGhost/issues) con:

1. Descripcion del problema
2. Pasos para reproducirlo
3. Comportamiento esperado vs actual
4. Sistema operativo y version de Rust (`rustc --version`)
5. Si es un problema de escaneo: tipo de disco/imagen y tamano

## Proponer cambios

1. Haz fork del repositorio
2. Crea una rama descriptiva (`git checkout -b feature/nuevo-formato-heic`)
3. Haz tus cambios
4. Asegurate de que todos los tests pasen: `cargo test`
5. Verifica que no haya warnings: `cargo build --release`
6. Haz commit con mensajes claros en espanol o ingles
7. Abre un Pull Request describiendo los cambios

## Guia de estilo

- Seguir las convenciones idiomaticas de Rust (`cargo clippy`)
- Comentarios y mensajes de usuario en espanol
- Nombres de variables y funciones en ingles (snake_case)
- Agregar tests para funcionalidad nueva
- No agregar dependencias externas sin justificacion

## Agregar un nuevo formato de archivo

Para agregar soporte a un nuevo formato multimedia:

1. Agrega la firma en `src/signatures/mod.rs` con:
   - `header`: magic bytes de la cabecera
   - `header_offset`: offset desde el inicio del archivo (0 para la mayoria)
   - `extra_check`: bytes adicionales para desambiguar si comparte header con otros formatos
   - `footer`: bytes de cierre (opcional, mejora la precision del tamano)
   - `max_size`: tamano maximo razonable del formato
2. Agrega el formato a la tabla de `README.md`
3. Agrega un caso de prueba en `create_test_image()` en `src/scanner/mod.rs`
4. Ejecuta `cargo test` para verificar deteccion correcta

## Estructura del proyecto

```
src/
  main.rs              -> Punto de entrada, CLI, modo interactivo/batch
  banner.rs            -> Banner ASCII y branding
  drives.rs            -> Deteccion de discos (Windows/Linux/macOS)
  signatures/mod.rs    -> Firmas de archivo (magic bytes)
  scanner/mod.rs       -> Motor de escaneo multi-hilo + tests
  recovery/mod.rs      -> Extraccion de archivos recuperados
  ui/mod.rs            -> Menus interactivos
  updater.rs           -> Auto-actualizacion via GitHub Releases
```

## Licencia

Al contribuir a este proyecto, aceptas que tus contribuciones se licencien bajo la misma licencia GPL-3.0 del proyecto.
