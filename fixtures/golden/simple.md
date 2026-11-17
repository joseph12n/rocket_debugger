# simple  (snapshot 1, 1 versiones)
> Fixture minimo del motor
db: test\robot.db | nodos 3 | vars 1 | prof 1

## Variables (1)
- saludo = (sin valor inicial)

## Flujo
1 log_message  INICIO simple
2 setVar → saludo  "hola"
3 log_message  {saludo}
