# Context pack

El contrato entre el motor y el consumidor (agente o UI). Un robot, un documento, ~1-3k tokens.

## Formato

Markdown. No JSON: las claves se repetirian una vez por nodo y el documento saldria ~4x mas grande
sin que un modelo lo lea mejor. El JSON se reserva para la UI.

## Secciones

```
# <robot>  (snapshot N, M versiones)
> descripcion del robot
db: <etiqueta> | nodos N | vars V | prof D
!! direccionamiento por linea NO FIABLE: N lineas duplicadas

## Hallazgos (5: 5 alta / 0 media / 0 baja)      ← accionables: defecto con linea
- [alta] L2.1 LINEA_COLISION — ... → ...

## Higiene (3: 0 alta / 1 media / 2 baja)        ← higiene: afecta al robot entero
- [media] L- ANIDAMIENTO — ... → ...

## Contrato implicito (subrobots)
- llama a X en L2.3 → provee: ...
  !! el hijo espera y nadie provee: ...

## Errores (vivos 0 · payload cambiado 0 · historicos 5 · modulo 0 · ruido 1)
- historicos de versiones anteriores: log_message(x4), raise_exception(x4) …
- ruido del motor (ignorado): 1 firma

## Variables (18)
- ruta_salida = (ruta)
- destinatario = (valor cifrado)

## Flujo
2 setVar → contador  0
```

| Seccion | Estado | Issue |
|---|---|---|
| cabecera + colisiones de linea | hecho | #5 |
| Hallazgos (accionables) | hecho | #4 |
| Higiene | hecho | #4 |
| Contrato implicito | hecho | #3 |
| Errores con atribucion temporal | hecho | #2 |
| Variables redactadas | hecho | #8, #11 |
| Flujo | hecho | #5 |

### Por que dos cubos de hallazgos

Un defecto con linea (`LINEA_COLISION`, `RUTA_ABSOLUTA`) es atendible hoy. Una carencia del robot
entero (`SIN_TRY`, `IF_SIN_ELSE`, `ANIDAMIENTO`, `VAR_NO_USADA`) es cierta pero no urguide; mezcladas,
la segunda diluia a la primera. Medido sobre 3 dbs reales (39 instancias, 70 accionables + 66 de
higiene): **2,7 accionables por robot afectado y 0 ocultos por el tope**.

## Redaccion (que NO sale del pack)

| Valor | Se convierte en |
|---|---|
| Cifrado de Rocketbot (`gAAAAA…`) | `(valor cifrado)` |
| Nombre sensible (`pass`, `token`, `serial`, …) | `(oculto)` |
| Ruta absoluta en el panel de variables | `(ruta)` |
| Tramo de usuario en cualquier ruta (`C:/Users/x`, `/home/x`) | `%USERPROFILE%` / `~` |
| Correo electronico (`juan@dominio.com`) | `j***@dominio.com` |

Un pack esta pensado para compartirse y para pasarselo a un agente: el material de credenciales y el
arbol de directorios del desarrollador no viajan.

## Presupuesto

- Tope por robot: ~2,5k tokens. Los parametros largos se cortan a 100 caracteres y los mensajes de
  hallazgo a 140.
- Tope por seccion: 5 hallazgos accionables y 5 de higiene; si se recorta, la cabecera lo dice
  (`5 de 23`) y una linea final lo cierra (`… y 18 mas`). Nunca se oculta el total en silencio.
- Medido: `ML_06_Legacy` (36 nodos) 1.128 tokens (8,2x sobre el JSON crudo); `reporteMercadolibre`
  (17 nodos, 86 vars) 1.812 tokens (5,6x).

## Estabilidad y hermeticidad

- El render es **determinista**: misma db + mismo snapshot + mismos logs => mismo texto. Hay un golden
  (`fixtures/golden/simple.md`) que falla si cambia el formato; renovarlo es una decision consciente.
- **El motor no lee logs por su cuenta.** `build_from_conn` no toca el sistema de ficheros; es el CLI
  quien pasa el directorio de logs con `build_from_conn_with_logs`. Sin eso, un test cuyo robot se
  llamara igual que uno real cambiaria de resultado segun la maquina.
- No contiene el nombre de usuario en ningun sitio: la etiqueta de db es relativa, las variables se
  redactan y en los **parametros de los comandos** el tramo del usuario se enmascara conservando la
  cola util (`C:/Users/josep/OneDrive/x` → `%USERPROFILE%/OneDrive/x`). La ruta sigue siendo legible
  porque forma parte de entender el flujo; el nombre del desarrollador no.
- El enmascarado vive en **un solo punto** (`render::field`), por donde pasan parametros, mensajes de
  hallazgo y mensajes de error. Un recorte no puede publicar medio nombre.

## Medicion

```bash
rocketdebug pack <robot> --stats
# crudo 9235 tok (34166 car.) | render 1128 tok (4172 car.) | 8.2x | -88%
```

La estimacion de tokens es `caracteres / 3,7` (+-15%). Sirve para decidir presupuesto, no para facturar.
