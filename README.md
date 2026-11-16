# XHomeAlone DL

Descargador independiente para catálogos por etiqueta de XHomeAlone. Pega enlaces como `XXX`; el programa lista los posts y descarga el MP4 declarado por su reproductor.

Para descargar, necesita Chrome o Edge instalado: abre una sesión temporal aislada para obtener el enlace MP4 vigente del reproductor antes de cada transferencia.

Los archivos se organizan bajo una carpeta identificada por la etiqueta. Los enlaces se aceptan únicamente cuando su host, identificador de vídeo y ruta corresponden al post solicitado.

## Compilar

Requiere Rust estable y herramientas de compilación MSVC en Windows.

```powershell
cargo build --release --locked
```

El binario es `xhomealone-dl.exe`. Los registros y transferencias locales quedan en `downloads/`, excluido de Git.

## Verificar

```powershell
cargo test --locked --offline
cargo fmt --check
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
```

## Licencia y uso responsable

El código propio se distribuye bajo la [licencia MIT](LICENSE). El propósito educativo y las responsabilidades de uso se describen en [USAGE_NOTICE.md](USAGE_NOTICE.md). Esta licencia no concede derechos sobre contenido ajeno.
