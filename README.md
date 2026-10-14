# lyrics

Visualizador de letras en la terminal. Sigue la canción que suena en Spotify, la muestra sincronizada y dibuja el espectro del audio del sistema detrás del texto.

El proyecto está en pausa. Lo que hay funciona; no hay trabajo activo encima.

## Qué hace

- Consulta la reproducción actual de Spotify y avanza la posición con el reloj local entre consultas.
- Busca la letra en [lrclib](https://lrclib.net) y la guarda en caché. Si viene con tiempos, la línea actual sigue la canción. Si no, muestra el texto plano.
- Captura el monitor de audio por defecto (PulseAudio o PipeWire) y dibuja barras con una FFT.
- En Kitty las barras van en una capa de píxeles detrás del texto. En otras terminales se dibujan como bloques de celda.
- Puede traducir la letra con DeepL y alternar entre original y traducción.
- `stats` lista el top 10 de Spotify, el país y el plan de la cuenta.

El espectro es el audio que sale del sistema, no un stream de Spotify. Si Spotify no es lo que está sonando, las barras y la letra no coinciden.

## Requisitos

- Linux con PulseAudio o PipeWire, y el monitor por defecto disponible (`@DEFAULT_MONITOR@`).
- `pkg-config` y las cabeceras de libpulse. En Fedora: `pulseaudio-libs-devel`.
- Rust reciente (el crate usa edition 2024).
- Kitty, si se quieren las barras en píxeles. El resto de terminales también sirve.

## Compilar

```sh
cargo build --release
```

El binario queda en `target/release/lyrics`.

## Spotify

Hay que crear una app en [developer.spotify.com](https://developer.spotify.com/dashboard) y registrar esta Redirect URI:

```text
http://127.0.0.1:8888/callback
```

Luego:

```sh
lyrics login <client_id> <client_secret>
```

Abre el navegador, autoriza la app y guarda la sesión. El puerto por defecto es 8888; se puede pasar otro como tercer argumento, y la Redirect URI tiene que coincidir.

Permisos que pide: lectura de la reproducción actual, del estado del reproductor, del top de canciones y del perfil.

La sesión se guarda en `$XDG_STATE_HOME/lyrics/spotify-auth.json` (si no existe esa variable, `~/.local/state/lyrics/`). Si ya hay una sesión de [sptlrx](https://github.com/raitonoberu/sptlrx), la reutiliza la primera vez.

## Uso

```sh
lyrics                         # letras de Spotify con el espectro detrás
lyrics --demo                  # audio y letra de prueba, sin Spotify
lyrics --translate es          # traduce la letra (vale junto con --demo)
lyrics deepl <clave>           # guarda la clave de DeepL
lyrics stats [corto|mediano|largo]
```

`stats` muestra el nombre, el país, el plan y el top 10. Sin argumento usa el periodo mediano: últimas 4 semanas (`corto`), últimos 6 meses (`mediano`) o cerca de un año (`largo`).

### Traducción

La clave puede ir en `DEEPL_API_KEY` o guardarse con `lyrics deepl`. El archivo queda en el mismo directorio de estado, como `deepl-key`, con permisos `0600`. Una clave que termina en `:fx` usa la API gratuita; el resto, la de pago.

Idiomas: `es`, `es-latam`, `en`, `en-gb`, `pt`, `pt-pt`, `fr`, `fr-ca`, `de`, `de-ch`, `it`, `ja`, `ko`, `zh`, `zh-hant`, `ru`. También acepta nombres (`spanish`, `español`, `english`, …).

La letra original no se toca. La traducción se guarda aparte, con los mismos tiempos, y solo se vuelve a pedir si cambia el texto.

### Teclas

| Tecla | Acción |
| --- | --- |
| `q`, `Esc`, `Ctrl+C` | salir |
| flechas, `j`/`k`, rueda | recorrer la letra en pausa (al dar play vuelve sola a la línea que suena) |
| RePág / AvPág | media pantalla |
| Inicio | volver a la línea que suena |
| espacio | pausa (solo en `--demo`; en vivo la pausa es la de Spotify) |
| `t` | original o traducción, si se pasó `--translate` |

## Dónde quedan los datos

| Qué | Dónde |
| --- | --- |
| Sesión de Spotify y clave de DeepL | `$XDG_STATE_HOME/lyrics` o `~/.local/state/lyrics` |
| Letras en caché (`.lyr`) | `$XDG_CACHE_HOME/lyrics` o `~/.cache/lyrics` |

## Cómo está armado

Cada módulo es un hilo o una pieza del dibujo, y `main` solo los junta en el bucle de frames.

| Módulo | Rol |
| --- | --- |
| `spotify` | OAuth, reproducción actual y estadísticas |
| `player` | Hilo que consulta Spotify y avanza la posición |
| `lyrics` | Búsqueda en lrclib y lectura de la caché |
| `lyr` | Formato binario de la caché de letras |
| `translate` | Cola de traducción con DeepL |
| `audio` | Captura del monitor con libpulse |
| `spectrum` | FFT, bandas logarítmicas y caída de las barras |
| `render` | Diferencias de celdas y salida ANSI |
| `scene` | Letra, tema de color y barras de celda |
| `kitty` | Barras en píxeles con el protocolo gráfico de Kitty |
