# Fase 0 — hallazgos medidos

Fecha: 2026-11-17 · Entorno: Rocketbot 2026.04.07 · 3 dbs · 28 robots
Prototipo de referencia: `C:\rocketbot_bots\labs\context_pack` (fuera del repo; dependia del MCP de
Rocketbot y de rutas locales). El producto es este crate.

## Medicion

| Metrica | Valor |
|---|---|
| Robots unicos | 28 (50 filas antes de deduplicar) |
| Llamadas padre→hijo | 20 |
| Contexto crudo (pretty) | 102.052 tokens |
| Contexto renderizado | 16.326 tokens |
| Ratio global | 6,3x |
| Rango por robot | 1,8x (3 nodos) … 13,9x (5 nodos) |
| Robot mas grande (32 nodos) | 18.276 → 1.827 tokens (10,0x) |
| Suite BBRE2 completa (10 robots) | 51.752 → 8.330 tokens (6,2x) |

El ratio **crece con el tamano**: 1,8x en robots de 3 nodos, 10x en el de 32. La compresion aparece
donde hace falta. Robots pequenos apenas se comprimen porque tienen poco ruido que quitar.

Nota de comparabilidad: el render de la fase 0 incluia 4 secciones (hallazgos, contrato, errores, flujo).
El render actual del crate incluye 2, asi que sus ratios (12,6x) **no son comparables** hasta que
lleguen las secciones de #2, #3 y #4.

## Defecto critico: lineas duplicadas

**34 lineas duplicadas en 13 de 28 robots.** En `ML_06_Legacy` el `try` y el `catch` comparten
`2.1`, `2.2`, `2.3`:

```
2 trycatch
  2.1 log_message   "Paso Legacy..."        ← rama then
  2.2 setVar        resumen_txt
  2.3 setVar        escrito
  ...
  2.1 log_message   "Error en el paso..."   ← rama else (MISMA linea)
  2.2 setVar        ok_legacy = False
  2.3 setVar        res_close
```

Consecuencia: `line` no es direccion unica. Toda herramienta que direccione por linea puede editar
el nodo equivocado. Se direcciona por `id`.

## Defectos reales confirmados

1. **`savexlsx` recibe el JSON como ruta.** `OSError: [Errno 22] Invalid argument: '{"path":"C:\\...'`
2. **`{fallas}` sin resolver**, visible en el log como `Validacion fallida: {fallas}`.
3. **7 entradas no declaradas** en `ML_06_Legacy` (`busqueda`, `cantidad`, `rango_fact`...): las hereda
   del padre y el contrato no esta escrito en ningun sitio.
4. **Comparacion booleana como texto**: `"{ok_legacy}" != "True"` sobre una variable que el robot
   escribe como booleano.
5. **Sin Try/Catch** en varios robots; **catch vacio** en otros.

## La atribucion es temporal (caso `contador`)

| snapshot | created_at | bytes | nodos |
|---|---|---|---|
| v25 | 12:47:10 | 22.768 | 31 |
| v26 | 12:50:24 | 24.116 | 32 |
| v27 | 12:52:29 | 24.500 | 32 |
| v28 | 12:55:20 | 30.736 | 36 |
| v29 | 12:59:37 | 31.820 | 36 |

Log: `NameError: name 'contador' is not defined` en L2.10, entre 12:49:22 y 12:50:28.

- La version viva era **v25/v26**, no la actual v29.
- `contador` **si esta inicializado en L2.6 en las 7 versiones**, incluida v25: la hipotesis
  "falta inicializar" es **falsa**.
- El error desaparece tras v27 => **el fix esta en el diff v26→v27**.

Lo que se aprende:

1. Atribuir un error al snapshot vivo en su timestamp, no al ultimo.
2. El historial de versiones no es solo el undo: es el **indice de atribucion**.
3. El diff entre snapshots es el **fix minado**, y es la base del registro de problemas.
4. El motor no debe afirmar causas. Aqui localiza *donde* y *cuando* con precision, y no alcanza
   para el *porque*: eso lo cierra el agente leyendo el diff de 2 snapshots (~2,6k tokens).

## Calibracion de reglas

Sin grafo de llamadas, las reglas intra-robot tienen muy baja precision:

| Robot | Hallazgos | Reales | Precision |
|---|---|---|---|
| ML_06_Legacy | 16 | 6 | 37% |
| ML_04_MySQL | 17 | 4 | 24% |
| ML_07_Gmail | 26 | 7 | 27% |
| Orquestador BBRE2 | 90 | 3 | **3%** |

Causa principal: las 86 variables del orquestador parecen muertas, pero son la **superficie de API**
de sus 8 subrobots. Cargando el grafo y el panel del padre, `ML_06_Legacy` baja a 7 hallazgos, todos
reales.

**Conclusion: el grafo de llamadas no es una feature, es un requisito de correccion.**

## Limites de esta fase

- 28 robots y 8 dias de logs: muestra pequena, sin robots de cliente grandes.
- La proyeccion a 300 comandos es lineal, no medida.
- Las reglas encontraron defectos pero **no se arreglo ninguno**: falta cerrar el ciclo
  detectar → agente → diff → verificar.

## Verificación en el motor (fase 1)

Lo anterior se midió con el prototipo en Python. Esto es lo que mide el motor en Rust sobre los mismos
datos, ya con todas las secciones cableadas:

| Métrica | Valor |
|---|---|
| Robots inspeccionados (3 dbs, con copias) | 39 |
| Hallazgos accionables | 70 (58 colisiones de línea + 12 rutas absolutas) |
| Hallazgos de higiene | 66 (28 `IF_SIN_ELSE`, 22 `SIN_TRY`, 14 `VAR_NO_USADA`, 2 `ANIDAMIENTO`) |
| Accionables por robot afectado | 2,7 |
| Ocultos por el tope | 0 |
| `ML_06_Legacy` | 1.128 tokens (8,2x sobre el JSON crudo) |
| `reporteMercadolibre` (86 vars) | 1.812 tokens (5,6x) |

### Corrección al informe de fase 0

El informe original atribuía a `ML_06_Legacy` errores «vivos». Con la atribución temporal cableada, el
motor dice **0 vivos / 5 históricos / 1 de ruido**, y es lo correcto: la v29 se guardó a las 12:59:37 y
el último error registrado del robot es de las 12:55:30, así que **todo su log precede a la versión
actual**. El `NameError` de `contador` aparece como histórico de la v25/v26, que es exactamente lo que
el caso de estudio pedía demostrar.

Los números de fase 0 (28 robots, 102.052 tokens crudos, 6,3x) se mantienen como referencia del
prototipo; los de esta tabla son los del motor y no son directamente comparables, porque el render
actual tiene seis secciones y el de fase 0 tenía cuatro.
