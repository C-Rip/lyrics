# lyrics

Visualizador de letras en la terminal. Sigue la canción que suena en el reproductor local, la muestra sincronizada y dibuja el espectro del audio del sistema detrás del texto.

## Qué hace

- Por defecto lee el reproductor por MPRIS2 en el bus de sesión: Spotify de escritorio, mpv, VLC, Strawberry, Rhythmbox, Audacious, cmus, o un navegador con YouTube.
- `--api` usa la API web de Spotify, para cuando la reproducción no está en esta máquina.
- Busca la letra en [lrclib](https://lrclib.net) y la guarda en caché. Si viene con tiempos y la duración cuadra, la línea actual sigue la canción. Si no, muestra el texto plano.
- En un navegador limpia el título de YouTube o YouTube Music antes de buscar: separa artista y canción, quita el canal y descarta el ruido del videoclip.
- Captura el monitor de audio por defecto (PulseAudio o PipeWire) y dibuja barras con una FFT. El espectro es el audio que sale del sistema, no el stream del reproductor.
- Al arrancar pregunta a la terminal si carga una imagen por memoria compartida, con el protocolo gráfico de Kitty. Si responde, las barras van en una capa de píxeles detrás del texto. Si no, se dibujan como bloques de celda.
- Puede traducir la letra con DeepL y alternar entre original y traducción.
- `login` y `stats` hablan con la API de Spotify: el top 10, el país y el plan de la cuenta.

## Requisitos

- Linux con sesión D-Bus, PulseAudio o PipeWire, y el monitor por defecto disponible (`@DEFAULT_MONITOR@`).
- `pkg-config` y las cabeceras de libpulse. En Fedora: `pulseaudio-libs-devel`.
- Rust reciente (el crate usa edition 2024).
- Una terminal que responda al protocolo gráfico de Kitty con memoria compartida, si se quieren las barras en píxeles. El resto de terminales dibuja las barras con bloques.
- Una app de Spotify en el dashboard, solo para `--api`, `login` y `stats`.

## Compilar

```sh
cargo build --release
```

El binario queda en `target/release/lyrics`.

## Uso

```sh
lyrics                          # reproductor local, con el espectro detrás
lyrics --api                    # fuerza la API web de Spotify
lyrics --demo                   # audio y letra de prueba, sin reproductor
lyrics --translate es           # traduce la letra (vale con --api y con --demo)
lyrics deepl <clave>            # guarda la clave de DeepL
lyrics login <client_id> <secret> [puerto]
lyrics stats [corto|mediano|largo]
```

`--api` no vale junto con `--demo`. `--translate` sí vale con cualquiera de los dos.

### Reproductor local

Si hay varios reproductores, manda el que está en reproducción. A igualdad de estado, gana el que dio la señal más reciente y, si ninguno avisó, uno de escritorio antes que un navegador. En pausa se queda el que ya estaba activo, para que la pantalla no pase a «Nada sonando». Parado o cerrado, sí. Sin sesión D-Bus dice «No hay sesión D-Bus». Al abrir, el centro dice «Buscando reproductor».

Se ignoran `playerctld` y los puentes de teléfono `kdeconnect` y `gsconnect`.

La posición de MPRIS se lee al cambiar de pista, al pausar, al buscar y como mucho cada 5 segundos. Entre medias avanza el reloj local. Si el reproductor publica `Rate`, ese reloj lo respeta: a velocidad 2 la letra avanza el doble.

### YouTube

En Firefox, Chromium, Chrome, Brave, Vivaldi y Opera, o si el título termina en YouTube o YouTube Music, el canal no se usa como artista. Si el título trae ` - ` (también raya y raya larga), la izquierda es el artista y la derecha la canción. Un guion pegado no parte: `Blink-182` se queda entero.

Se quitan los bloques del final, como `(Videoclip oficial)` o un programa de televisión anidado. Se conservan si nombran una versión: remix, acústico, instrumental, cover, live o en vivo. También se quitan prefijos entre corchetes, comillas envolventes y un `ft.` o `feat.` final. `SodaStereoVEVO`, si el título no trae separador, queda en `Soda Stereo`.

Un reproductor de escritorio no entra en esta limpieza.

Si no hay letra, el centro muestra el título ya limpio y debajo la app, por ejemplo `YouTube | Chrome`. La caché de ese video usa el id de YouTube (`watch`, `youtu.be`, Shorts, embed o live).

### Letras

Primero se pide a lrclib la canción con su duración, usando el artista principal. Si no está, se busca por artista y título y se elige la sincronizada cuya duración cae a 8 segundos o menos de la del video. Si todas quedan más lejos, se muestra el texto plano de la más cercana, sin tiempos. Si esa búsqueda vuelve vacía, se repite con el artista y el título en una sola consulta. Un instrumental más cercano que cualquier letra se muestra como instrumental. Sin duración conocida se toma la primera sincronizada.

Mientras llega la respuesta el centro dice «buscando letra». Una respuesta saturada (429 o 5xx) o un fallo de red se reintenta, de 5 segundos hasta un minuto, y en pantalla se lee «lrclib saturado, reintentando». «No encontrada» no se guarda: al cambiar de pista o al reiniciar se vuelve a pedir. Un error que no es transitorio se muestra y no se repite.

La caché vieja en JSON se convierte a `.lyr` al arrancar.

### Spotify

Hace falta para `--api`, `login` y `stats`. Crea una app en [developer.spotify.com](https://developer.spotify.com/dashboard) y registra esta Redirect URI:

```text
http://127.0.0.1:8888/callback
```

Luego:

```sh
lyrics login <client_id> <client_secret>
```

Abre el navegador, autoriza la app y guarda la sesión. El puerto por defecto es 8888; se puede pasar otro como tercer argumento, y la Redirect URI tiene que coincidir.

Permisos que pide: lectura de la reproducción actual, del estado del reproductor, del top de canciones y del perfil.

La sesión se guarda en `$XDG_STATE_HOME/lyrics/spotify-auth.json` (si no existe esa variable, `~/.local/state/lyrics/`), con permisos `0600`. Si ya hay una sesión de [sptlrx](https://github.com/raitonoberu/sptlrx), la reutiliza la primera vez.

Con `--api` el centro arranca en «Conectando con Spotify». La reproducción se consulta cada 2 segundos, y cada segundo en pausa, para volver pronto a la línea que suena. Entre medias avanza el reloj local. Sin canción dice «Nada sonando en Spotify».

`stats` muestra el nombre, el país, el plan y el top 10. Sin argumento usa el periodo mediano: últimas 4 semanas (`corto`), últimos 6 meses (`mediano`) o cerca de un año (`largo`).

### Traducción

La clave puede ir en `DEEPL_API_KEY` o guardarse con `lyrics deepl`. El archivo queda en el mismo directorio de estado, como `deepl-key`, con permisos `0600`. Una clave que termina en `:fx` usa la API gratuita; el resto, la de pago.

Idiomas: `es`, `es-latam`, `en`, `en-gb`, `pt`, `pt-pt`, `fr`, `fr-ca`, `de`, `de-ch`, `it`, `ja`, `ko`, `zh`, `zh-hant`, `ru`. También acepta nombres (`spanish`, `español`, `english`, …).

La letra original no se toca. La traducción se guarda aparte, con los mismos tiempos, y solo se vuelve a pedir si cambia el texto. Las líneas repetidas se envían una vez. Si DeepL detecta que una línea ya está en el idioma pedido, se deja igual. Un fallo de red o un límite de peticiones se reintenta, de 5 a 20 segundos, y el aviso sale en la última fila. Una clave rechazada o la cuota agotada se muestran y no se repiten.

Con `--translate` la vista arranca en la traducción. La tecla `t` vuelve al original y esa elección se conserva al cambiar de canción.

### Espectro

El monitor se lee en mono a 44,1 kHz. Una ventana de 2048 muestras, con ventana de Hann, reparte la energía en bandas logarítmicas de 50 Hz a 12 kHz. El número de barras depende del ancho de la terminal: cada una ocupa dos columnas y deja una de separación, y el grupo queda centrado.

Las barras suben con una constante de unos 25 ms y caen con gravedad. La sensibilidad se ajusta sola: si una banda se satura, baja; si hay señal y cabe más, sube. Si PipeWire deja de entregar muestras porque la salida se suspendió, a los 150 ms las barras quedan en silencio. Si se cae la conexión, por ejemplo al pasar a unos audífonos, se reintenta cada segundo contra el monitor por defecto.

### Pantalla

La línea que suena queda en el centro vertical, en cian y negrita. Las anteriores van en blanco tenue y las siguientes en un azul apagado. Las barras usan el tono `86, 95, 137`. Una línea sincronizada vacía, cuando es la que suena, se muestra como ♪. El texto se parte por palabras y, si una palabra no cabe, por caracteres.

En una letra sincronizada el desplazamiento solo se queda en pausa: al reanudar vuelve a la línea que suena. En texto plano se recorre desde arriba y el desplazamiento se mantiene aunque siga sonando. Al cambiar de letra el desplazamiento vuelve a cero.

Las barras en píxeles salen de una prueba al arrancar. Si la entrada y la salida son una terminal, se manda un píxel por memoria compartida y se espera hasta 200 ms. Una respuesta `OK` enciende la capa, detrás del texto. La imagen nueva se coloca antes de quitar la anterior, así no hay un cuadro vacío. Un error, el silencio o una salida que no es terminal dejan las barras en bloques de celda, de abajo arriba, con octavos en la punta. La respuesta se recuerda: al cambiar el tamaño no se vuelve a preguntar.

El dibujo va a unos 60 cuadros por segundo mientras algo cambia. Tras medio segundo quieto baja a 10. Texto e imagen salen en la misma actualización.

### Teclas

| Tecla | Acción |
| --- | --- |
| `q`, `Esc`, `Ctrl+C` | salir |
| flechas, `j`/`k`, rueda | recorrer la letra |
| RePág / AvPág | media pantalla |
| Inicio | volver a la línea que suena |
| espacio | pausa (solo en `--demo`; en vivo la pausa es la del reproductor) |
| `t` | original o traducción, si se pasó `--translate` |

## Dónde quedan los datos

| Qué | Dónde |
| --- | --- |
| Sesión de Spotify y clave de DeepL | `$XDG_STATE_HOME/lyrics` o `~/.local/state/lyrics` |
| Letras en caché (`.lyr`) | `$XDG_CACHE_HOME/lyrics` o `~/.cache/lyrics` |
| Traducciones | el mismo directorio de caché, en `translations/` |

El `.lyr` es binario: cabecera `LYR` versión 1, el cuerpo comprimido, las frases repetidas una sola vez y los tiempos como diferencias. Los `.json` de una caché anterior se convierten a ese formato al arrancar.

## Cómo está armado

Cada módulo es un hilo o una pieza del dibujo, y `main` solo los junta en el bucle de frames.

| Módulo | Rol |
| --- | --- |
| `mpris` | Reproductor local por MPRIS2 y limpieza de títulos de YouTube |
| `spotify` | OAuth, reproducción por la API web y estadísticas |
| `player` | Hilo del reproductor, posición local (con `Rate`) y reintentos de lrclib |
| `lyrics` | Búsqueda en lrclib, por duración, y lectura de la caché |
| `lyr` | Formato binario de la caché de letras |
| `translate` | Cola de traducción con DeepL |
| `audio` | Captura del monitor con libpulse |
| `spectrum` | FFT, bandas logarítmicas, sensibilidad y caída de las barras |
| `render` | Diferencias de celdas y salida ANSI |
| `scene` | Letra, tema de color y barras de celda |
| `kitty` | Prueba del protocolo gráfico y barras en píxeles detrás del texto |
