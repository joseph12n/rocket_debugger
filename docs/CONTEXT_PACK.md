# Context pack

El contrato entre el motor y el consumidor (agente o UI). Un robot, un documento, ~1-2,5k tokens.

## Formato

Markdown. No JSON: las claves se repetirian una vez por nodo y el documento saldria ~4x mas grande
sin que un modelo lo lea mejor. El JSON se reserva para la UI.

## Secciones

```
# <robot>  (snapshot N, M versiones)
> descripcion del robot
db: <etiqueta> | nodos N | vars V | prof D
!! direccionamiento por linea NO FIABLE: N lineas duplicadas
   <linea> x<n>, ...

## Variables
- <nombre> = <valor o (sin valor inicial)>

## Flujo
<linea> <comando> → <variable_escrita> [else]  <parametros>
  <anidado>
```

Cada seccion corresponde a una card de la UI. Las secciones pendientes:

| seccion | estado | issue |
|---|---|---|
| cabecera + colisiones | hecho | #5 |
| Variables | hecho | #5 |
| Flujo | hecho | #5 |
| Hallazgos | pendiente | #4 |
| Contrato implicito (subrobots) | pendiente | #3 |
| Errores (vivos / stale / obsoletos / modulo / ruido) | pendiente | #2 |

## Presupuesto

- Tope por robot: 2,5k tokens. Si se pasa, primero se recorta el flujo (los parametros largos ya se
  cortan a 100 caracteres) y nunca los hallazgos.
- Tope por tool MCP: ~3k tokens. Si una consulta devuelve mas, se pagina o se resume.

## Estabilidad

- El render es **determinista**: misma db + mismo snapshot => mismo texto. Hay un golden test
  (`fixtures/golden/simple.md`) que falla si cambia el formato; renovarlo es una decision consciente.
- No contiene rutas absolutas del usuario: la etiqueta de db es `padre\fichero.db`.
- Los parametros se normalizan a una linea (saltos -> `⏎`) y se cortan a 100 caracteres.

## Medicion

```bash
rocketdebug pack <robot> --stats
# crudo 9235 tok (34166 car.) | render 733 tok (2712 car.) | 12.6x | -92%
```

La estimacion de tokens es `caracteres / 3,7` (+-15%). Sirve para decidir presupuesto, no para
facturar.
